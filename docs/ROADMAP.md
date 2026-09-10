# Atoll 功能路线图

路线图描述用户能做什么及明确的接入边界。当前以 **Codex** 为支持和验证对象；Claude Code 保留为实验性兼容代码，未在真实 Claude Code 环境验证，新配置默认关闭其显示。

## 本轮交付

| 编号 | 功能 | 状态 | 用户可用的能力与边界 |
| --- | --- | --- | --- |
| F01 | Codex 桌面会话直达 | 已实现 | 无终端信息的本地会话可通过官方 `codex://threads/<id>` 打开对应对话；终端失效时也可回退。需安装并注册 Codex 桌面应用；只接受合法本地会话 ID。沿用桌面应用本身的登录和会话访问状态 |
| F02 | 完整的 Codex 原生提问回复 | 已实现，接入为实验性 | 通过 `atoll-codex.exe` 启动 CLI 会话，在卡片中逐题回答、查看完整选项说明、输入多行文本、返回修改草稿并统一提交；秘密输入遮罩。卡片与终端先答者生效，取消、断连会撤销卡片。基于真实 `item/tool/requestUserInput`，不用工具审批代替回复 |
| F03 | Windows Terminal 精确跳转 | 已实现 | 通过 Atoll 启动时记录窗口、标签页、分屏身份；隐藏标签页及运行中输出变化仍能回到原分屏。身份失效后回退终端窗口或 Codex 桌面对话。当前验收范围为 Windows Terminal 与 Codex 桌面端，IDE 暂不纳入 |

F02 的接入范围是 **通过 Atoll 启动或恢复的 Codex CLI 会话**。现有桌面会话和普通 `codex` 命令启动的会话仍在 Codex 中回答。上游 app-server / WebSocket 接口处于实验阶段，Atoll 的本地中转仅监听回环地址、要求临时令牌且拒绝浏览器 Origin。Atoll 不可用时仍可在 Codex 终端回答。

当前 Codex 原生问题支持单选或自由文本，没有多选字段；不把多选伪装成工具审批。多选待上游接口提供相应能力后再评估，不作为此次 Codex 接入的验收项。

## 已有能力

- 在 Windows 任务栏显示 Codex 额度和运行、等待、完成状态，点击展开详情。
- 悬停预览待处理会话，移开自动收起，预览不抢焦点。
- 安装、检查、卸载 Codex hooks，在 Atoll 中允许或拒绝实际工具审批。
- 长后台任务完成后显示静音 Windows 通知，**三秒后收起**；运行期间点击返回会话。
- 从 Codex 本地日志发现会话和额度，按活动显示会话、恢复上次显示状态。
- 开机启动、任务栏显隐和额度颜色设置；安装 hooks 前备份，保留用户配置。
- Claude Code 的额度、hooks 和单题兼容保留为 **实验性**，不承诺正式支持。

## 后续功能

对照 [Open Island README](https://github.com/Octane0411/open-vibe-island/blob/334c58073ec0ea8a1b34da0c71f969b1affd0959/README.md) 与其 [路线图](https://github.com/Octane0411/open-vibe-island/blob/334c58073ec0ea8a1b34da0c71f969b1affd0959/docs/roadmap.zh-CN.md)，比较基线为 2026-09-10。仅保留 Windows 与 Codex 的实际需求，不照搬 macOS 产品形态。

| 编号 | 功能 | 希望实现什么 | 状态 |
| --- | --- | --- | --- |
| F04 | 更多代理 | Cursor、Gemini CLI、OpenCode 等代理的会话与操作 | 不做：当前只考虑 Codex |
| F05 | 通知偏好与声音 | 分代理提醒、提示音、退出后的通知恢复 | 不做：保留静音完成通知总开关 |
| F06 | 中英文界面切换 | 在设置中切换语言 | 不做：保持现有界面和双语 README |
| F07 | 检查更新与升级 | 程序内发现新版本，完成可验证的 Windows 升级与回退 | 待排期 |
| F08 | WSL / SSH 会话 | 查看远端工作，并返回对应终端连接 | 候选，待具体场景 |
| F09 | Codex 桌面生命周期与双向操作 | 更稳定地跟随桌面会话，并在桌面提供可接入接口后处理原生提问 | 候选；直达对话已由 F01 覆盖 |

刷新频率、面板尺寸、卡片停留等体验参数按反馈调整，不作为主要功能里程碑。实现限制见 [已知问题](KNOWN_ISSUES.md)。

## 接入依据与验证

- [Codex 桌面命令](https://learn.chatgpt.com/docs/app/commands)：官方本地会话深链。
- [Codex app-server](https://learn.chatgpt.com/docs/app-server)：原生提问及请求撤销通知，CLI remote 连接；协议结构同时对照本机 CLI 0.154.0 生成的 JSON Schema。
- [Codex hooks](https://learn.chatgpt.com/docs/hooks)：工具审批独立于原生提问，新增 hooks 仍需在 `/hooks` 中信任。
- [Windows 桌面通知](https://learn.microsoft.com/en-us/windows/win32/shell/quickstart-sending-desktop-toast)：三秒收起已通过 Windows 实际 ApplicationHidden 回调验证。
- Windows Terminal 1.24 的实际分屏、输出变化、隐藏标签页跳转测试通过；提问回传、终端抢先回答和重复回复通过真实本地管道与中转测试；本机 Codex app-server 握手通过。模型实际发起提问和不同桌面版本的定位仍需持续兼容验证。
