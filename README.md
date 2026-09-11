# DSH Client — DeepSeek Harness 的 macOS 桌面客户端

一个用 **Rust** 写的 macOS 原生壳（tao + wry/WKWebView + tray-icon），包装**官方未修改**的 `@deepseek-ai/dsh` npm 包。它启动官方的 `dsh web`，把本地 Web GUI 装进原生窗口，并提供菜单栏托盘。

## 架构

```
┌───────────────────────── DSH.app（Rust 壳）─────────────────────────┐
│  main.rs   原生窗口 + WKWebView + 应用菜单 + 托盘（tao/wry/tray-icon）│
│  worker.rs 进程监督：拉起 dsh web --no-open --port 0，解析认证 URL   │
│  runtime.rs 运行时引导：Node 检测/下载、npm 安装 dsh、版本检查/更新   │
└───────────────┬─────────────────────────────────────────────────────┘
                │ 启动（仅通过稳定公开接口：CLI 参数 + 打印的 URL）
                ▼
   node_modules/@deepseek-ai/dsh（官方 npm 包，不做任何改动）
                │
                ▼
   dsh web → http://127.0.0.1:<port>/?token=…  ← WKWebView 加载这个 URL
```

- **配置共享**：dsh 仍使用 `~/.dsh`（与官方 CLI 完全一致），壳不另起炉灶。
- **更新机制**：应用启动时检查 npm registry 最新版；托盘菜单「检查更新…」可随时安装新版本，安装后「重启 Harness」即生效。壳本身从不 patch dsh 源码，因此官方每发一版，只需要 `npm install @deepseek-ai/dsh@latest`。
- **运行时目录**：`~/Library/Application Support/DSH Client/`（runtime/、cache/、logs/）。没有系统 Node 时自动下载官方 Node 发行版。

## 直接使用

构建产物：`build/DSH.app`（ad-hoc 签名，本机可直接双击运行）。分发用 `build/DSH.zip`（解压后拖入「应用程序」即可）。

```bash
open build/DSH.app
```

首次启动会：检查 Node（缺失则自动下载官方 LTS 发行版）→ `npm install @deepseek-ai/dsh@latest`（约几十秒）→ 启动 `dsh web` → 窗口加载 GUI。注意：从 Finder 启动的应用不继承终端 PATH，所以应用自带 Node 检测/下载是必需的兜底。

## 从源码构建

依赖：Rust（1.95+）、Xcode Command Line Tools、python3（仅生成图标用）。

```bash
scripts/build-app.sh          # 产出 build/DSH.app
scripts/build-app.sh --zip    # 再产出 build/DSH.zip
```

项目使用项目内的 `.cargo` 作为 CARGO_HOME，不会污染 `~/.cargo`。

## 使用说明

| 操作 | 方式 |
|---|---|
| 显示/隐藏窗口 | 托盘图标左键单击；菜单「显示 / 隐藏窗口」 |
| 关闭窗口后继续运行 | 点窗口红叉只是隐藏，服务在菜单栏继续运行 |
| 检查更新 | 托盘 →「检查更新…」；发现新版会自动下载，重启 Harness 生效 |
| 重启 Harness | 托盘 →「重启 Harness」（会重启本地 dsh web 进程） |
| 打开配置目录 | 托盘 →「打开配置目录 (~/.dsh)」 |
| 开发者工具 | 菜单「视图」→「开发者工具」（⌥⌘I）；重新载入 ⌘R |
| 退出 | 托盘 →「退出 DSH Client」或 ⌘Q（会同时终止本地 dsh 进程） |

日志：`~/Library/Application Support/DSH Client/logs/dsh-client.log`

## 环境变量

- `DSHCLIENT_HOME`：覆盖应用数据目录（默认 `~/Library/Application Support/DSH Client`）
- `DSHCLIENT_NODE`：指定 Node 可执行文件路径（跳过自动检测/下载）

## 发布（GitHub Actions）

推 `v*` tag 会自动构建并发布 GitHub Release（见 `.github/workflows/release.yml`）：

- `macos-15` runner：原生构建 arm64 + 交叉编译 x86_64 → lipo 合成通用二进制
- 产出三个 zip：`DSH-arm64.zip`、`DSH-x86_64.zip`、`DSH-universal.zip`
- 手动触发（`workflow_dispatch`）只构建上传 artifacts，不发 Release

```bash
git tag -a v0.2.0 -m "release v0.2.0" && git push origin v0.2.0
```

发布产物为 ad-hoc 签名、未公证；正式分发可在此基础上接入 Apple Developer 签名与 notarytool 公证。

## 常见问题

- **强制退出后端口被占用？** 壳会记录 dsh 的 pid，下次启动时自动回收残留进程。
- **第一次打开提示「无法验证开发者」？** 本应用是 ad-hoc 本地签名（未做 Apple 公证）。右键点击 →「打开」即可；正式分发需要 Apple Developer 账号签名+公证。
- **更新跟不上官方？** 壳只依赖 `dsh web` 的 CLI 与打印 URL 这两个稳定接口；dsh 还是 0.x，若某版 CLI 行为变化，改 `worker.rs` 中对应的启动参数即可，壳逻辑无需重写。
