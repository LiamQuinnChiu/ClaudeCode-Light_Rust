# ClaudeCode-Light (Rust全栈重构版)

![License](https://img.shields.io/badge/License-MIT-blue.svg)
![Rust](https://img.shields.io/badge/Rust-1.95%2B-000000.svg?logo=rust&logoColor=white)
![Platform](https://img.shields.io/badge/Platform-Windows-0078D6.svg?logo=windows&logoColor=white)
![ESP32](https://img.shields.io/badge/ESP32--C3-Rust%20embedded-E8372C.svg?logo=rust&logoColor=white)
![Version](https://img.shields.io/badge/version-V.rs0.4.1-brightgreen.svg)

一个给 Claude Code 用的物理状态灯。挂在 Claude Code 的 Hook 上，把 AI 状态（思考、跑命令、等授权、出错、完成）翻译成红黄绿三盏灯。抬头看一眼灯就知道状态。

#### 全栈 Rust：ESP32-C3 固件 + 桌面守护进程，hook → 灯 的延迟在 30ms 量级。

**===================**

> ### ⚠️从旧版 C++ 固件升级必须线刷一次，旧固件没有 OTA 服务。

双击 `Rust固件线刷程序\flash.bat`，按提示操作（自动查找 `Firmware/` 下的 `.bin`，缺 `espflash` 时自动下载）。刷完灯开始跑 demo 模式。

**===================**

## 性能（V.rs0.4.1 版本基准测试）

热路径 = Claude Code hook 触发 → 灯变化。在真实 ESP32-C3 上实测（n=16）：

| 阶段 | 时间（中位数） |
|---|---|
| hook 进程链（cmd + hook.bat + pipe-client） | ~25ms |
| daemon：命令到达 → ESP32 确认写入 | ~98ms（65~179） |
| **合计** | **~120ms** |

- daemon 自身开销（管道 + 状态机）已经测不出来：`hook_to_ble` 与 `gatt` 几乎相等
- 剩余下限是 BLE 链路本身（一次带响应写入 ≈ 1~2 个连接间隔）；
  要再压需要改固件的连接间隔或改用无响应写入，方案见架构文档
- daemon 常驻，**同一会话内没有冷启动**；只有开机后第一次需要 0.7~1.7s 起进程 + 3~6s 连 BLE

复测：`tools\bench-latency.ps1`（可选 `-GapMs 120` 模拟密集切换）。

## 快速开始

### 1. 一键配置（零依赖，不需要任何开发环境）

双击 `setup.bat`

### 2. 首次刷固件（USB）

> ⚠️ 从旧版 C++ 固件升级必须线刷一次，旧固件没有 OTA 服务。

双击 `Rust固件线刷程序\flash.bat`，按提示操作（自动查找 `Firmware/` 下的 `.bin`，缺 `espflash` 时自动下载）。刷完灯开始跑 demo 模式。

### 3. OTA 无线升级

后续固件更新用蓝牙 OTA，不需要 USB：

1. 用 Chrome/Edge 打开 `web-OTA/index.html`
2. 选 `Firmware/` 目录下的 `.bin` 固件文件
3. 点「开始升级」，等进度条完成

> OTA 中断可重刷，不影响运行中的固件（双分区安全升级）。

## 从源码重新编译

### 桌面端

```powershell
cd Rust
cargo test --release        # 29 个单元测试：pipe 组帧 / 大 payload / 状态机
cargo build --release
# 把两个产物放到项目 bin\ 目录（setup.bat 从这里安装）
New-Item -ItemType Directory -Force ..\bin | Out-Null
Copy-Item target\release\cursorlight.exe ..\bin\ -Force
Copy-Item target\release\pipe-client.exe ..\bin\ -Force
```

> **保持项目文件夹精简**：编译产物默认落在 `Rust\target\`（约 320 MB，已被 `.gitignore` 忽略，
> 但会占磁盘）。想让项目目录始终只有源码，构建前设置一次 `CARGO_TARGET_DIR` 即可：
> ```powershell
> $env:CARGO_TARGET_DIR = "$env:LOCALAPPDATA\CursorLightBuild"
> cargo build --release   # 产物改到该目录，项目里不再出现 target\
> ```

### 固件

```bash
rustup toolchain install nightly
rustup target add riscv32imc-unknown-none-elf --toolchain nightly
cd Rust/ESP32_Firmware
cargo build --release       # 产物：target/riscv32imc-unknown-none-elf/release/cursorlight (ELF)
espflash save-image --chip esp32c3 --flash-size 4mb \
  target/riscv32imc-unknown-none-elf/release/cursorlight \
  ../../Firmware/CC-Status-Light_V.rs0.4.1.bin
```

线刷与 OTA 使用同一个**应用镜像**（写入 app 分区偏移 `0x10000`）。

## 目录结构

```
CC-Status-Light/
├── setup.bat + setup.ps1          # 一键配置（安装 bin/ + 生成 hooks + 启动 daemon）
├── Rust固件线刷程序/               # 首次 USB 线刷
│   ├── flash.bat
│   └── flash.ps1
├── bin/                           # 【发行包内置·不入库】cursorlight.exe、pipe-client.exe
├── Firmware/                      # 【发行包内置·不入库】CC-Status-Light_V.rs0.4.1.bin
├── web-OTA/                       # 浏览器 OTA 页面 + manifest.json
├── Rust/
│   ├── crates/daemon/             # 桌面守护进程源码
│   ├── crates/pipe-client/        # hook → daemon 的 pipe 客户端
│   └── ESP32_Firmware/            # ESP32 固件 workspace（firmware + light-core）
├── traffic_light_desktop.py       # 任务栏桌面圆点
├── docs/ARCHITECTURE.md           # 完整架构说明（数据流 / 协议 / 延迟 / 可靠性）
├── tools/bench-latency.ps1        # 端到端延迟基准
├── CHANGELOG.md
└── LICENSE
```

## 技术要点

### 桌面守护进程 (Rust)

- **Named Pipe IPC**：`\\.\pipe\cursorlight`，hook 直接写 pipe，无轮询
- **单实例保护**：命名互斥体 + pipe `first_pipe_instance` 双保险；多开会让两个进程抢同一条 BLE 链路，灯效错乱
- **按行组帧**：读到 `\n` 才算一条消息结束，单条上限 1 MB（Edit / Write 的大 JSON 不会被截断）
- **每连接一个新实例**：pipe 名始终保持可用，避免 `disconnect` 复用丢消息
- **状态机**：`thinking / pre_tool / post_tool / stop / idle / error / build / alarm / busy / denied / plan_*`，含超时升级、force_green 保护窗
- **事件驱动**：mode 走 `watch` 通道 + `changed()` 唤醒，没有轮询（旧实现固定 sleep 150ms，平均白等 75ms）
- **BLE 长连接**：扫描 → 缓存地址直连 → 指数退避重连；重连后**无条件重发当前模式**并读回特征值对账
- **常驻 + 生命周期**：会话按 `session_id` 去重；开关 Claude 都会把灯复位到 green；
  最后一个会话结束后 daemon 不退出，保持热连接（无冷启动），长时间无事件才兜底退出
- **hook 侧最小化**：每个事件只 spawn 一个 `pipe-client`；daemon 不在时由它自己拉起（不需要探针）

### ESP32 固件 (Rust embedded)

- **no_std** + Embassy 异步运行时
- **trouble-host**：纯 Rust BLE 协议栈，类型安全的 GATT 服务定义
- **BLE OTA**：双分区安全升级，SHA-256 校验，中断不影响运行固件
- **light-core**：独立 crate，可在 PC 上 `cargo test` 验证灯效算法

## 协议与致谢

MIT 协议，详见 [LICENSE](LICENSE)。

- 硬件方案和基础灯效来自 [JasonLam08/cursor_agent_status_light](https://github.com/JasonLam08/cursor_agent_status_light)
- 全栈 Rust 重写，ESP32 固件 + 桌面 daemon，替代原 Python + Arduino C++ 方案
