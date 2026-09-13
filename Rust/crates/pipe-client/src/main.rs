// pipe-client — 供 hook.bat / 脚本向 daemon 发送 pipe 消息
//
// 用法：
//   pipe-client <json_message>              — 直接发送命令行里的 JSON
//   pipe-client --stdin                     — 从 stdin 读取 hook JSON 并发送
//   pipe-client --stdin --action pre_tool   — 读取 stdin JSON 并注入 action 字段
//   pipe-client --stdin --action stop --status completed
//                                           — 追加/覆盖 status 字段
//   pipe-client --ping                      — 存活探针（不改变 daemon 任何状态）
//
// V.rs0.4.1:
// 1. 新增 --ping：旧实现用 session_start 当存活探针，
//    导致每次 hook 调用都让 daemon 的会话计数 +1，计数永远归不了零。
// 2. 新增 --status：Stop 事件据此带上真实状态，
//    否则 daemon 只能拿到 "unknown"，永远点绿灯（错误永远不红）。
// 3. stdin 为空时退化为 {} 而不是直接失败（某些 hook 不提供 stdin）。
// 4. 重试次数 3 → 10，覆盖 daemon 断连重建的窗口。

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::thread;
use std::time::{Duration, Instant};

const PIPE_NAME: &str = r"\\.\pipe\cursorlight";
/// 阶段 1：daemon 应该已经在跑，只做短重试（覆盖 disconnect→connect 间隙）
const FAST_RETRIES: u32 = 4;
/// 阶段 2：判定 daemon 没在跑 → 自己把它拉起来，再等 pipe 就绪（约 4.2s 上限）
const COLD_RETRIES: u32 = 35;
/// 重试间隔（毫秒）
const RETRY_DELAY_MS: u64 = 120;

fn main() -> anyhow::Result<()> {
    let t_start = Instant::now();
    let args: Vec<String> = std::env::args().collect();

    let mut use_stdin = false;
    let mut timing = false;
    let mut ping = false;
    let mut action_override: Option<String> = None;
    let mut status_override: Option<String> = None;
    let mut positional: Vec<String> = Vec::new();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--stdin" => use_stdin = true,
            "--ping" => ping = true,
            "--timing" => timing = true,
            "--action" => {
                i += 1;
                match args.get(i) {
                    Some(v) => action_override = Some(v.clone()),
                    None => {
                        eprintln!("--action requires a value");
                        std::process::exit(1);
                    }
                }
            }
            "--status" => {
                i += 1;
                match args.get(i) {
                    Some(v) => status_override = Some(v.clone()),
                    None => {
                        eprintln!("--status requires a value");
                        std::process::exit(1);
                    }
                }
            }
            "--help" | "-h" => {
                print_usage();
                return Ok(());
            }
            "--version" | "-V" => {
                println!("pipe-client {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            other => {
                if other.starts_with("--") {
                    eprintln!("Unknown option: {}", other);
                    print_usage();
                    std::process::exit(2);
                }
                positional.push(other.to_string());
            }
        }
        i += 1;
    }

    // ---- 构造消息体 ----
    let mut msg = if ping {
        // 探针：固定负载，永远不读 stdin，也不带任何状态语义
        r#"{"action":"ping"}"#.to_string()
    } else if use_stdin {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        let trimmed = buf.trim().to_string();
        if trimmed.is_empty() {
            // 没有 stdin 也不能让整条 hook 事件丢掉
            "{}".to_string()
        } else {
            trimmed
        }
    } else if !positional.is_empty() {
        positional[0].clone()
    } else {
        print_usage();
        std::process::exit(1);
    };

    // ---- 注入字段（action / status 覆盖 stdin 里的同名值）----
    if let Some(action) = action_override {
        msg = inject_field(&msg, "action", &action)?;
    }
    if let Some(status) = status_override {
        msg = inject_field(&msg, "status", &status)?;
    }

    // ---- 带重试发送 ----
    let payload = format!("{}\n", msg);
    send_with_retry(&payload)?;

    if timing {
        // 仅用于延迟测量：本进程从启动到写完 pipe 的耗时
        println!("sent in {:.1}ms", t_start.elapsed().as_secs_f64() * 1000.0);
    }

    Ok(())
}

/// 打印用法（stdout）
fn print_usage() {
    println!("Usage: pipe-client <json_message>");
    println!("       pipe-client --stdin [--action <action>] [--status <status>]");
    println!("       pipe-client --ping");
    println!("       pipe-client --timing           # 打印本进程启动到写完 pipe 的耗时");
    println!("       pipe-client --version");
}

/// 向 JSON 里注入或覆盖一个字符串字段。
///
/// stdin 来自 Claude Code hook，包含 tool_name / permission_mode / cwd 等字段，
/// 但没有 action（由 hook.bat 的参数决定），也可能没有 status。
fn inject_field(json: &str, key: &str, value: &str) -> anyhow::Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("Invalid JSON: {}", e))?;

    if let Some(obj) = v.as_object_mut() {
        obj.insert(key.to_string(), serde_json::Value::String(value.to_string()));
    } else {
        return Err(anyhow::anyhow!("JSON payload is not an object"));
    }

    Ok(serde_json::to_string(&v)?)
}

/// 带重试的 Named Pipe 发送，并在必要时自己拉起 daemon。
///
/// 这样 hook 脚本不需要"先探针再发送"（多一次进程启动，实测约 15~20ms），
/// 冷启动路径也不会丢事件：本进程拿着完整载荷等 pipe 就绪后补发。
fn send_with_retry(payload: &str) -> anyhow::Result<()> {
    let mut last_err = None;

    // 阶段 1：热路径，daemon 通常已经在跑
    for _ in 0..FAST_RETRIES {
        match try_send(payload) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
            }
        }
    }

    // 阶段 2：还没成功 → 拉起同目录的 daemon，再等 pipe 就绪
    let spawned = try_start_daemon();
    for _ in 0..COLD_RETRIES {
        match try_send(payload) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!("daemon pipe not reachable (spawn attempted: {})", spawned)
    }))
}

/// 拉起同目录下的 cursorlight.exe（daemon）。
///
/// 优先用 DETACHED_PROCESS + CREATE_BREAKAWAY_FROM_JOB 彻底脱离当前进程树，
/// 避免 hook 进程结束时把 daemon 连带回收；作业对象不允许 breakaway 时退回
/// CREATE_NO_WINDOW（此时由 daemon 的命名互斥体保证不会重复启动）。
fn try_start_daemon() -> bool {
    let daemon = match std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|dir| dir.join("cursorlight.exe")))
    {
        Some(p) => p,
        None => return false,
    };
    if !daemon.exists() {
        return false;
    }

    #[cfg(windows)]
    {
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;

        let detached = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB;
        if spawn_daemon(&daemon, detached) {
            return true;
        }
        return spawn_daemon(&daemon, CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = daemon;
        false
    }
}

#[cfg(windows)]
fn spawn_daemon(daemon: &std::path::Path, flags: u32) -> bool {
    use std::os::windows::process::CommandExt;
    std::process::Command::new(daemon)
        .creation_flags(flags)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .is_ok()
}

/// 单次发送尝试。
fn try_send(payload: &str) -> anyhow::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .open(PIPE_NAME)
        .map_err(|e| anyhow::anyhow!("Cannot connect to daemon pipe '{}': {}", PIPE_NAME, e))?;

    file.write_all(payload.as_bytes())?;
    file.flush()?;
    Ok(())
}
