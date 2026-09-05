# Atoll

[中文](#中文) · [English](#english)

<p align="center">
  <a href="https://github.com/WXGopher/atoll/actions/workflows/ci.yml"><img src="https://github.com/WXGopher/atoll/actions/workflows/ci.yml/badge.svg" alt="CI"></a>
  <a href="https://github.com/WXGopher/atoll/releases/latest"><img src="https://img.shields.io/github/v/release/WXGopher/atoll?include_prereleases" alt="Release"></a>
  <img src="https://img.shields.io/badge/platform-Windows-0078d4" alt="Windows">
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-GPL--3.0-blue" alt="GPL-3.0-or-later"></a>
</p>

## 中文

**在 Windows 任务栏查看 Claude Code 和 Codex 的剩余额度，点击查看会话详情，并直接处理 Claude Code 的审批和提问。**

Atoll 通过 Claude Code hooks 和 Codex 本地会话日志跟踪活动。平时只在任务栏显示简洁的额度与状态，需要时展开详情或审批卡片。

<p align="center">
  <img src="docs/panel.png" width="400" alt="Atoll 详情面板：会话状态和额度窗口">
</p>

### 功能

- **任务栏额度与状态**：显示每个代理最紧张的额度窗口，以及等待处理、运行中、已完成的会话数量。颜色阈值可在设置中调整；仅等待或运行状态需要动画。
- **按活动显示代理**：启动时恢复上次保存的代理显隐、额度和会话文本，不主动刷新额度。收到 Claude hook 或新的 Codex 日志事件后更新，并隐藏连续十五分钟没有活动的代理；再次活动时自动显示。点击详情不会触发额度请求。
- **详情面板自动收起**：点击任务栏控件或托盘图标展开；点击桌面、其他窗口，或切换到其他窗口后自动收起。再次点击 Atoll 图标也能关闭。
- **Codex 会话自动识别**：每两秒读取本地日志中的开始、完成和中断事件，支持从旧日志目录恢复的会话。启动时先建立日志基线，新的事件到来后更新显示；进入实时状态后，连续十五分钟没有活动的会话会从列表移除。
- **Claude Code 审批卡片**：允许或拒绝工具调用，回答 `AskUserQuestion`。已被你的权限设置允许的工具调用不会弹出审批卡片。
- **返回会话终端**：对有终端信息的会话，点击详情行可定位 Windows Terminal 或 VS Code 中对应的终端。无法定位终端的会话行不会显示可点击提示。
- **设置与托盘**：支持开机启动、按代理显示或隐藏任务栏内容、修改颜色阈值。右键任务栏控件或托盘图标进入设置或退出。
- **任务栏集成**：跟随任务栏位置、自动隐藏和通知区域大小变化；嵌入失败时使用贴近任务栏的浮动显示。重复启动 Atoll 会替换旧实例。
- **额度读取**：Claude Code 使用其已有凭据读取额度，并尽量复用本机缓存；Codex 从本地 rollout 日志读取额度。请求受限时会退避重试。

<img src="docs/readout.png" width="96" alt="垂直任务栏中的额度控件">
<img src="docs/card.png" width="440" alt="Claude Code 工具审批卡片">

项目仍在早期开发。Claude Code 的 hooks、审批和终端定位已接入；Codex 目前支持额度和会话状态，审批回复及终端定位仍待接入。部分截图来自较早版本，具体外观以当前程序为准。

### 安装与使用

从 [GitHub Releases](https://github.com/WXGopher/atoll/releases) 下载最新 Windows x86_64 压缩包，解压后在该目录运行：

```powershell
.\atoll.exe setup install claude
.\atoll.exe
```

第一条命令会将 `atoll.exe` 和 `atoll-hook.exe` 复制到 `%LOCALAPPDATA%\Atoll\bin` 并安装 Claude Code hooks。只使用 Codex 时，直接运行 `atoll.exe` 即可自动读取本地会话。

- 左键点击任务栏控件或托盘图标：展开或收起详情。
- 点击详情以外的位置，或切换窗口：自动收起详情。
- 右键点击任务栏控件或托盘图标：打开设置或退出。
- 需要开机启动时，在设置中打开相应开关。

检查或移除 Claude Code hooks：

```powershell
.\atoll.exe setup status claude
.\atoll.exe setup uninstall claude
```

`atoll.exe headless` 可在终端输出收到的 hook 事件，用于排查集成问题。它只监视 hook 事件流，不显示窗口。

### 配置与本地数据

Atoll 在现有 hooks 旁添加自己的配置，卸载时只移除自己添加的部分。默认不修改 Claude Code 的 `statusLine`。旧的 `--wrap-status-line` 兼容选项仍保留，但通常无需使用。

| 路径或设置 | 用途 |
| --- | --- |
| `~/.claude/settings.json` | 安装、检查或移除 Atoll 的 hooks |
| `~/.claude/.credentials.json` | 只读，用于请求 Claude Code 额度；不记录凭据 |
| `~/.claude/projects/**/*.jsonl` | 只读，用于会话标题 |
| `~/.codex/sessions/**/*.jsonl` | 只读，用于 Codex 会话状态与额度 |
| `%LOCALAPPDATA%\Atoll\bin` | hooks 使用的稳定安装路径，避免后续编译覆盖正在使用的程序 |
| `%APPDATA%\Atoll\display.json` | 上次显示的代理、额度和会话文本；不保存审批连接或终端跳转目标 |

环境变量：

| 变量 | 用途 |
| --- | --- |
| `CODEX_HOME` | 为 Codex 会话跟踪指定 `.codex` 目录的替代位置 |
| `ATOLL_PIPE_NAME` | 指定命名管道，适用于隔离开发实例 |
| `ATOLL_CONFIG_DIR` | 指定配置目录 |
| `ATOLL_SKIP_HOOKS=1` | 让 hook 程序直接退出，不连接 Atoll |

Atoll 未启动、忙碌或响应超时时，hooks 会让代理回到原终端继续询问。卸载 hooks 不会删除已安装的程序文件。

### 构建与验证

在 Windows 上安装近期稳定版 Rust 工具链后运行：

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

需要真实 Windows 桌面的窗口及显示恢复回归测试可单独运行：

```powershell
cargo test -p atoll native_slint_readout_stays_frameless -- --ignored --nocapture
cargo test -p atoll --test display_lifecycle -- --ignored --nocapture
```

正式发布包由 [GitHub Actions](.github/workflows/release.yml) 构建，包含 `atoll.exe`、`atoll-hook.exe`、README 和许可证。每个压缩包均提供 `SHA256SUMS.txt` 和构建来源证明，可使用 GitHub CLI 验证：

```powershell
gh attestation verify atoll-v0.1.4-windows-x86_64.zip --repo WXGopher/atoll
```

后续计划包括悬停预览、Codex 审批集成及通知，见 [开发路线图](docs/ROADMAP.md)。

### 致谢与许可证

Atoll 受到 macOS 项目 [open-vibe-island](https://github.com/Octane0411/open-vibe-island) 的启发，是独立的 Windows 原生实现，并非其代码移植。

采用 GPL-3.0-or-later 许可证，详见 [LICENSE](LICENSE)。

---

## English

**See Claude Code and Codex quota in the Windows taskbar, open session details with a click, and answer Claude Code approvals and questions without returning to the terminal.**

Atoll follows Claude Code hooks and Codex's local session logs. Quota and task counts stay in the taskbar; details and approval cards appear when needed.

<p align="center">
  <img src="docs/panel.png" width="400" alt="Atoll's detail panel with session states and quota windows">
</p>

### Features

- **Taskbar quota and status**: see each agent's tightest quota window and counts of waiting, running and completed sessions. Colour thresholds are configurable; only waiting or running states animate.
- **Activity-driven visibility**: startup restores the saved agent visibility, quota and session text without refreshing quota. A Claude hook or a new Codex log event updates the display and hides agents silent for fifteen minutes; activity brings them back. Opening details does not request quota.
- **Details that dismiss automatically**: click the readout or tray icon to open the panel. Click the desktop, another window, or switch windows to dismiss it. Clicking the Atoll icon again also closes it.
- **Automatic Codex session tracking**: local start, completion and interruption events are read every two seconds, including conversations resumed from older directories. Startup establishes a log baseline; new events resume live display updates. Once live, sessions leave the list after fifteen minutes without activity.
- **Claude Code approval cards**: allow or deny tools and answer `AskUserQuestion`. Tools already allowed by your own permissions do not raise a card.
- **Return to the session's terminal**: rows with terminal metadata can locate the corresponding Windows Terminal or VS Code terminal. Rows whose terminal is unknown show no click affordance.
- **Settings and tray**: configure launch at login, agent visibility and colour thresholds. Right-click the readout or tray icon for Settings and Quit.
- **Taskbar integration**: follows the taskbar's position, auto-hide and notification-area size; falls back to a floating readout beside the taskbar if embedding fails. Starting another Atoll replaces the existing instance.
- **Quota readings**: Claude Code's existing credentials fetch quota with local cache reuse where possible; Codex quota comes from local rollout logs. Rate-limited requests back off before retrying.

<img src="docs/readout.png" width="96" alt="Quota readout in a vertical taskbar">
<img src="docs/card.png" width="440" alt="Claude Code tool approval card">

The project is in early development. Claude Code hooks, approvals and terminal navigation are integrated. Codex supports quota and session state; approval replies and terminal navigation are still planned. Some screenshots show earlier versions of the interface.

### Install and use

Download the latest Windows x86_64 archive from [GitHub Releases](https://github.com/WXGopher/atoll/releases), extract it, and run these commands from that directory:

```powershell
.\atoll.exe setup install claude
.\atoll.exe
```

The first command copies `atoll.exe` and `atoll-hook.exe` to `%LOCALAPPDATA%\Atoll\bin` and installs Claude Code hooks. For Codex alone, run `atoll.exe` directly to discover local sessions automatically.

- Left-click the taskbar readout or tray icon to toggle details.
- Click outside the panel or switch windows to dismiss it.
- Right-click the readout or tray icon for Settings and Quit.
- Enable launch at login in Settings if desired.

Check or remove Claude Code hooks:

```powershell
.\atoll.exe setup status claude
.\atoll.exe setup uninstall claude
```

`atoll.exe headless` prints incoming hook events to the terminal for troubleshooting. It watches the hook stream without displaying windows.

### Configuration and local data

Atoll adds its hooks alongside yours and removes only what it added. Claude Code's `statusLine` is unchanged by default. The legacy `--wrap-status-line` option remains available but is normally unnecessary.

| Path or setting | Purpose |
| --- | --- |
| `~/.claude/settings.json` | Install, inspect or remove Atoll hooks |
| `~/.claude/.credentials.json` | Read-only credentials for Claude Code quota requests; credentials are not logged |
| `~/.claude/projects/**/*.jsonl` | Read-only session titles |
| `~/.codex/sessions/**/*.jsonl` | Read-only Codex session activity and quota |
| `%LOCALAPPDATA%\Atoll\bin` | Stable hook binaries, kept separate from later builds |
| `%APPDATA%\Atoll\display.json` | Saved agent visibility, quota and session text; excludes approval connections and terminal targets |

Environment variables:

| Variable | Purpose |
| --- | --- |
| `CODEX_HOME` | Alternate `.codex` directory for Codex session tracking |
| `ATOLL_PIPE_NAME` | Named pipe override for an isolated development instance |
| `ATOLL_CONFIG_DIR` | Configuration directory override |
| `ATOLL_SKIP_HOOKS=1` | Exit the hook immediately without connecting to Atoll |

If Atoll is unavailable, busy or times out, hooks let the agent continue prompting in its terminal. Uninstalling hooks leaves the installed binaries in place.

### Build and verify

With a recent stable Rust toolchain on Windows:

```powershell
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build --workspace --release
```

Run the native window and display restoration regressions separately on a Windows desktop:

```powershell
cargo test -p atoll native_slint_readout_stays_frameless -- --ignored --nocapture
cargo test -p atoll --test display_lifecycle -- --ignored --nocapture
```

Release archives are built by [GitHub Actions](.github/workflows/release.yml) and contain `atoll.exe`, `atoll-hook.exe`, the README and license. Each archive has a `SHA256SUMS.txt` checksum alongside it and a build provenance attestation. Verify the attestation with the GitHub CLI:

```powershell
gh attestation verify atoll-v0.1.4-windows-x86_64.zip --repo WXGopher/atoll
```

Hover previews, Codex approval integration and notifications are planned; see the [roadmap](docs/ROADMAP.md).

### Acknowledgements and license

Atoll is inspired by [open-vibe-island](https://github.com/Octane0411/open-vibe-island) for macOS. It is an independent Windows-native implementation, not a port of that project's code.

Licensed under GPL-3.0-or-later. See [LICENSE](LICENSE).
