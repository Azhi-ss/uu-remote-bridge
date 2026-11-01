# 原生桌面操作

## Agent：观察 → 操作 → 核实

Agent 通过 Bash 调用原生 CLI，无需打开本机 UU 可视化窗口，也无需 Computer Use 点击该窗口。
截图仍是 GUI 任务的观察依据；远程命令可直接完成的工作走 terminal-exec。

```sh
uu-native-remote desktop-snapshot DEVICE_ID /tmp/uu-desktop.ppm
# macOS 主控：转成图像工具能读取的 PNG
sips -s format png /tmp/uu-desktop.ppm --out /tmp/uu-desktop.png
```

用图像查看工具读取该 PNG，依据这张远程画面定位。`desktop-snapshot` 输出完整分辨率，
例如 2816×1760；将截图上的像素坐标除以图像宽高，得到 0..1 的归一化坐标。
本机 UU 窗口尺寸、终端字符列数都不参与这里的坐标计算。

一次 `input` 可发送一个对象或多个事件的数组。点击示例（替换为实际观察到的位置）：

```sh
uu-native-remote input DEVICE_ID '[{"action":"mouse_move_absolute","abs_x":0.5,"abs_y":0.5},{"action":"mouse_press","button":1},{"action":"mouse_release","button":1}]'
uu-native-remote input DEVICE_ID '{"action":"text_input","content":"hello 你好"}'
# Ctrl+A：17 = Ctrl，65 = A；每个按下要有匹配释放
uu-native-remote input DEVICE_ID '[{"action":"kbd_press","key":17},{"action":"kbd_press","key":65},{"action":"kbd_release","key":65},{"action":"kbd_release","key":17}]'
```

输入协议对官方 Windows host 使用数字 VK 与按钮：

| 操作 | 字段 |
| --- | --- |
| 移动 | `action: mouse_move_absolute`, `abs_x`, `abs_y`：0..1 |
| 按住 / 释放 | `mouse_press` / `mouse_release`, `button`：左 1 / 右 2 / 中 4 |
| 拖动 | move → press → move… → release |
| 滚轮 | `mouse_scroll`, `delta_x`, `delta_y`；纵向 +120 向上、−120 向下 |
| 按键 | `kbd_press` / `kbd_release`, `key`：Windows VK 数字 |
| 常用 VK | Backspace 8、Tab 9、Enter 13、Shift 16、Ctrl 17、Alt 18、Esc 27、方向 37–40、Delete 46、Win 91 |
| 字符串 | `text_input`, `content`：Unicode 字符串 |

通过 JSON 序列化器构造动态文本，并用进程参数数组传入，避免手工 shell 插值破坏引号。
操作后重新 snapshot 并查看，确认远端变化；异步打开程序时给它完成时间。
结束时释放操作中按住的按钮/键；删除本次临时截图，不提交用户画面或凭据。

## 用户：直接操作预览

```sh
uu-native-remote desktop-view DEVICE_ID 160
```

该命令需要交互式 TTY，按终端尺寸缩放画面，点击、拖动、滚轮、打字、粘贴都在同一个连接里完成。

- `Ctrl+]`：本地退出并恢复终端。传统终端的 `Ctrl+5` 编码也可能触发退出。
- `Ctrl+C`：发送到远端。
- `Ctrl+G` 后按 `R`：Win+R。前缀后按其他键发送 Win+该键，Esc 取消前缀。
- 终端失焦或退出时释放正在拖动的鼠标按钮及修饰键。

给用户直接操作时使用这一模式。Agent 使用单次 snapshot/input 循环更容易取得可检查的画面；
若用 PTY 自动测试 viewer，向该 PTY 写标准 SGR 鼠标、按键、bracketed-paste 序列，并持续读取输出。
当前没有后台桌面 daemon 或 JSONL 长连接控制接口，不要虚构这些命令。

边界：ANSI 预览最多 10 fps、鼠标精度以终端字符格为单位；没有音频或剪贴板同步。
传统终端无法报告完整物理键盘状态，快捷键按成对 press/release 转发，不适合持续按键的游戏。
终端/操作系统截获的组合键无法到达 viewer。独立 `input`/snapshot 与持续 viewer 不同时打开。
