//! Process supervisor: boots the official dsh runtime, runs `dsh web`,
//! parses the authenticated URL from its stdout, watches the child, and
//! applies npm updates on demand. The worker stays responsive to commands
//! while a session thread owns the blocking I/O.

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use tao::event_loop::EventLoopProxy;

use crate::runtime::Runtime;

/// Commands the main thread can send to the worker.
pub enum WorkerCmd {
    /// Boot the runtime (install if needed) and start a dsh session.
    Start,
    /// Kill the current session and start a fresh one.
    Restart,
    /// Kill the current session and stay idle.
    #[allow(dead_code)]
    Stop,
    /// Check npm for a newer dsh; install it when found.
    CheckUpdate,
    /// Kill the session and shut the worker down.
    Quit,
}

/// Messages the worker (or the menu/tray bridge threads) push to the UI loop.
#[derive(Debug, Clone)]
pub enum UiMsg {
    /// One-line status for the loading page and the log.
    Status(String),
    /// `dsh web` is listening; `url` is the authenticated local URL.
    Ready { session: u64, url: String },
    /// The dsh child exited.
    Exited { session: u64, code: Option<i32>, uptime_secs: u64 },
    /// Human-readable update-check result.
    UpdateInfo(String),
    /// A menu item was activated (muda item id).
    Menu(String),
    /// Left click on the tray icon.
    TrayLeftClick,
    /// The worker finished shutting down; safe to exit.
    Bye,
}

pub struct Worker {
    cmd_tx: Sender<WorkerCmd>,
}

impl Worker {
    pub fn start(rt: Runtime, proxy: EventLoopProxy<UiMsg>) -> Worker {
        let (tx, rx) = std::sync::mpsc::channel();
        thread::spawn(move || worker_loop(rt, proxy, rx));
        Worker { cmd_tx: tx }
    }

    pub fn send(&self, cmd: WorkerCmd) {
        let _ = self.cmd_tx.send(cmd);
    }
}

struct WorkerInner {
    rt: Runtime,
    proxy: EventLoopProxy<UiMsg>,
    /// Session generation counter; sessions check this before acting so a
    /// stale boot can never double-spawn dsh.
    session: u64,
    expected: Arc<AtomicU64>,
    handle: Option<JoinHandle<()>>,
    slot: Arc<Mutex<Option<Child>>>,
}

fn worker_loop(rt: Runtime, proxy: EventLoopProxy<UiMsg>, rx: Receiver<WorkerCmd>) {
    let mut inner = WorkerInner {
        rt,
        proxy,
        session: 0,
        expected: Arc::new(AtomicU64::new(0)),
        handle: None,
        slot: Arc::new(Mutex::new(None)),
    };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            WorkerCmd::Start => {
                inner
                    .proxy
                    .send_event(UiMsg::Status("正在启动 DeepSeek Harness…".into()))
                    .ok();
                inner.start_session();
                // Non-blocking startup update check (informs only).
                check_update(&inner);
            }
            WorkerCmd::Restart => {
                inner
                    .proxy
                    .send_event(UiMsg::Status("正在启动 DeepSeek Harness…".into()))
                    .ok();
                inner.start_session();
            }
            WorkerCmd::Stop => inner.stop_session(),
            WorkerCmd::CheckUpdate => check_update(&inner),
            WorkerCmd::Quit => {
                inner.stop_session();
                inner
                    .proxy
                    .send_event(UiMsg::Status("正在退出…".into()))
                    .ok();
                inner.rt.log("worker: quit, bye");
                inner.proxy.send_event(UiMsg::Bye).ok();
                return;
            }
        }
    }
}

impl WorkerInner {
    /// Reap an orphaned dsh from a previous force-quit, then boot a session.
    fn start_session(&mut self) {
        self.reap_stale_pid();
        self.session += 1;
        self.expected.store(self.session, Ordering::SeqCst);
        // Stop anything still running, then wait for its thread so installs
        // can never run concurrently into the same runtime prefix.
        if let Some(prev) = self.handle.take() {
            self.kill_child();
            let _ = prev.join();
        }
        self.slot = Arc::new(Mutex::new(None));
        let rt = self.rt.clone();
        let proxy = self.proxy.clone();
        let session = self.session;
        let expected = self.expected.clone();
        let slot = self.slot.clone();
        self.handle = Some(thread::spawn(move || {
            run_session(rt, proxy, session, expected, slot);
        }));
    }

    fn stop_session(&mut self) {
        self.expected.store(0, Ordering::SeqCst);
        if let Some(prev) = self.handle.take() {
            self.kill_child();
            let _ = prev.join();
        }
    }

    fn kill_child(&self) {
        if let Some(child) = self.slot.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }

    /// Kill a leftover `dsh web` from a previous run (e.g. force-quit), if any.
    fn reap_stale_pid(&self) {
        let pid_file = self.rt.pid_file();
        let Ok(content) = std::fs::read_to_string(&pid_file) else {
            return;
        };
        let Ok(pid) = content.trim().parse::<u32>() else {
            return;
        };
        if pid == std::process::id() {
            return;
        }
        // kill -0: still alive?
        let alive = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if alive {
            self.rt.log(&format!("reaping stale dsh pid {pid}"));
            let _ = Command::new("kill")
                .args(["-9", &pid.to_string()])
                .status();
        }
        let _ = std::fs::remove_file(&pid_file);
    }
}

/// One dsh session: ensure the runtime, spawn `dsh web --no-open --port 0`,
/// stream its stdout (parsing the `dsh web: <url>` line), and report exit.
fn run_session(
    rt: Runtime,
    proxy: EventLoopProxy<UiMsg>,
    session: u64,
    expected: Arc<AtomicU64>,
    slot: Arc<Mutex<Option<Child>>>,
) {
    let started = Instant::now();
    let still_current = || expected.load(Ordering::SeqCst) == session;

    // 1. Runtime bootstrap (Node + official dsh package).
    let node = match rt.ensure_node(|s| {
        let _ = proxy.send_event(UiMsg::Status(s));
    }) {
        Ok(n) => n,
        Err(e) => {
            rt.log(&format!("node bootstrap failed: {e}"));
            let _ = proxy.send_event(UiMsg::Status(format!("Node.js 不可用：{e}")));
            report_exit(&proxy, session, None, started.elapsed().as_secs());
            return;
        }
    };
    let (node_bin, bundled) = node;
    if !still_current() {
        return;
    }

    if rt.installed_version().is_none() {
        let _ = proxy.send_event(UiMsg::Status(
            "首次运行：正在安装官方 @deepseek-ai/dsh（约需几十秒）…".into(),
        ));
        if let Err(e) = rt.install_dsh(&node_bin, bundled, "latest") {
            rt.log(&format!("install failed: {e}"));
            let _ = proxy.send_event(UiMsg::Status(format!("安装失败：{e}")));
            report_exit(&proxy, session, None, started.elapsed().as_secs());
            return;
        }
        let v = rt.installed_version().unwrap_or_default();
        rt.log(&format!("installed dsh {v}"));
    }
    if !still_current() {
        return;
    }

    // 2. Spawn `dsh web`.
    let dsh_bin = rt.dsh_bin();
    let mut cmd = Command::new(&node_bin);
    cmd.arg(&dsh_bin)
        .arg("web")
        .arg("--no-open")
        .arg("--port")
        .arg("0");
    cmd.current_dir(dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")));
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            rt.log(&format!("spawn dsh failed: {e}"));
            let _ = proxy.send_event(UiMsg::Status(format!("启动失败：{e}")));
            report_exit(&proxy, session, None, started.elapsed().as_secs());
            return;
        }
    };
    rt.log(&format!("dsh web started pid={}", child.id()));
    if let Ok(mut f) = std::fs::File::create(rt.pid_file()) {
        use std::io::Write;
        let _ = writeln!(f, "{}", child.id());
    }
    *slot.lock().unwrap() = Some(child);

    // 3. Stream stdout/stderr on helper threads.
    {
        let mut guard = slot.lock().unwrap();
        let child = guard.as_mut().unwrap();
        if let Some(stdout) = child.stdout.take() {
            let rt2 = rt.clone();
            let p2 = proxy.clone();
            let s2 = session;
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    if line.starts_with("dsh web: ") {
                        let rest = &line["dsh web: ".len()..];
                        let url = rest
                            .split(" (LAN:")
                            .next()
                            .unwrap_or(rest)
                            .trim()
                            .to_string();
                        rt2.log(&format!("ready line: {line}"));
                        let _ = p2.send_event(UiMsg::Ready { session: s2, url });
                    } else {
                        rt2.log(&format!("[dsh stdout] {line}"));
                    }
                }
            });
        }
        if let Some(stderr) = child.stderr.take() {
            let rt2 = rt.clone();
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    rt2.log(&format!("[dsh stderr] {line}"));
                }
            });
        }
    }

    // 4. Watch the child (poll so external kills are honoured instantly).
    let (code, uptime) = loop {
        if !still_current() {
            // Invalidated: make sure our child is dead, then leave quietly.
            let mut guard = slot.lock().unwrap();
            if let Some(c) = guard.as_mut() {
                let _ = c.kill();
                let _ = c.wait();
            }
            *guard = None;
            let _ = std::fs::remove_file(rt.pid_file());
            return;
        }
        let status = slot
            .lock()
            .unwrap()
            .as_mut()
            .and_then(|c| c.try_wait().ok().flatten());
        if let Some(status) = status {
            *slot.lock().unwrap() = None;
            break (status.code(), started.elapsed().as_secs());
        }
        thread::sleep(Duration::from_millis(200));
    };
    let _ = std::fs::remove_file(rt.pid_file());
    rt.log(&format!("dsh exited code={code:?} uptime={uptime}s"));
    report_exit(&proxy, session, code, uptime);
}

fn report_exit(proxy: &EventLoopProxy<UiMsg>, session: u64, code: Option<i32>, uptime_secs: u64) {
    let _ = proxy.send_event(UiMsg::Exited {
        session,
        code,
        uptime_secs,
    });
}

/// Registry check + install, on a short-lived thread (network can be slow).
fn check_update(inner: &WorkerInner) {
    let rt = inner.rt.clone();
    let proxy = inner.proxy.clone();
    thread::spawn(move || {
        let _ = proxy.send_event(UiMsg::Status("正在检查更新…".into()));
        let node = match rt.ensure_node(|s| {
            let _ = proxy.send_event(UiMsg::Status(s));
        }) {
            Ok(n) => n,
            Err(e) => {
                let _ = proxy.send_event(UiMsg::UpdateInfo(format!("检查更新失败：{e}")));
                return;
            }
        };
        let (node_bin, bundled) = node;
        let installed = rt.installed_version();
        // First boot just installed the latest version; nothing to check.
        let Some(installed) = installed else {
            return;
        };
        let latest = rt.latest_version(&node_bin, bundled);
        let text = match latest {
            None => "检查更新失败：无法访问 npm registry".to_string(),
            Some(v) if v == installed => format!("已是最新版本 {v}"),
            Some(v) => {
                let _ = proxy.send_event(UiMsg::Status(format!(
                    "发现新版本 {v}（当前 {installed}），正在下载…"
                )));
                match rt.install_dsh(&node_bin, bundled, &v) {
                    Ok(()) => {
                        rt.log(&format!("update installed {v} (was {installed})"));
                        format!("已更新到 {v}（原 {installed}）。请从托盘菜单“重启 Harness”使新版本生效。")
                    }
                    Err(e) => format!("更新到 {v} 失败：{e}"),
                }
            }
        };
        rt.log(&format!("update check: {text}"));
        let _ = proxy.send_event(UiMsg::UpdateInfo(text));
    });
}
