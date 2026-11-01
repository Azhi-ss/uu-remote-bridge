---
name: uu-remote-bridge
description: "通过 UU Remote 原生 CLI 操作远程电脑：执行指定 session 的命令、传文件、取桌面画面和发送鼠标键盘。提到 UU远程、uuyc-cli、SK 远程电脑或希望替代远程 Computer Use 时使用；本机应用操作不在此范围。"
license: MIT
metadata:
  implementation: "native-clean Rust host/viewer; bin/uu-native-remote"
---

# UU Remote native control

远程电脑操作优先通过本 skill 的 `uu-native-remote` Bash 接口完成。
命令任务直接用远程 terminal session；GUI 任务通过原生桌面画面与输入协议完成。
这替换的是“控制本机 UU 窗口，再在远程画面中操作”的 Computer Use 路径。
本机其他应用仍使用它们自己的 CLI、API 或 GUI 工具。

## 选择设备与通道

1. 用 `command -v uu-native-remote` 找到 CLI，运行 `uu-native-remote --help` 确认可用命令。
   若已有源码 checkout，也可通过该仓库的 `bin/uu-native-remote` 运行。
   新机器的 CLI 构建和 PATH 配置见[仓库安装说明](https://github.com/Azhi-ss/uu-remote-bridge/blob/rewrite/native-clean/README.md)。
2. `uuyc-cli device list --json` 匹配用户指定的名字与在线状态，保存本次设备 ID。
   `uuyc-cli device status --json` 查看本机已连接的远程设备。
   官方 CLI 要检查 JSON 的 `success`，它的退出码可能仍为 0。
3. 按任务选通道：

| 任务 | 接口 |
| --- | --- |
| 运行程序、查看配置、进入 WSL | `terminal-list` → `terminal-exec DEVICE SESSION COMMAND` |
| 上传、下载、目录与删除 | `files-list / files-upload / files-download / files-remove` |
| Agent 操作桌面 GUI | [桌面操作参考](references/desktop.md)：snapshot → inspect → input → verify |
| 用户直接操作终端预览 | `desktop-view DEVICE [COLUMNS]`，鼠标键盘直接转发 |

原生客户端读取 `UU_CONFIG` 或 `~/.config/uuremote/config.json` 中已有凭据；
不需要启动本机 UU 可视化远控窗口。官方 CLI 设备管理仍依赖已登录的官方 App。
原生凭据缺失/过期时报告具体错误，当前 clean 客户端没有 `login` 子命令。
桌面功能需 desktop 构建。在 CLI 源码仓库根目录执行
`cargo build --release --features desktop --manifest-path native-clean/Cargo.toml`。

## 命令与文件

```sh
uu-native-remote terminal-list DEVICE_ID
uu-native-remote terminal-exec DEVICE_ID session1 'whoami'
uu-native-remote files-list DEVICE_ID 'C:/Users/Public'
uu-native-remote files-upload DEVICE_ID ./local.txt 'C:/Users/Public/local.txt'
uu-native-remote files-download DEVICE_ID 'C:/Users/Public/local.txt' ./copy.txt
```

`terminal-exec` 按精确名称或数字 ID 附加已有 session，完成后 detach；不会杀掉会话。
从 session 列表确认 shell 后使用对应语法，Windows PowerShell 中进入 WSL 用 `wsl.exe`。
GUI 的 `uuyc-cli term attach` 只开窗口，不是可执行的 stdin/stdout 管道。
读配置优先远程命令或 files-download，文件内容与密钥只在当前任务需要的范围内处理。
文件传输完成后核对大小，重要内容核对哈希；清理由本次任务产生的临时文件。

## 连接与结果

每次 CLI 调用会建立并关闭连接。同一设备的桌面、独立 input 和文件命令顺序执行，
先退出持续的 `desktop-view` 再调用其他命令。先前由本任务打开的官方连接可在测试时断开，
结束后恢复；保留已有远程 terminal session。

错误 2002 表示设备占用。检查并关闭本任务自己的连接；确认是自己的残留连接时，
可仅为那一次调用设置 `UU_NATIVE_FORCE_JOIN=1`。其他使用者占用时协调，不把强制接管设为默认。
错误 2005 或 HTTP 429（可能附带 code 1004）表示限流，停止密集重试，至少等待 30 秒后仅重试一次。设备离线或凭据错误要解决根因。

命令返回、输入成功发送不等于远端完成：命令核对输出，GUI 核对新的桌面画面，文件核对内容。
实测范围是 macOS 主控连接官方 SK Windows 被控端；其他系统、锁屏、显示器布局需要单独验证。

只有需要兼容支持旧终端管道的官方 CLI 时，才读 [legacy-pipe.md](references/legacy-pipe.md)。
该旧路径的 512 KB 上限、速度基线和 shell 限制不适用于原生文件通道。
