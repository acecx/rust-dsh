//! Runtime bootstrap: Node discovery/download, official `@deepseek-ai/dsh`
//! installation, and npm-registry update checks.
//!
//! Everything lives under `<home>` (default: `~/Library/Application Support/DSH Client`):
//!
//! ```text
//! <home>/
//!   runtime/
//!     node/                    (bundled Node.js, downloaded on demand)
//!     node_modules/@deepseek-ai/dsh/
//!     package.json             (pins the exact dsh version)
//!     dsh-web.pid
//!   cache/                     (npm cache, keeps the app hermetic)
//!   logs/dsh-client.log
//! ```
//!
//! The dsh package itself is the official, unmodified npm artifact; the app
//! never patches it, so staying current is just `npm install <pkg>@latest`.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

pub const DSH_PKG: &str = "@deepseek-ai/dsh";

/// Serializes node downloads and dsh installs so concurrent threads (session
/// bootstrap vs. update check) can never race on the runtime directory.
fn runtime_ops_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Extract the value of a JSON string field with a tiny hand-rolled scanner.
fn extract_json_field(s: &str, key: &str) -> Option<String> {
    let pat = format!("\"{key}\"");
    let rest = s.get(s.find(&pat)? + pat.len()..)?;
    let rest = rest.get(rest.find(':')? + 1..)?;
    let start = rest.find('"')? + 1;
    let end = rest.get(start..)?.find('"')? + start;
    Some(rest[start..end].to_string())
}

/// Prefer the latest **LTS** release: scan the entries and take the first
/// version whose `"lts"` field is not `false`.
fn first_lts_version_in_node_index(s: &str) -> Option<String> {
    let marker = "\"version\":\"v";
    let mut rest = s;
    while let Some(pos) = rest.find(marker) {
        let after = &rest[pos + marker.len()..];
        let end = after.find('"')?;
        let version = after[..end].to_string();
        // The "lts" field of this entry lives shortly after the version.
        let window = &after[end..after.len().min(end + 1500)];
        if window.contains("\"lts\":false") || window.contains("\"lts\": false") {
            rest = after;
            continue;
        }
        return Some(version);
    }
    None
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[derive(Clone)]
pub struct Runtime {
    home: PathBuf,
}

impl Runtime {
    /// Resolve the app home: `$DSHCLIENT_HOME` override, else the macOS
    /// `~/Library/Application Support/DSH Client` convention.
    pub fn new() -> Self {
        let home = std::env::var_os("DSHCLIENT_HOME")
            .map(PathBuf::from)
            .or_else(|| dirs::home_dir().map(|h| h.join("Library/Application Support/DSH Client")))
            .unwrap_or_else(|| PathBuf::from("."));
        let _ = fs::create_dir_all(home.join("logs"));
        Self { home }
    }

    pub fn runtime_dir(&self) -> PathBuf {
        self.home.join("runtime")
    }

    pub fn dsh_bin(&self) -> PathBuf {
        self.runtime_dir()
            .join("node_modules")
            .join(DSH_PKG)
            .join("lib")
            .join("bin.js")
    }

    pub fn pid_file(&self) -> PathBuf {
        self.runtime_dir().join("dsh-web.pid")
    }

    /// Append a timestamped line to the app log and mirror it to stderr.
    pub fn log(&self, msg: &str) {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let line = format!("[{ts}] {msg}\n");
        eprint!("{line}");
        let path = self.home.join("logs").join("dsh-client.log");
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }

    /// Version of the dsh package currently installed in the runtime.
    pub fn installed_version(&self) -> Option<String> {
        let pkg = self
            .runtime_dir()
            .join("node_modules")
            .join(DSH_PKG)
            .join("package.json");
        let s = fs::read_to_string(pkg).ok()?;
        extract_json_field(&s, "version")
    }

    /// Resolve a working Node binary: `$DSHCLIENT_NODE`, the bundled runtime
    /// copy, the system PATH, or a fresh download from nodejs.org.
    /// Returns `(node_binary, npm_comes_from_bundle)`.
    pub fn ensure_node<F: FnMut(String)>(&self, mut status: F) -> Result<(PathBuf, bool), String> {
        if let Some(p) = std::env::var_os("DSHCLIENT_NODE").map(PathBuf::from) {
            if p.is_file() {
                return Ok((p, false));
            }
        }
        let bundled = self.runtime_dir().join("node").join("bin").join("node");
        if bundled.is_file() {
            return Ok((bundled, true));
        }
        if let Some(p) = which("node") {
            return Ok((p, false));
        }
        status("未找到 Node.js，正在下载官方 Node 发行版…".into());
        self.download_node()?;
        if bundled.is_file() {
            Ok((bundled, true))
        } else {
            Err("Node 下载完成但二进制缺失".into())
        }
    }

    fn download_node(&self) -> Result<(), String> {
        let _guard = runtime_ops_lock().lock().unwrap();
        // Another thread may have finished while we waited.
        if self.runtime_dir().join("node").join("bin").join("node").is_file() {
            return Ok(());
        }
        let arch = match std::env::consts::ARCH {
            "aarch64" => "arm64",
            "x86_64" => "x64",
            a => return Err(format!("不支持的 CPU 架构: {a}")),
        };
        let index = self.http_get("https://nodejs.org/dist/index.json")?;
        let ver = first_lts_version_in_node_index(&index).ok_or("无法解析 nodejs.org 版本列表")?;
        self.log(&format!("download node v{ver} darwin-{arch}"));
        let rt = self.runtime_dir();
        fs::create_dir_all(&rt).map_err(|e| e.to_string())?;
        let tgz = rt.join(format!("node-v{ver}-darwin-{arch}.tar.gz"));
        let url = format!("https://nodejs.org/dist/v{ver}/node-v{ver}-darwin-{arch}.tar.gz");
        self.http_download(&url, &tgz)?;
        let st = Command::new("tar")
            .arg("-xzf")
            .arg(&tgz)
            .arg("-C")
            .arg(&rt)
            .status()
            .map_err(|e| e.to_string())?;
        if !st.success() {
            return Err("解压 Node 失败".into());
        }
        let dir = rt.join(format!("node-v{ver}-darwin-{arch}"));
        fs::rename(&dir, rt.join("node")).map_err(|e| e.to_string())?;
        let _ = fs::remove_file(&tgz);
        Ok(())
    }

    /// Build the base npm command with a hermetic environment. The bundled
    /// node's bin dir is prepended to PATH so dependency lifecycle scripts
    /// (which spawn bare `node`) resolve correctly.
    fn npm_command(&self, node: &Path, bundled: bool) -> Result<Command, String> {
        let mut cmd = if bundled {
            let mut c = Command::new(node);
            c.arg(self.runtime_dir().join("node/lib/node_modules/npm/bin/npm-cli.js"));
            c
        } else {
            let npm = which("npm").ok_or("PATH 中找不到 npm")?;
            Command::new(npm)
        };
        if let Some(bin_dir) = node.parent() {
            let current = std::env::var_os("PATH").unwrap_or_default();
            let mut paths = vec![bin_dir.to_path_buf()];
            paths.extend(std::env::split_paths(&current));
            if let Some(joined) = std::env::join_paths(paths).ok() {
                cmd.env("PATH", joined);
            }
        }
        cmd.env("npm_config_loglevel", "error")
            .env("npm_config_update_notifier", "false")
            .env("npm_config_audit", "false")
            .env("npm_config_fund", "false")
            .env("npm_config_cache", self.home.join("cache"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(cmd)
    }

    /// Run npm. When Node is bundled we drive npm's JS entry directly so we
    /// never depend on a system `npm` being on PATH.
    pub fn npm(&self, node: &Path, bundled: bool, args: &[String]) -> Result<Output, String> {
        let mut cmd = self.npm_command(node, bundled)?;
        cmd.args(args);
        cmd.output().map_err(|e| format!("npm 启动失败: {e}"))
    }

    /// Install the official dsh package at the given npm dist-tag/version
    /// (typically `latest`), pinned exactly via `--save-exact`.
    pub fn install_dsh(
        &self,
        node: &Path,
        bundled: bool,
        version: &str,
    ) -> Result<(), String> {
        let _guard = runtime_ops_lock().lock().unwrap();
        let rt = self.runtime_dir();
        fs::create_dir_all(&rt).map_err(|e| e.to_string())?;
        let pkg = rt.join("package.json");
        if !pkg.exists() {
            fs::write(&pkg, "{\"name\":\"dsh-runtime\",\"private\":true}")
                .map_err(|e| e.to_string())?;
        }
        let rt_s = rt.to_string_lossy().to_string();
        let spec = format!("{DSH_PKG}@{version}");
        let args = vec![
            "install".to_string(),
            "--prefix".to_string(),
            rt_s,
            "--save-exact".to_string(),
            "--no-audit".to_string(),
            "--no-fund".to_string(),
            spec,
        ];
        self.log(&format!("npm install {DSH_PKG}@{version}"));
        let out = self.npm(node, bundled, &args)?;
        if !out.status.success() {
            let tail = String::from_utf8_lossy(&out.stderr);
            let tail: String = tail.lines().rev().take(8).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" | ");
            return Err(format!("npm install 失败: {tail}"));
        }
        Ok(())
    }

    /// Query the npm registry for the latest published dsh version.
    pub fn latest_version(&self, node: &Path, bundled: bool) -> Option<String> {
        let args = vec![
            "view".to_string(),
            DSH_PKG.to_string(),
            "version".to_string(),
        ];
        match self.npm_with_timeout(node, bundled, &args, Duration::from_secs(25)) {
            Ok(out) if out.status.success() => {
                let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if v.is_empty() {
                    None
                } else {
                    Some(v)
                }
            }
            _ => None,
        }
    }

    fn npm_with_timeout(
        &self,
        node: &Path,
        bundled: bool,
        args: &[String],
        timeout: Duration,
    ) -> Result<Output, String> {
        let mut cmd = self.npm_command(node, bundled)?;
        cmd.args(args);
        let mut child = cmd.spawn().map_err(|e| e.to_string())?;
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    if let Some(mut s) = child.stdout.take() {
                        let _ = s.read_to_end(&mut stdout);
                    }
                    if let Some(mut s) = child.stderr.take() {
                        let _ = s.read_to_end(&mut stderr);
                    }
                    return Ok(Output {
                        status,
                        stdout,
                        stderr,
                    });
                }
                Ok(None) => {
                    if Instant::now() > deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err("npm 超时".into());
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(e) => return Err(e.to_string()),
            }
        }
    }

    fn http_get(&self, url: &str) -> Result<String, String> {
        let out = Command::new("curl")
            .args(["-fsSL", "--max-time", "60", url])
            .output()
            .map_err(|e| format!("curl 失败: {e}"))?;
        if !out.status.success() {
            return Err(format!("请求 {url} 失败"));
        }
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }

    fn http_download(&self, url: &str, dest: &Path) -> Result<(), String> {
        let st = Command::new("curl")
            .args(["-fL", "--retry", "2", "--max-time", "600", "-o"])
            .arg(dest)
            .arg(url)
            .status()
            .map_err(|e| format!("curl 失败: {e}"))?;
        if !st.success() {
            return Err(format!("下载失败: {url}"));
        }
        Ok(())
    }
}
