mod ble;
mod instance;
mod log;
mod pipe;
mod state;
mod types;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

use crate::instance::InstanceGuard;
use crate::types::{DaemonCmd, Mode, State, TurnPhase};

/// 长时间没有任何 hook 事件、且没有活跃会话时的自动退出时间（毫秒）。
/// 这是"僵尸 daemon"的兜底：正常路径下 SessionEnd 会让计数归零并退出。
const IDLE_EXIT_MS: u64 = 2 * 60 * 60 * 1000;

#[derive(Parser)]
#[command(name = "cursorlight", version, about = "CursorLight 桌面守护进程")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 后台启动 daemon
    Start,
    /// 停止 daemon
    Stop,
    /// 查看运行状态
    Status,
    /// 手动发送模式
    Send {
        /// 模式名称（如 thinking, busy, success, error, alarm, off 等）
        mode: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // 1. 初始化日志
    log::init();

    // 2. 解析 CLI
    let cli = Cli::parse();

    // 3. 根据子命令分派
    match cli.command {
        Some(Commands::Start) => {
            cmd_start()?;
        }
        Some(Commands::Stop) => {
            cmd_stop()?;
        }
        Some(Commands::Status) => {
            cmd_status()?;
        }
        Some(Commands::Send { mode }) => {
            cmd_send(&mode)?;
        }
        None => {
            // 无子命令 → 前台运行 daemon
            run_daemon().await?;
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// CLI 子命令实现
// ---------------------------------------------------------------------------

/// 后台启动 daemon（Windows: CREATE_NO_WINDOW）
fn cmd_start() -> Result<()> {
    use std::process::Command;

    // 已经在跑就不要再生一个（多实例会抢同一条 BLE 链路，造成状态不同步）
    if let InstanceGuard::AlreadyRunning = instance::acquire(instance::SINGLETON_NAME) {
        log::info("Daemon is already running, skip start");
        return Ok(());
    }

    let exe = std::env::current_exe()?;
    log::info(&format!("Starting daemon in background: {}", exe.display()));

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        Command::new(&exe)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()?;
    }
    #[cfg(not(windows))]
    {
        Command::new(&exe).spawn()?;
    }

    log::info("Daemon started in background");
    Ok(())
}

/// 停止 daemon（通过 pipe 发送停止命令）
fn cmd_stop() -> Result<()> {
    log::info("Sending stop signal to daemon...");

    #[cfg(windows)]
    {
        use std::io::Write;
        match std::fs::OpenOptions::new()
            .write(true)
            .open(pipe::PIPE_NAME)
        {
            Ok(mut f) => {
                f.write_all(b"{\"action\":\"stop\",\"status\":\"stop\"}\n")?;
                let _ = f.flush();
                log::info("Stop signal sent");
            }
            Err(e) => {
                log::error(&format!("Cannot connect to daemon pipe: {}", e));
                log::info("Daemon may not be running");
            }
        }
    }

    Ok(())
}

/// 查看 daemon 状态。
///
/// 用命名互斥体判断，而不是"pipe 能不能开" ——
/// 后者在 pipe 正在 disconnect→connect 的间隙会误报，
/// 也看不出"daemon 活着但没有 pipe"的僵尸态。
fn cmd_status() -> Result<()> {
    match instance::acquire(instance::SINGLETON_NAME) {
        InstanceGuard::AlreadyRunning => log::info("Daemon is running"),
        InstanceGuard::Acquired(_guard) => log::info("Daemon is not running"),
        InstanceGuard::Unavailable(code) => {
            log::warn(&format!("Cannot determine status (mutex error {})", code))
        }
    }
    Ok(())
}

/// 手动发送模式命令到 daemon
fn cmd_send(mode_str: &str) -> Result<()> {
    let m = Mode::from_str(mode_str).ok_or_else(|| anyhow::anyhow!("Unknown mode: {mode_str}"))?;

    log::info(&format!("Sending mode: {}", m.as_str()));

    #[cfg(windows)]
    {
        use std::io::Write;
        let msg = serde_json::json!({ "action": m.as_str() });
        match std::fs::OpenOptions::new()
            .write(true)
            .open(pipe::PIPE_NAME)
        {
            Ok(mut f) => {
                f.write_all(msg.to_string().as_bytes())?;
                f.write_all(b"\n")?;
                let _ = f.flush();
                log::info(&format!("Mode '{}' sent to daemon", m));
            }
            Err(e) => {
                return Err(anyhow::anyhow!("Cannot connect to daemon: {}", e));
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Daemon 主循环
// ---------------------------------------------------------------------------

/// Daemon 主循环：协调 Named Pipe 接收、状态机处理、BLE 发送
///
/// 架构：
///   hook → Named Pipe → DaemonCmd → 状态机 → Mode → BLE task → ESP32
///
/// 两个通道：
/// - cmd_rx: pipe → 主循环（DaemonCmd, mpsc，保证事件顺序）
/// - mode_tx/mode_rx: 主循环 → BLE task（Mode, watch，只保留最新值，永不阻塞）
async fn run_daemon() -> Result<()> {
    // 单实例保护：拿不到锁说明已经有 daemon 在跑，直接退出。
    // 这是防止"两个 daemon 抢同一条 BLE 链路"的第一道防线。
    let _guard: Option<instance::SingleInstance> = match instance::acquire(instance::SINGLETON_NAME) {
        InstanceGuard::Acquired(g) => Some(g),
        InstanceGuard::AlreadyRunning => {
            log::warn("Another CursorLight daemon is already running, exiting");
            return Ok(());
        }
        InstanceGuard::Unavailable(code) => {
            log::warn(&format!(
                "Single-instance guard unavailable (mutex error {}), continuing without it",
                code
            ));
            None
        }
    };

    log::info("=== CursorLight daemon starting ===");
    log::info(&format!("Version {}", env!("CARGO_PKG_VERSION")));

    // 先同步创建 pipe 实例：第二个 daemon 会在这里失败（第二道防线）
    let first_server = match pipe::create_server() {
        Ok(s) => s,
        Err(e) => {
            log::error(&format!(
                "Failed to create Named Pipe ({}): another daemon is probably running",
                e
            ));
            return Ok(());
        }
    };

    // 创建 channel
    let (cmd_tx, mut cmd_rx) = mpsc::channel::<DaemonCmd>(256);
    // 通道负载带上"命令到达时刻"，用于测量 hook → BLE 的真实端到端延迟
    let (mode_tx, mode_rx) = watch::channel::<(Mode, Instant)>((Mode::Green, Instant::now()));

    // 启动 Named Pipe 监听任务（持有 cmd_tx：任务结束时主循环会收到 None 并退出）
    let pipe_handle = tokio::spawn(async move {
        if let Err(e) = pipe::pipe_server(first_server, cmd_tx).await {
            log::error(&format!("Pipe server stopped: {}", e));
        }
    });

    // 启动 BLE 子系统（内部含初始化失败重试与任务复活）
    let ble_handle = tokio::spawn(ble::ble_supervisor(mode_rx));

    log::info("All subsystems started. Waiting for hook events...");

    // 主循环：接收 pipe 命令 → 状态机 → 发送 BLE 模式
    let mut daemon_state = state::default_state();
    // 活跃会话 id 集合：按 id 去重，compact/resume 导致的重复 SessionStart 只算一次
    let mut sessions: HashSet<String> = HashSet::new();
    // 没有 session_id 时的回退计数
    let mut anonymous_sessions: u32 = 0;
    let mut saw_session_start = false;
    let mut last_event_ms = now_ms();
    let mut last_resolved_mode: Option<Mode> = None;

    // 定时检查超时升级的间隔（每秒检查一次）
    let mut resolve_interval = tokio::time::interval(Duration::from_secs(1));
    resolve_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // 启动时先落一次状态文件，避免桌面端点读上一次会话的陈旧状态
    write_state_json(&daemon_state, Mode::Green);

    loop {
        tokio::select! {
            // 注意：这里必须显式处理 None。
            // 旧实现用 `Some(cmd) = cmd_rx.recv()` + `else =>`，
            // 但 else 分支要求"所有分支都被 disable"，
            // 而下面那个不可反驳的 tick 分支永远不会 disable
            // → else 是死代码，pipe 任务一死主循环就变成永久空转的僵尸。
            cmd = cmd_rx.recv() => {
                let cmd = match cmd {
                    Some(c) => c,
                    None => {
                        log::info("Command channel closed, shutting down...");
                        break;
                    }
                };

                let now = now_ms();
                let recv_at = Instant::now();
                last_event_ms = now;
                // 注意：不再重复打印 Received command（pipe.rs 已打印 Parsed command），
                // 每条日志都是一次文件写，省下来就是端到端延迟。


                // 处理特殊停止命令（`cursorlight stop` / setup 脚本）
                if matches!(cmd, DaemonCmd::Stop { ref status } if status == "stop") {
                    log::info("Received stop signal, shutting down...");
                    break;
                }

                match cmd {
                    // 存活探针：只记日志，不计数、不改状态
                    DaemonCmd::Ping => {
                        log::info("Ping (liveness probe)");
                        continue;
                    }

                    // 生命周期管理：按 session_id 去重计数
                    //
                    // 计数只由真正的 Hook 事件驱动。
                    // 旧实现有两个致命问题：
                    //   1. hook.bat 用 session_start 当存活探针 → 每次 hook 调用都 +1，
                    //      计数永远归不了零，daemon 永不退出（灯永远停在最后一个状态）；
                    //   2. 计数是"裸加减"，任何一条多余的 session_end 都会让计数归零，
                    //      daemon 在会话中途自杀。
                    DaemonCmd::SessionStart { session_id } => {
                        saw_session_start = true;
                        // 本次 daemon 的第一个会话 → 先复位到绿灯再计数，
                        // 保证"Claude 一开灯就是干净的 green"，而不是上次会话残留的模式。
                        let first_session = sessions.is_empty() && anonymous_sessions == 0;
                        if session_id.is_empty() {
                            anonymous_sessions = anonymous_sessions.saturating_add(1);
                            log::info(&format!(
                                "Session started (anonymous, active={})",
                                anonymous_sessions
                            ));
                        } else if sessions.insert(session_id.clone()) {
                            log::info(&format!(
                                "Session started (id={}, active={})",
                                short_id(&session_id),
                                sessions.len()
                            ));
                        } else {
                            log::info(&format!(
                                "Session already active (id={}), ignored",
                                short_id(&session_id)
                            ));
                        }

                        if first_session {
                            if let Some(mode) =
                                state::process_cmd(&mut daemon_state, DaemonCmd::Idle, now)
                            {
                                last_resolved_mode = Some(mode);
                                let _ = mode_tx.send((mode, recv_at));
                                write_state_json(&daemon_state, mode);
                                log::info(&format!(
                                    "Session start -> light reset to {} (state file synced)",
                                    mode
                                ));
                            }
                        }
                        continue;
                    }
                    DaemonCmd::SessionEnd { session_id } => {
                        // 没见过任何 SessionStart 的 SessionEnd 一律忽略，
                        // 否则一次多余事件就会让 daemon 中途自杀
                        if !saw_session_start {
                            log::warn("SessionEnd without any SessionStart, ignored");
                            continue;
                        }

                        if session_id.is_empty() {
                            anonymous_sessions = anonymous_sessions.saturating_sub(1);
                        } else if !sessions.remove(&session_id) {
                            log::warn(&format!(
                                "SessionEnd for unknown session (id={}), ignored",
                                short_id(&session_id)
                            ));
                        }

                        let active = sessions.len() + anonymous_sessions as usize;
                        log::info(&format!(
                            "Session ended (id={}, active={})",
                            short_id(&session_id),
                            active
                        ));

                        // 会话结束 → 复位状态机并点绿灯（原 idle 的语义）
                        if let Some(mode) =
                            state::process_cmd(&mut daemon_state, DaemonCmd::Idle, now)
                        {
                            last_resolved_mode = Some(mode);
                            // 先推给 BLE，再写状态文件：文件 I/O 不占关键路径
                            let _ = mode_tx.send((mode, recv_at));
                            write_state_json(&daemon_state, mode);
                        }
                        kill_desktop_dots();

                        if active == 0 {
                            // daemon 常驻：绿灯由 BLE 任务异步送达，断连时会自动重试，
                            // 不会像旧实现那样"发完就退出"导致绿灯丢失。
                            // 常驻同时让 pipe 与 BLE 保持热连接，下次会话零冷启动延迟。
                            log::info("All sessions ended, light -> green; daemon stays resident");
                        }
                        continue;
                    }

                    // 状态机处理
                    other => {
                        let is_idle = matches!(other, DaemonCmd::Idle);

                        if let Some(mode) = state::process_cmd(&mut daemon_state, other, now) {
                            log::info(&format!("State machine -> mode: {}", mode));
                            last_resolved_mode = Some(mode);
                            // 关键路径优先：先交给 BLE 任务，再落状态文件
                            let _ = mode_tx.send((mode, recv_at));
                            write_state_json(&daemon_state, mode);
                        } else {
                            log::info("State machine: no mode change (force green window)");
                        }

                        // 会话结束：兜底清理桌面圆点（hook.bat 的 py stop 是主路径）
                        if is_idle {
                            kill_desktop_dots();
                        }
                    }
                }
            }

            // 定时检查超时升级（PendingAuth 15s → alarm, AwaitingUser 30s → busy）
            _ = resolve_interval.tick() => {
                let now = now_ms();

                // 僵尸兜底：没有活跃会话且长时间没有任何事件 → 自己退出
                let active = sessions.len() + anonymous_sessions as usize;
                if active == 0 && now.saturating_sub(last_event_ms) > IDLE_EXIT_MS {
                    log::info("No hook event for a long time, shutting down...");
                    break;
                }

                let resolved = state::resolve_mode(&daemon_state, now);

                // 如果超时升级导致模式变化，发送到 BLE 和桌面端
                if last_resolved_mode != Some(resolved) {
                    log::info(&format!("Timeout escalation: {:?} → {}", last_resolved_mode, resolved));
                    last_resolved_mode = Some(resolved);
                    daemon_state.last_mode = resolved;
                    let _ = mode_tx.send((resolved, Instant::now()));
                    write_state_json(&daemon_state, resolved);
                }
            }
        }
    }

    // 清理
    pipe_handle.abort();
    ble_handle.abort();
    log::info("=== CursorLight daemon stopped ===");
    Ok(())
}

/// session_id 的日志缩写（只取前 8 位，避免长 UUID 刷屏）
fn short_id(id: &str) -> String {
    if id.chars().count() <= 8 {
        id.to_string()
    } else {
        id.chars().take(8).collect()
    }
}

/// 当前时间戳（毫秒）
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 获取 state_desktop.json 的固定路径（与 Python 端共享）
///
/// Windows: %LOCALAPPDATA%\CursorLight\state_desktop.json
/// Linux/macOS: ~/.cursorlight/state_desktop.json
fn state_json_path() -> std::path::PathBuf {
    let dir = if cfg!(target_os = "windows") {
        std::env::var("LOCALAPPDATA")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join("CursorLight")
    } else {
        std::env::var("HOME")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|_| std::env::temp_dir())
            .join(".cursorlight")
    };
    // 确保目录存在
    let _ = std::fs::create_dir_all(&dir);
    dir.join("state_desktop.json")
}

/// turn_phase → snake_case 字符串（与 Python 端的判断保持兼容）
fn phase_str(phase: TurnPhase) -> &'static str {
    match phase {
        TurnPhase::Idle => "idle",
        TurnPhase::Thinking => "thinking",
        TurnPhase::Busy => "busy",
        TurnPhase::PendingAuth => "pending_auth",
        TurnPhase::AwaitingUser => "awaiting_user",
    }
}

/// 写 state_desktop.json 供桌面灯读取
fn write_state_json(state: &State, mode: Mode) {
    use std::io::Write;
    let path = state_json_path();

    let json = serde_json::json!({
        "last_mode": mode.as_str(),
        "turn_phase": phase_str(state.turn_phase),
        "awaiting_build": state.awaiting_build,
        "build_started": state.build_started,
        "plan_touched": state.plan_touched,
        "last_ts": state.last_ts,
        "turn_started_ms": state.turn_started_ms,
        "pending_since": state.pending_since,
        "force_green_until": state.force_green_until,
    });

    if let Ok(mut f) = std::fs::File::create(&path) {
        let _ = f.write_all(json.to_string().as_bytes());
        let _ = f.flush();
    }
}

/// 会话结束时尝试终止桌面灯进程。
///
/// 策略：读取 traffic_light_desktop.pid 文件获取 PID，用 taskkill 终止。
/// hook.bat 里的 `py stop` 是主路径，这里是兜底。
fn kill_desktop_dots() {
    // PID 文件路径：与 traffic_light_desktop.py 同目录（安装目录根）
    // bin/cursorlight.exe → bin/ → 安装根
    let pid_path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().and_then(|p| p.parent()).map(|p| p.to_path_buf()))
        .map(|p| p.join("traffic_light_desktop.pid"));

    let pid_path = match pid_path {
        Some(p) => p,
        None => {
            log::warn("Cannot determine PID file path for desktop dots");
            return;
        }
    };

    if !pid_path.exists() {
        log::info("Desktop dots PID file not found, already stopped");
        return;
    }

    let pid_str = match std::fs::read_to_string(&pid_path) {
        Ok(s) => s.trim().to_string(),
        Err(e) => {
            log::warn(&format!("Cannot read PID file: {}", e));
            return;
        }
    };

    let pid: u32 = match pid_str.parse() {
        Ok(p) => p,
        Err(_) => {
            log::warn(&format!("Invalid PID in file: '{}'", pid_str));
            let _ = std::fs::remove_file(&pid_path);
            return;
        }
    };

    log::info(&format!("Killing desktop dots process PID={}", pid));

    #[cfg(windows)]
    {
        use std::process::Command;
        match Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {
                log::info("Desktop dots process terminated");
            }
            Ok(_) => {
                log::info("Desktop dots process not found (already exited)");
            }
            Err(e) => {
                log::warn(&format!("taskkill failed: {}", e));
            }
        }
    }

    // 不主动删除 PID 文件：如果进程其实没被干掉（PID 过期/权限），
    // 删掉文件会让 hook.bat 的 `py stop` 再也找不到目标，
    // 桌面圆点就一直亮着。文件交给 traffic_light_desktop.py 在确认终止后自己清理。
}
