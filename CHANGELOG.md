# LOG

> **⚠️ 0.3.0 Rust 版本与 旧版0.3.0以下（Python + Arduino C++）不兼容。**
> 旧版 Python 桌面端（cc_state_bridge.py / ble_daemon.py 等）和 Arduino 固件（.ino）已全部移除，
> 由 Rust 全栈重写替代。旧版固件的 BLE 服务和状态文件格式与新版 daemon 不兼容，
> 需要同时升级固件和桌面端，不能混用。

---

## [0.4.1] — 2026-09-13

### 修复 — 「状态不同步」与「生命周期管理失效」

**daemon（`Rust/crates/daemon`）**

- **单实例保护**：新增命名互斥体（`Local\CursorLightDaemon`）+ pipe `first_pipe_instance` 双保险。
  旧版本允许多个 daemon 同时存活，两个进程争抢同一条 BLE 链路、各自维护状态机，
  灯效随机跳变。实测曾出现两个 daemon 并存的僵尸状态。
- **pipe 消息不再被截断**：旧实现只 `read` 一次固定 4096 字节就当成完整消息，
  PostToolUse 里带文件内容的大 JSON（Edit / Write）全部解析失败（实测 18 次），
  整条事件丢失 → 状态机收不到「工具干完了」，灯卡在 busy / alarm。
  现在按 `\n` 组帧、读到 EOF，上限 1 MB。
- **pipe 不再丢消息**：每个客户端改用全新实例服务，并始终保留一个空闲实例在监听。
  旧实现 `connect → read → disconnect → connect` 复用同一实例，
  实测 20 条消息丢 1 条；丢的如果恰好是 `stop` / `session_end` / `post_tool`，灯就停在错误状态。
- **daemon 不再僵尸化**：`tokio::select!` 的 `else` 分支在旧代码里永远不可达
  （tick 分支用的是不可反驳模式，永不 disable），pipe 任务一死主循环就永久空转：
  进程活着、没有 pipe、却还在后台抢 BLE。现在显式处理 `None` → 干净退出。
- **日志不再 panic**：`println!` / `eprintln!` 在控制台被销毁后会 panic，
  而 panic 恰好发生在 pipe 任务里 → 任务静默死亡。改为 `writeln!` 并忽略错误。
- **字段名修复**：Claude Code 发的是 `permission_mode`，旧代码只读 `perm_mode`，
  于是永远取到默认值 → 连 `Read` / `Glob` / `Grep` 都被判成「等授权」点红灯。
  现在两个字段名都接受，并把免授权工具移出 `PENDING_TOOLS`。
- **Stop 状态**：Claude Code 的 Stop hook 不带 `status`，旧代码永远得到 `unknown` → 永远绿灯，
  错误状态永远出不来。现在由 hook 侧注入 `--status completed`，
  并新增 `error` 动作接 `PostToolUseFailure` → 红灯真的能亮。
- **绿灯保护窗**：旧代码对所有 Stop 结果都设 2s 保护窗，
  error / success 会在 1 秒后被 `resolve_mode()` 覆盖成 green。
  现在只有结果本身是绿灯（中断）才设窗，新结果会清掉旧窗。
- **断连不丢状态**：mode 通信用 `watch` 通道（只保留最新值、永不阻塞），
  重连成功后无条件重发当前模式，并读回 ESP32 的 mode 特征值做对账。
- **BLE 子系统自愈**：初始化失败或任务意外退出会自动重启
  （旧版本失败即永久降级成「只记日志、没有灯」）。
- 新增 `cursorlight --version`；`status` 改用互斥体判断（能识别僵尸态）。

**hook / 安装（`setup.ps1`）**

- **生命周期计数重做**：会话按 `session_id` 去重（`HashSet`），
  `SessionEnd` 只对已知会话生效，活跃会话归零才退出。
- **探针不再污染计数**：旧 `ensure_daemon` 用 `{"action":"session_start"}` 当存活探针，
  于是每次 hook 调用都让计数 +1，计数永远归不了零 → daemon 永不退出、灯永远停在最后一个状态。
  现在改用新增的 `--ping`（无任何副作用），并且每次调用只发一条消息（stdin 只能被读一次）。
- **stdin 修复**：等待 daemon 就绪的 `timeout /t 1` 会吃掉 hook 的 stdin
  （实测导致 `session_id` 丢失），改为 `ping -n 2 127.0.0.1`。
- **安装顺序修复**：旧脚本先覆盖 exe 再检查进程，文件被占用时
  `$ErrorActionPreference = "Stop"` 会直接中断安装（二进制没更新、hook 也没重写）。
  现在先停旧 daemon（pipe stop + `Stop-Process` 兜底）→ 再安装 → 打印版本号确认新代码上线。
- 未识别的 hook action 不再往 pipe 里写垃圾消息（旧版会发出 `{"action":"!ACT!"}`）。
- `setup.ps1` 保持纯 ASCII：Windows PowerShell 5.1 会把无 BOM 的脚本按 ANSI 解码，
  非 ASCII 字节会破坏语法解析。

### 一键安装对"零环境用户"友好化

- `setup.ps1` 重写为 6 步，**使用者不需要任何开发环境**：无 Rust / cargo / espflash / 联网 /
  管理员权限，全部依赖（`bin\` 与 `Firmware\`）随发行包提供。
- 新增 **[1/6] 环境自检**：PowerShell 版本、安装目录可写、蓝牙支持服务、Claude Code 配置目录、
  Python（桌面圆点，可选）。除 Windows 本身外全部是"缺失只警告、不中断"。
- 新增 **[2/6] 发行包完整性检查**：缺少 `bin\cursorlight.exe` / `bin\pipe-client.exe` 时
  不再提示"请自己编译"，而是明确指向**完整发行包**。
- **Python 自动探测并写进 hook.bat**：依次尝试 `py -3` / `python` / `python3`，并实际执行
  `import sys, tkinter` 验证可用性。找不到时在 hook.bat 里生成禁用注释 + 安装时给出警告，
  实体灯照常工作（任务栏圆点是可选功能）。
- **健壮性**：`Invoke-Native` 增加 catch —— 程序无法启动（杀软拦截 / 策略限制）不会再让整个安装中断；
  daemon 启动失败改为警告 + 日志路径提示；安装后校验 `--version` 是否正常响应，用来发现杀软拦截。
- 发行包必须包含：`bin\cursorlight.exe`、`bin\pipe-client.exe`、`Firmware\*.bin`、
  `traffic_light_desktop.py`、`setup.bat`、`setup.ps1`、`Rust固件线刷程序\`、`web-OTA\`。
- README 补充：`bin\` 与 `Firmware\` 由发行包提供；自己构建时可用
  `CARGO_TARGET_DIR` 把产物放到项目外，避免项目目录重新膨胀 ~320 MB。

### 端到端延迟优化（实测热路径 227ms → 120ms，−47%）

- **BLE 任务改事件驱动**：旧实现固定 `sleep(150ms)` 轮询，每条命令平均白等 75ms；
  现在用 `watch::Receiver::changed()` 事件唤醒（空闲时最多等 1s 做重连兜底）。
- **debounce 收紧**：`Alarm 200→0`、`Green/Off 500→120`、`Red/Yellow 300→60`、其它 `400→90`；
  并且被 debounce 阻塞时**内联等满再发**，而不是交给主循环下一轮
  （事件驱动后那一轮可能等 1s，实测 SessionEnd 的 green 曾花 898ms，现在 98ms）。
- **hook 侧每个事件只 spawn 1 个进程**：删掉 `hook.bat` 里的 `chcp`（每次都是一次进程启动）
  与存活探针 `--ping`；改为 `pipe-client` 自己在 pipe 不可用时拉起 daemon 并补发消息
  （`DETACHED_PROCESS | CREATE_BREAKAWAY_FROM_JOB`，退路 `CREATE_NO_WINDOW`）。
  实测 hook 进程链 50ms → 25ms。
- **关键路径去掉文件 I/O 与重复日志**：先 `mode_tx.send` 再写 `state_desktop.json`；
  删掉与 `Parsed command` 重复的 `Received command`；日志文件句柄改为复用（不再每条 3 次系统调用）。
- **新增延迟基准脚本** `tools/bench-latency.ps1`（daemon 侧记录 `Latency: mode=X hook_to_ble=Yms gatt=Zms`，
  纯 ASCII 便于脚本解析）。剩余下限是 BLE 链路本身（一次带响应写入 65~179ms），
  要再压需要改固件（请求更短连接间隔 / 用无响应写入），已在架构文档里写明方案但**未启用**。

### 生命周期与桌面灯同步（开 / 关都对齐）

- **Claude 打开**：`SessionStart` 在本次进程的第一个会话时把状态机复位为 **green**
  并写状态文件；`hook.bat` 同时启动桌面圆点。
- **Claude 关闭**：`SessionEnd` 复位为 **green**、写状态文件，并关闭桌面圆点。
- **daemon 改为常驻**（最后一个会话结束不再退出）：绿灯由 BLE 任务异步送达、断连自动重试，
  不会再出现"发完就退出导致绿灯丢失、灯停在红色"；同时保持 pipe 与 BLE 热连接，
  下次会话零冷启动延迟。仅在长时间（2h）无事件且无活跃会话时兜底退出。
- **桌面圆点关闭更可靠**：`stop` 除 PID 文件外，新增**按窗口标题 `CL` 反查 PID** 的兜底；
  daemon 的兜底 taskkill 不再删除 PID 文件（删了会让 `py stop` 找不到目标，圆点一直亮着）。

### 文档

- 新增 `docs/ARCHITECTURE.md`（并复制一份到桌面 `CursorLight-架构说明.md`）：
  数据流图、组件清单、事件映射、状态机、生命周期语义、IPC/BLE 协议、
  延迟实测与分解、可靠性设计、路径速查、版本与构建。

### 测试

- 单元测试 29 个（pipe 解析 / 组帧 / 大 payload、状态机模式映射与绿灯保护窗）。
- 固件：nightly + `riscv32imc-unknown-none-elf` 目标 `cargo build --release` 通过，
  经 `espflash save-image` 产出 app 镜像，app descriptor 校验为 `0.4.1`。
- 端到端：ping 20/20、session_start 20/20 无丢失；20 KB payload 正常解析；
  `stop` → success 且 2 秒后不被覆盖；`error` → 红灯常驻；
  活跃会话归零后 daemon 自动退出并回到绿灯；第二个 daemon 立即退出（单实例生效）。

---

## [0.3.2] — 2026-09-12

### 变更

- 清理编译产物，缩减仓库体积
- 修复 hook 配置同步问题
- hook.bat 从版本控制移除（由 setup.ps1 自动生成）

---

## [0.3.1-beta] — 2026-09-12

### 变更

- **编译修复**：
  - 新增 `.cargo/config.toml`，设置默认编译目标为 `riscv32imc-unknown-none-elf`
  - `Cargo.lock` 中 `portable-atomic` 固定在 v1.14.0（v1.15.0 与 riscv32imc 不兼容）
  - 固件文件名更新为 `cursorlight-v.rs0.3.1-beta.bin`
- **flash.ps1**：改为自动查找 Firmware/ 目录下的 .bin 文件，不再硬编码版本号。

---

## [0.3.0-beta] — 2026-09-12

### 全栈 Rust 重写

- **桌面守护进程**（新）：`Rust/crates/daemon/` — 替代全部 Python 脚本
  - Named Pipe IPC (`\\.\pipe\cursorlight`)，延迟从 ~185ms 降到 ~31ms
    > ⚠️ **实测更正（V.rs0.4.1，2026-09-13）**：这里的 `~31ms` 从未成立，请勿作为性能承诺引用。
    > daemon 侧打点实测热路径为 **~120ms**（hook 进程链 ~25ms + BLE 写入 ~98ms），
    > 其中 BLE 链路占绝大部分且与语言无关；换成 Named Pipe 真正省掉的是旧实现
    > **150ms 的文件轮询**，而不是"快 6 倍"。语言本身带来的差异约 **每事件 15~30ms**
    > （Rust 进程启动 6.8ms vs `py -3` 22.2ms / `import bleak` 36.3ms，同机实测）。
    > 准确数字与分解见 README「性能」一节与 `docs/ARCHITECTURE.md`。
  - 从 `cc_state_bridge.py` 移植的状态机（11 种 DaemonCmd 变体）
  - 从 `ble_daemon.py` 移植的 BLE 长连接（btleplug，自动重连 + debounce）
  - CLI：`cursorlight start / stop / status / send <mode>`
- **删除旧代码**：8 个 Python 脚本、Arduino .ino 固件、install.ps1、部署脚本
- **目录重组**：
  - `ESP32_Firmware/` — Rust 固件 workspace（firmware + light-core）
  - `Rust/` — 桌面 daemon workspace
  - `hooks/` — Claude Code hook 脚本

### 不兼容说明

| 组件 | 旧版 | 新版 | 兼容性 |
|---|---|---|---|
| 固件 | Arduino C++ (.ino) | Rust | ❌ 不兼容 |
| 桌面端 | Python (bleak + asyncio) | Rust | ❌ 不兼容 |
| 状态传递 | state_desktop.json 文件轮询 | Named Pipe 直连 | ❌ 不兼容 |

---

## [0.2.0-selftest] — 2026-09-11

### 新增

- **开机自检**：上电后用阻塞延时跑 4 步固定灯序，不依赖 BLE 与任务调度，
  用于判定「PWM 是否真的在输出」「极性与引脚映射是否正确」：
  1. 红 0% / 黄 50% / 绿 100%
  2. 红 100% / 黄 0% / 绿 50%
  3. 原始占空比 255（应为全灭）
  4. 原始占空比 0（应为全亮）
- **LEDC 寄存器回读**：自检结束打印 `timer0` 的 `duty_res / clk_div / tick_sel / pause / rst`，
  以及三个通道的 `timer_sel / sig_out_en / idle_lv / duty_start / duty`，
  用于确认配置是否真正落到硬件。
- **固件版本标识**：开机 banner 增加一行版本号，便于分辨板子上跑的是哪一版。
- **发行包内附文档**：包内加入本文件与 `README.md`。

### 变更

- **线刷镜像裁剪**：整片镜像由 4 MB 裁到约 396 KB（去掉尾部全 `0xFF` 填充，按 flash 扇区对齐），
  线刷耗时降到约十分之一。
- **`flash.ps1` 默认波特率 460800**（可用 `-Baud` 覆盖），并显示镜像大小与耗时提示。

---

## [0.1.1] — 2026-09-11

### 新增

- `ledc.set_global_slow_clock(LSGlobalClkSource::APBClk)`：显式选择 APB 时钟源
  （esp-hal 的分频计算以 APB 时钟为前提）。
- LED 通道初始占空比设为 100%（默认熄灭），避免异常时呈现"全亮"。
- 动画任务改为**先输出一帧再等待节拍**，并打印一次 `LED ready: r=.. y=.. g=..`。
- `pack.ps1` 自动把版本号写入包内 `manifest.json`。

---

## [0.1.0] — 2026-09-11

### 新增

- **首个完整的 Rust 固件**：BLE 灯效服务（`b8b7e001/…/b8b7e002`，READ + WRITE + NOTIFY）
  与 OTA 服务（`b8b7e0f0/f1/f2/f3`），广播名 `CursorLight`。
- **灯效引擎**：11 种模式（demo / thinking / ai / busy / success / error / alarm / off / red / yellow / green），
  5 ms 节拍、三路 8 位 PWM、公共正极反相输出、10 分钟空闲熄灯。
- **灯效逻辑独立成 `light-core`**：纯函数实现，主机端可直接单测。
- **无线 OTA**：双槽分区表 + 分块写入 + SHA-256 校验 + otadata 切换，
  写入失败或断电不影响正在运行的固件。
- **网页升级页** `web-ota/index.html`：Web Bluetooth 连接、流控、进度与错误诊断，
  并内置"设备上没有 OTA 服务（还是旧固件）"的识别提示。
- **零依赖刷机包**：`pack.ps1` 一键打包，内置 espflash，用户双击 `flash.bat` 即可线刷。
- **主机端单测**：灯效时序、模式解析、otadata 编解码共 13 个用例。
