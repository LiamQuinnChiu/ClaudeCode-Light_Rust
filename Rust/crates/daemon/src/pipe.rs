// Named Pipe 服务端 — 接收 Claude Code hook 事件
//
// 协议：每条消息是一行 JSON，以 \n 结尾；客户端写完后可立即关闭。
//
// V.rs0.4.1 修复的三件事：
// 1. **组帧**：旧实现只 read 一次 4096 字节就当成完整消息，
//    PostToolUse 里带文件内容的大 JSON（Edit/Write）会被截断成非法 JSON，
//    整条事件丢失 → 状态机永远收不到"工具干完了"，灯卡在 busy/alarm。
//    现在按 \n 组帧、读到 EOF 或超时，上限 1 MB。
// 2. **不死**：旧实现用 `?` 把 pipe 的瞬态错误升级成"任务结束"，
//    而主循环又无法感知（select! 的 else 分支不可达），
//    于是 daemon 变成没有 pipe 的僵尸，还在后台抢 BLE。
//    现在任何错误都只重建实例并继续服务。
// 3. **探针**：新增 ping（不产生任何副作用），供 hook.bat 判断存活。

use crate::types::DaemonCmd;
use std::io;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

/// Named Pipe 路径 — hook 脚本写入同一个路径
pub const PIPE_NAME: &str = r"\\.\pipe\cursorlight";

/// 单条消息上限，防止异常客户端把内存吃满
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// 单次读取块大小
const READ_CHUNK: usize = 16 * 1024;

/// 单次读取等待上限（秒）— 客户端连上却不发数据时不会把服务端卡死
const READ_TIMEOUT_SECS: u64 = 10;

/// 写日志时单条命令的最大长度（避免把 4 KB 的 hook JSON 全塞进日志）
const LOG_PREVIEW_CHARS: usize = 400;

// ---------------------------------------------------------------------------
// 实例创建
// ---------------------------------------------------------------------------

/// 每个 pipe 名的最大实例数。
///
/// MSDN 要求同一 pipe 名的所有实例使用同一个值，所以集中定义在这里。
/// 用 4 而不是 1：始终保留"空闲实例"在监听，pipe 名永不消失。
const MAX_INSTANCES: usize = 4;

/// 创建 pipe 服务端的**第一个**实例。
///
/// `first_pipe_instance(true)` 保证同一时刻只有一个进程能持有该 pipe 名 ——
/// 第二个 daemon 会在这里直接失败，配合 main 里的命名互斥体形成双保险。
pub fn create_server() -> io::Result<NamedPipeServer> {
    ServerOptions::new()
        .first_pipe_instance(true)
        .max_instances(MAX_INSTANCES)
        .create(PIPE_NAME)
}

/// 创建后续（备用）实例。
///
/// 不能带 first_pipe_instance —— 只要已存在实例，带这个标志必然失败。
fn create_spare() -> io::Result<NamedPipeServer> {
    ServerOptions::new()
        .max_instances(MAX_INSTANCES)
        .create(PIPE_NAME)
}

/// 带重试地创建备用实例
async fn create_spare_retry() -> Option<NamedPipeServer> {
    for attempt in 1..=5u32 {
        match create_spare() {
            Ok(s) => return Some(s),
            Err(e) => {
                crate::log::warn(&format!(
                    "Pipe spare create attempt {}/5 failed: {}",
                    attempt, e
                ));
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// pipe_server：主循环，持续接受客户端连接
// ---------------------------------------------------------------------------

/// 启动 Named Pipe 监听循环。
///
/// 实例策略（V.rs0.4.1 重写）：
///   1. 每个客户端用**全新实例**服务，绝不用 disconnect() 复用旧实例；
///   2. 收到连接后立刻补一个空闲实例继续监听 → pipe 名永不消失。
///
/// 旧实现是 `connect → read → disconnect → connect` 复用同一个实例。
/// 实测 20 条消息丢 1 条（客户端 write 返回成功、服务端什么都没收到），
/// 丢的如果恰好是 stop / session_end / post_tool，灯就会停在错误状态。
pub async fn pipe_server(
    first: NamedPipeServer,
    tx: mpsc::Sender<DaemonCmd>,
) -> io::Result<()> {
    crate::log::info(&format!("Named Pipe server listening on {}", PIPE_NAME));
    let mut pending = first;

    loop {
        // ---- 1. 等待客户端连接 ----
        if let Err(e) = pending.connect().await {
            crate::log::warn(&format!("Pipe connect error: {}", e));
            drop(pending);
            match create_spare_retry().await {
                Some(s) => {
                    pending = s;
                    continue;
                }
                None => {
                    return Err(io::Error::other(
                        "named pipe instance could not be recreated",
                    ));
                }
            }
        }

        // ---- 2. 立刻补一个空闲实例继续监听 ----
        // 服务当前客户端期间，新客户端仍能连上并先把消息缓冲在备用实例里，
        // 不会因为"管道忙 / 文件不存在"而丢消息。
        let spare = create_spare_retry().await;

        // ---- 3. 读取并处理这一轮的消息 ----
        match read_messages(&mut pending).await {
            Ok(messages) => {
                for msg in messages {
                    let msg = msg.trim();
                    if msg.is_empty() {
                        continue;
                    }
                    match parse_cmd(msg) {
                        Some(cmd) => {
                            crate::log::info(&format!("Parsed command: {:?}", cmd));
                            if tx.send(cmd).await.is_err() {
                                crate::log::info("Command channel closed, pipe server exiting");
                                return Ok(());
                            }
                        }
                        None => crate::log::warn(&format!(
                            "Failed to parse command ({} bytes): {}",
                            msg.len(),
                            preview(msg)
                        )),
                    }
                }
            }
            Err(e) => crate::log::warn(&format!("Pipe read error: {}", e)),
        }

        // ---- 4. 交接：丢弃已用完的实例，用备用实例继续监听 ----
        drop(pending);
        pending = match spare {
            Some(s) => s,
            None => match create_spare_retry().await {
                Some(s) => s,
                None => {
                    return Err(io::Error::other(
                        "named pipe instance could not be recreated",
                    ));
                }
            },
        };
    }
}

/// 读取一次客户端会话里的所有完整行。
///
/// 读到 \n 即认为一条消息结束；客户端关闭或超时则收尾。
async fn read_messages(server: &mut NamedPipeServer) -> io::Result<Vec<String>> {
    let mut raw: Vec<u8> = Vec::with_capacity(READ_CHUNK);
    let mut chunk = vec![0u8; READ_CHUNK];

    loop {
        let n = match tokio::time::timeout(
            Duration::from_secs(READ_TIMEOUT_SECS),
            server.read(&mut chunk),
        )
        .await
        {
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => {
                crate::log::warn("Pipe read timeout, closing idle client");
                break;
            }
        };

        if n == 0 {
            break; // 客户端已关闭写端
        }

        raw.extend_from_slice(&chunk[..n]);

        if raw.contains(&b'\n') {
            break; // 至少凑齐一条完整消息
        }
        if raw.len() > MAX_MESSAGE_BYTES {
            crate::log::warn(&format!(
                "Pipe message exceeds {} bytes, truncating",
                MAX_MESSAGE_BYTES
            ));
            raw.truncate(MAX_MESSAGE_BYTES);
            break;
        }
    }

    let text = String::from_utf8_lossy(&raw).into_owned();
    Ok(text
        .split('\n')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect())
}

/// 取 hook JSON 里的 session_id（缺失时返回空串 → daemon 走匿名计数回退）
fn session_id_of(v: &serde_json::Value) -> String {
    v.get("session_id")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// 日志预览（截断，避免几 KB 的 hook JSON 刷屏）
fn preview(msg: &str) -> String {
    if msg.chars().count() <= LOG_PREVIEW_CHARS {
        return msg.to_string();
    }
    let mut s: String = msg.chars().take(LOG_PREVIEW_CHARS).collect();
    s.push_str("...<truncated>");
    s
}

// ---------------------------------------------------------------------------
// parse_cmd：JSON → DaemonCmd
// ---------------------------------------------------------------------------

/// 解析一行 JSON 为 `DaemonCmd`。
///
/// hook 脚本发送的 JSON 格式：
/// - `{"action":"thinking"}`
/// - `{"action":"pre_tool","tool_name":"Bash","permission_mode":"auto"}`
/// - `{"action":"post_tool","tool_name":"Bash"}`
/// - `{"action":"stop","status":"completed"}`
/// - `{"action":"idle"}`
/// - `{"action":"error"}`
/// - `{"action":"ping"}`
///
/// 返回 `None` 如果 JSON 格式错误或 action 未知。
fn parse_cmd(json: &str) -> Option<DaemonCmd> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let action = v.get("action")?.as_str()?;

    match action {
        // UserPromptSubmit — 用户发送了新提示
        "thinking" => Some(DaemonCmd::Thinking),

        // PreToolUse — Claude Code 准备调用工具
        //
        // 字段名兼容：Claude Code 实际发的是 `permission_mode`，
        // 旧实现只读 `perm_mode` → 永远取到默认值 "default"，
        // 于是连 Read/Glob 这种免授权工具都会被判成"等授权"点红灯。
        "pre_tool" => {
            let tool_name = v
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let perm_mode = v
                .get("permission_mode")
                .or_else(|| v.get("perm_mode"))
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string();
            Some(DaemonCmd::PreTool {
                tool_name,
                perm_mode,
            })
        }

        // PostToolUse — 工具调用完成
        "post_tool" => {
            let tool_name = v
                .get("tool_name")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            Some(DaemonCmd::PostTool { tool_name })
        }

        // Stop — Claude Code 停止（completed / error / aborted）
        "stop" => {
            let status = v
                .get("status")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            Some(DaemonCmd::Stop { status })
        }

        // SessionEnd — 会话结束，回到空闲
        "idle" => Some(DaemonCmd::Idle),

        // PlanDetect — 检测到 plan 文本
        "plan_detect" => {
            let text = v
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(DaemonCmd::PlanDetect { text })
        }

        // PlanFile — plan 文件路径
        "plan_file" => {
            let path = v
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some(DaemonCmd::PlanFile { path })
        }

        // Denied — 授权被拒绝
        "denied" => {
            let failure_type = v
                .get("failure_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            Some(DaemonCmd::Denied { failure_type })
        }

        // Build — 构建事件
        "build" => Some(DaemonCmd::Build),

        // Alarm — 告警事件（如等待用户回答）
        "alarm" => Some(DaemonCmd::Alarm),

        // Busy — 忙碌状态
        "busy" => Some(DaemonCmd::Busy),

        // Error — 工具失败 / 报错 → 红灯
        "error" => Some(DaemonCmd::Error),

        // 存活探针（不改变任何状态）
        "ping" => Some(DaemonCmd::Ping),

        // 生命周期管理 —— 带上 session_id，daemon 按会话去重计数。
        // 同一个 session 重复收到 SessionStart（如 compact/resume）也只算一次。
        "session_start" => Some(DaemonCmd::SessionStart {
            session_id: session_id_of(&v),
        }),
        "session_end" => Some(DaemonCmd::SessionEnd {
            session_id: session_id_of(&v),
        }),

        // 未知 action
        _ => {
            crate::log::warn(&format!("Unknown action: {}", action));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_thinking() {
        let cmd = parse_cmd(r#"{"action":"thinking"}"#).unwrap();
        assert!(matches!(cmd, DaemonCmd::Thinking));
    }

    #[test]
    fn parse_pre_tool_legacy_perm_mode() {
        let cmd =
            parse_cmd(r#"{"action":"pre_tool","tool_name":"Bash","perm_mode":"auto"}"#).unwrap();
        match cmd {
            DaemonCmd::PreTool {
                tool_name,
                perm_mode,
            } => {
                assert_eq!(tool_name, "Bash");
                assert_eq!(perm_mode, "auto");
            }
            _ => panic!("expected PreTool"),
        }
    }

    #[test]
    fn parse_pre_tool_permission_mode() {
        let cmd = parse_cmd(
            r#"{"action":"pre_tool","tool_name":"Write","permission_mode":"acceptEdits"}"#,
        )
        .unwrap();
        match cmd {
            DaemonCmd::PreTool { perm_mode, .. } => assert_eq!(perm_mode, "acceptEdits"),
            _ => panic!("expected PreTool"),
        }
    }

    #[test]
    fn parse_post_tool() {
        let cmd = parse_cmd(r#"{"action":"post_tool","tool_name":"Write"}"#).unwrap();
        match cmd {
            DaemonCmd::PostTool { tool_name } => assert_eq!(tool_name, "Write"),
            _ => panic!("expected PostTool"),
        }
    }

    #[test]
    fn parse_post_tool_with_large_payload() {
        // 旧实现会在 4 KB 处截断，这条用例保证大 payload 能被解析
        let big = "x".repeat(64 * 1024);
        let json = format!(
            r#"{{"action":"post_tool","tool_name":"Edit","tool_input":{{"new_string":"{}"}}}}"#,
            big
        );
        let cmd = parse_cmd(&json).unwrap();
        match cmd {
            DaemonCmd::PostTool { tool_name } => assert_eq!(tool_name, "Edit"),
            _ => panic!("expected PostTool"),
        }
    }

    #[test]
    fn parse_stop() {
        let cmd = parse_cmd(r#"{"action":"stop","status":"completed"}"#).unwrap();
        match cmd {
            DaemonCmd::Stop { status } => assert_eq!(status, "completed"),
            _ => panic!("expected Stop"),
        }
    }

    #[test]
    fn parse_idle() {
        let cmd = parse_cmd(r#"{"action":"idle"}"#).unwrap();
        assert!(matches!(cmd, DaemonCmd::Idle));
    }

    #[test]
    fn parse_ping() {
        let cmd = parse_cmd(r#"{"action":"ping"}"#).unwrap();
        assert!(matches!(cmd, DaemonCmd::Ping));
    }

    #[test]
    fn parse_error() {
        let cmd = parse_cmd(r#"{"action":"error"}"#).unwrap();
        assert!(matches!(cmd, DaemonCmd::Error));
    }

    #[test]
    fn parse_plan_detect() {
        let cmd = parse_cmd(r#"{"action":"plan_detect","text":"step 1: ..."}"#).unwrap();
        match cmd {
            DaemonCmd::PlanDetect { text } => assert_eq!(text, "step 1: ..."),
            _ => panic!("expected PlanDetect"),
        }
    }

    #[test]
    fn parse_denied() {
        let cmd = parse_cmd(r#"{"action":"denied","failure_type":"permission_denied"}"#).unwrap();
        match cmd {
            DaemonCmd::Denied { failure_type } => assert_eq!(failure_type, "permission_denied"),
            _ => panic!("expected Denied"),
        }
    }

    #[test]
    fn parse_build() {
        let cmd = parse_cmd(r#"{"action":"build"}"#).unwrap();
        assert!(matches!(cmd, DaemonCmd::Build));
    }

    #[test]
    fn parse_alarm() {
        let cmd = parse_cmd(r#"{"action":"alarm"}"#).unwrap();
        assert!(matches!(cmd, DaemonCmd::Alarm));
    }

    #[test]
    fn parse_busy() {
        let cmd = parse_cmd(r#"{"action":"busy"}"#).unwrap();
        assert!(matches!(cmd, DaemonCmd::Busy));
    }

    #[test]
    fn parse_session_lifecycle() {
        match parse_cmd(r#"{"action":"session_start","session_id":"abc"}"#).unwrap() {
            DaemonCmd::SessionStart { session_id } => assert_eq!(session_id, "abc"),
            _ => panic!("expected SessionStart"),
        }
        match parse_cmd(r#"{"action":"session_end","session_id":"abc"}"#).unwrap() {
            DaemonCmd::SessionEnd { session_id } => assert_eq!(session_id, "abc"),
            _ => panic!("expected SessionEnd"),
        }
        // 缺 session_id 也要能解析（走匿名回退）
        match parse_cmd(r#"{"action":"session_start"}"#).unwrap() {
            DaemonCmd::SessionStart { session_id } => assert!(session_id.is_empty()),
            _ => panic!("expected SessionStart"),
        }
    }

    #[test]
    fn parse_unknown_action() {
        assert!(parse_cmd(r#"{"action":"foobar"}"#).is_none());
    }

    #[test]
    fn parse_invalid_json() {
        assert!(parse_cmd("not json").is_none());
    }

    #[test]
    fn parse_missing_action() {
        assert!(parse_cmd(r#"{"foo":"bar"}"#).is_none());
    }

    #[test]
    fn parse_empty_string() {
        assert!(parse_cmd("").is_none());
    }

    #[test]
    fn preview_truncates_long_payload() {
        let long = "y".repeat(5000);
        let p = preview(&long);
        assert!(p.len() < long.len());
        assert!(p.ends_with("<truncated>"));
    }
}
