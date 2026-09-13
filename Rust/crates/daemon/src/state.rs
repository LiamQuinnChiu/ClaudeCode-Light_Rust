// 状态机模块 — 处理 DaemonCmd，维护 State，决定灯效模式切换
//
// 从 cc_state_bridge.py / ble_daemon.py 移植，用 Rust 重写。
// 核心职责：
// 1. 接收 DaemonCmd，根据当前 State + TurnPhase 决定下一状态
// 2. 生成需要发送给 BLE 的 Mode 指令

use crate::types::{DaemonCmd, Mode, State, TurnPhase};

// ---------------------------------------------------------------------------
// 状态机配置常量
// ---------------------------------------------------------------------------

/// pending 超时升级 alarm 的时间（毫秒）
pub const PENDING_ESCALATE_MS: u64 = 15_000;

/// awaiting_user 超时回落的时间（毫秒）
pub const ALARM_TIMEOUT_MS: u64 = 30_000;

/// ESC 后绿灯保护窗口（毫秒）
pub const FORCE_GREEN_DURATION_MS: u64 = 2_000;

// ---------------------------------------------------------------------------
// 工具名集合常量
// ---------------------------------------------------------------------------

/// PreToolUse 阶段直接触发 alarm 的工具
const ALARM_PRE_TOOLS: &[&str] = &["AskUserQuestion"];

/// 需要授权弹窗的工具（permission_mode 不是预授权时进入 alarm pending）
///
/// 注意：Read / Glob / Grep 已移除 —— 它们不弹授权，
/// 之前因为字段名不匹配（perm_mode vs permission_mode）全部被判成"等授权"，
/// 结果每次读文件都闪红色警灯。
const PENDING_TOOLS: &[&str] = &[
    "Bash",
    "Write",
    "Edit",
    "WebFetch",
    "WebSearch",
    "NotebookEdit",
    "Agent",
    "Task",
];

/// 预授权（不会弹窗）的 permission_mode 取值
fn is_auto_perm(perm_mode: &str) -> bool {
    matches!(
        perm_mode.to_ascii_lowercase().as_str(),
        "auto" | "bypasspermissions" | "bypass_permissions" | "acceptedits" | "acceptall"
            | "accept_all" | "yolo" | "dontask" | "never"
    )
}

/// PostToolUse 阶段触发 alarm 的工具
const ALARM_POST_TOOLS: &[&str] = &["CreatePlan", "EnterPlanMode"];

/// PostToolUse 阶段清除 plan 标记的工具
const PLAN_CLEAR_TOOLS: &[&str] = &["ExitPlanMode"];

// ---------------------------------------------------------------------------
// 计划文件后缀
// ---------------------------------------------------------------------------

const PLAN_FILE_EXTENSIONS: &[&str] = &[".md", ".txt", ".plan"];

// ---------------------------------------------------------------------------
// 公开 API
// ---------------------------------------------------------------------------

/// 创建默认初始状态
pub fn default_state() -> State {
    State::default()
}

/// 处理 hook 命令，返回应该发送给 BLE 的模式。
///
/// 核心逻辑，从 cc_state_bridge.py 的各个 ACTION 分支移植。
/// 返回 `None` 表示本次命令不需要更新 BLE（例如绿灯保护期内）。
pub fn process_cmd(state: &mut State, cmd: DaemonCmd, now_ms: u64) -> Option<Mode> {
    state.last_ts = now_ms;

    match cmd {
        // -----------------------------------------------------------------
        // 1. Thinking — 重置 turn 状态
        // -----------------------------------------------------------------
        DaemonCmd::Thinking => {
            state.turn_phase = TurnPhase::Thinking;
            state.last_mode = Mode::Thinking;
            state.turn_started_ms = now_ms;
            // 新一轮提问开始 → 上一次中断留下的绿灯保护窗立即失效
            state.force_green_until = 0;
            state.pending_since = 0;
            state.awaiting_build = false;
            state.build_started = false;
            state.plan_touched = false;
            Some(Mode::Thinking)
        }

        // -----------------------------------------------------------------
        // 2. PreTool — 等待授权
        // -----------------------------------------------------------------
        DaemonCmd::PreTool {
            tool_name,
            perm_mode,
        } => {
            // 绿灯保护期内不更新
            if is_in_force_green(state, now_ms) {
                return None;
            }

            if ALARM_PRE_TOOLS.contains(&tool_name.as_str()) {
                // AskUserQuestion → alarm, 进入 AwaitingUser
                state.turn_phase = TurnPhase::AwaitingUser;
                state.last_mode = Mode::Alarm;
                state.pending_since = now_ms;
                Some(Mode::Alarm)
            } else if PENDING_TOOLS.contains(&tool_name.as_str()) {
                if is_auto_perm(&perm_mode) {
                    // 预授权不 alarm，直接 busy
                    state.last_mode = Mode::Busy;
                    Some(Mode::Busy)
                } else {
                    // 需要授权 → alarm, 进入 PendingAuth
                    state.turn_phase = TurnPhase::PendingAuth;
                    state.last_mode = Mode::Alarm;
                    state.pending_since = now_ms;
                    Some(Mode::Alarm)
                }
            } else {
                // 其他工具 → busy
                state.last_mode = Mode::Busy;
                Some(Mode::Busy)
            }
        }

        // -----------------------------------------------------------------
        // 3. PostTool — 工具执行完成
        // -----------------------------------------------------------------
        DaemonCmd::PostTool { tool_name } => {
            if ALARM_POST_TOOLS.contains(&tool_name.as_str()) {
                // CreatePlan / EnterPlanMode → alarm
                state.plan_touched = true;
                state.last_mode = Mode::Alarm;
                Some(Mode::Alarm)
            } else if PLAN_CLEAR_TOOLS.contains(&tool_name.as_str()) {
                // ExitPlanMode → busy, 清除 plan 标记
                state.plan_touched = false;
                state.last_mode = Mode::Busy;
                Some(Mode::Busy)
            } else if tool_name == "AskUserQuestion" {
                // AskUserQuestion PostTool → 如果 phase==AwaitingUser 则 busy
                if state.turn_phase == TurnPhase::AwaitingUser {
                    state.turn_phase = TurnPhase::Busy;
                    state.last_mode = Mode::Busy;
                    Some(Mode::Busy)
                } else {
                    Some(state.last_mode)
                }
            } else if PENDING_TOOLS.contains(&tool_name.as_str()) {
                // Pending tools 的 PostTool → 如果 phase==AwaitingUser 或 PendingAuth → busy
                if state.turn_phase == TurnPhase::AwaitingUser
                    || state.turn_phase == TurnPhase::PendingAuth
                {
                    state.turn_phase = TurnPhase::Busy;
                    state.last_mode = Mode::Busy;
                    Some(Mode::Busy)
                } else {
                    Some(state.last_mode)
                }
            } else {
                // 其他工具完成后保持当前模式
                Some(state.last_mode)
            }
        }

        // -----------------------------------------------------------------
        // 4. Stop — turn 结束
        // -----------------------------------------------------------------
        DaemonCmd::Stop { status } => {
            let mode = match status.as_str() {
                "completed" => {
                    if state.build_started {
                        // 有 build 完成 → success
                        Mode::Success
                    } else if state.awaiting_build || state.plan_touched {
                        // 还在等 build 或 plan 还在 → alarm
                        Mode::Alarm
                    } else {
                        Mode::Success
                    }
                }
                "error" => Mode::Error,
                // 中断 / 未知（ESC、hook 没带 status）→ 绿灯
                _ => Mode::Green,
            };

            // 绿灯保护窗口：
            // - 结果本身就是绿灯（中断 / 未知）时打开 2s 保护窗；
            // - 结果是 success / error 时**清掉**上一次残留的保护窗，
            //   否则新结果会在 1 秒后被 resolve_mode() 判成 green 覆盖掉。
            state.force_green_until = if mode == Mode::Green {
                now_ms + FORCE_GREEN_DURATION_MS
            } else {
                0
            };

            state.last_mode = mode;
            state.turn_phase = TurnPhase::Idle;
            Some(mode)
        }

        // -----------------------------------------------------------------
        // 5. Idle — session 结束
        // -----------------------------------------------------------------
        DaemonCmd::Idle => {
            state.last_mode = Mode::Green;
            state.turn_phase = TurnPhase::Idle;
            state.pending_since = 0;
            state.awaiting_build = false;
            state.build_started = false;
            state.plan_touched = false;
            state.force_green_until = 0;
            Some(Mode::Green)
        }

        // -----------------------------------------------------------------
        // 6. PlanDetect — 检测到 plan 等待模式
        // -----------------------------------------------------------------
        DaemonCmd::PlanDetect { text } => {
            if is_plan_waiting_text(&text) {
                state.plan_touched = true;
                state.last_mode = Mode::Alarm;
                Some(Mode::Alarm)
            } else {
                Some(state.last_mode)
            }
        }

        // -----------------------------------------------------------------
        // 7. PlanFile — plan 文件路径
        // -----------------------------------------------------------------
        DaemonCmd::PlanFile { path } => {
            if is_plan_file_path(&path) {
                state.plan_touched = true;
                state.last_mode = Mode::Alarm;
                Some(Mode::Alarm)
            } else {
                Some(state.last_mode)
            }
        }

        // -----------------------------------------------------------------
        // 8. Denied — 授权被拒绝
        // -----------------------------------------------------------------
        DaemonCmd::Denied { failure_type } => {
            if failure_type == "permission_denied" {
                state.last_mode = Mode::Thinking;
                Some(Mode::Thinking)
            } else {
                state.last_mode = Mode::Error;
                Some(Mode::Error)
            }
        }

        // -----------------------------------------------------------------
        // 9. Build — build 事件
        // -----------------------------------------------------------------
        DaemonCmd::Build => {
            state.build_started = true;
            state.last_mode = Mode::Busy;
            Some(Mode::Busy)
        }

        // -----------------------------------------------------------------
        // 10. Alarm — 强制 alarm
        // -----------------------------------------------------------------
        DaemonCmd::Alarm => {
            state.last_mode = Mode::Alarm;
            Some(Mode::Alarm)
        }

        // -----------------------------------------------------------------
        // 11. Busy — 如果不在 alarm 则 busy
        // -----------------------------------------------------------------
        DaemonCmd::Busy => {
            if state.last_mode != Mode::Alarm {
                state.last_mode = Mode::Busy;
                Some(Mode::Busy)
            } else {
                Some(Mode::Alarm)
            }
        }

        // -----------------------------------------------------------------
        // 12. Error — 工具失败 / 报错 → 红灯
        // -----------------------------------------------------------------
        DaemonCmd::Error => {
            state.turn_phase = TurnPhase::Busy;
            state.last_mode = Mode::Error;
            Some(Mode::Error)
        }

        // 生命周期与探针由 main.rs 处理，不会到达这里
        DaemonCmd::SessionStart { .. } | DaemonCmd::SessionEnd { .. } | DaemonCmd::Ping => None,
    }
}

/// 根据状态解析当前应该显示的灯效模式。
///
/// 从 ble_daemon.py 的 resolve_mode() 移植。
/// 主要处理超时升级/回落逻辑，不修改状态。
pub fn resolve_mode(state: &State, now_ms: u64) -> Mode {
    // 1. 绿灯保护期内直接返回 green
    if state.force_green_until > now_ms {
        return Mode::Green;
    }

    match state.turn_phase {
        // 2. PendingAuth — 等待授权
        TurnPhase::PendingAuth => {
            if state.pending_since > 0 && now_ms.saturating_sub(state.pending_since) >= PENDING_ESCALATE_MS
            {
                // 超过 15s 升级为 alarm
                Mode::Alarm
            } else {
                // 保持 last_mode（通常是 alarm）
                state.last_mode
            }
        }

        // 3. AwaitingUser — 等待用户回答
        TurnPhase::AwaitingUser => {
            if state.pending_since > 0
                && now_ms.saturating_sub(state.pending_since) >= ALARM_TIMEOUT_MS
            {
                // 超过 30s 回落到 busy
                Mode::Busy
            } else {
                Mode::Alarm
            }
        }

        // 4. 其他 phase
        _ => {
            if state.awaiting_build {
                // 等待 build → alarm
                Mode::Alarm
            } else {
                // 返回 last_mode，如果为 Off 则默认 green
                match state.last_mode {
                    Mode::Off => Mode::Green,
                    other => other,
                }
            }
        }
    }
}

/// 检查是否为 plan 文件路径（基于后缀名判断）
fn is_plan_file_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    PLAN_FILE_EXTENSIONS
        .iter()
        .any(|ext| lower.ends_with(ext))
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 是否处于绿灯保护期内
fn is_in_force_green(state: &State, now_ms: u64) -> bool {
    state.force_green_until > now_ms
}

/// 检测 plan 等待文本（plan 需要用户确认的提示）
fn is_plan_waiting_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("waiting for plan")
        || lower.contains("plan approval")
        || lower.contains("plan review")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh() -> State {
        State::default()
    }

    #[test]
    fn stop_completed_does_not_open_green_window() {
        let mut st = fresh();
        let m = process_cmd(
            &mut st,
            DaemonCmd::Stop {
                status: "completed".into(),
            },
            1_000,
        )
        .unwrap();
        assert_eq!(m, Mode::Success);
        assert_eq!(st.force_green_until, 0);
        // 1 秒后仍然是 success —— 旧实现这里会被绿灯保护窗覆盖成 green
        assert_eq!(resolve_mode(&st, 2_000), Mode::Success);
    }

    #[test]
    fn stop_error_keeps_error() {
        let mut st = fresh();
        process_cmd(
            &mut st,
            DaemonCmd::Stop {
                status: "error".into(),
            },
            1_000,
        );
        assert_eq!(resolve_mode(&st, 5_000), Mode::Error);
    }

    #[test]
    fn stop_aborted_opens_green_window() {
        let mut st = fresh();
        let m = process_cmd(
            &mut st,
            DaemonCmd::Stop {
                status: "aborted".into(),
            },
            1_000,
        )
        .unwrap();
        assert_eq!(m, Mode::Green);
        assert!(st.force_green_until > 1_000);
        assert_eq!(resolve_mode(&st, 1_500), Mode::Green);
        assert!(process_cmd(
            &mut st,
            DaemonCmd::PreTool {
                tool_name: "Bash".into(),
                perm_mode: "default".into(),
            },
            1_600
        )
        .is_none());
    }

    #[test]
    fn new_result_clears_stale_green_window() {
        let mut st = fresh();
        process_cmd(
            &mut st,
            DaemonCmd::Stop {
                status: "aborted".into(),
            },
            1_000,
        );
        process_cmd(
            &mut st,
            DaemonCmd::Stop {
                status: "completed".into(),
            },
            1_200,
        );
        assert_eq!(st.force_green_until, 0);
        assert_eq!(resolve_mode(&st, 1_500), Mode::Success);
    }

    #[test]
    fn auto_permission_modes_are_busy_not_alarm() {
        let mut st = fresh();
        for perm in ["auto", "acceptEdits", "bypassPermissions"] {
            let m = process_cmd(
                &mut st,
                DaemonCmd::PreTool {
                    tool_name: "Bash".into(),
                    perm_mode: perm.into(),
                },
                0,
            )
            .unwrap();
            assert_eq!(m, Mode::Busy, "perm={}", perm);
        }
    }

    #[test]
    fn read_only_tools_never_alarm() {
        let mut st = fresh();
        for tool in ["Read", "Glob", "Grep"] {
            let m = process_cmd(
                &mut st,
                DaemonCmd::PreTool {
                    tool_name: tool.into(),
                    perm_mode: "default".into(),
                },
                0,
            )
            .unwrap();
            assert_eq!(m, Mode::Busy, "tool={}", tool);
        }
        let m = process_cmd(
            &mut st,
            DaemonCmd::PreTool {
                tool_name: "Bash".into(),
                perm_mode: "default".into(),
            },
            0,
        )
        .unwrap();
        assert_eq!(m, Mode::Alarm);
    }

    #[test]
    fn error_action_is_red() {
        let mut st = fresh();
        let m = process_cmd(&mut st, DaemonCmd::Error, 0).unwrap();
        assert_eq!(m, Mode::Error);
        assert_eq!(resolve_mode(&st, 10), Mode::Error);
    }

    #[test]
    fn idle_resets_to_green() {
        let mut st = fresh();
        process_cmd(&mut st, DaemonCmd::Alarm, 0);
        let m = process_cmd(&mut st, DaemonCmd::Idle, 1).unwrap();
        assert_eq!(m, Mode::Green);
        assert_eq!(st.turn_phase, TurnPhase::Idle);
        assert_eq!(st.force_green_until, 0);
    }

    #[test]
    fn session_and_ping_commands_are_ignored_by_state_machine() {
        let mut st = fresh();
        assert!(process_cmd(&mut st, DaemonCmd::Ping, 0).is_none());
        assert!(process_cmd(
            &mut st,
            DaemonCmd::SessionStart {
                session_id: "x".into()
            },
            0
        )
        .is_none());
        assert!(process_cmd(
            &mut st,
            DaemonCmd::SessionEnd {
                session_id: "x".into()
            },
            0
        )
        .is_none());
    }
}

