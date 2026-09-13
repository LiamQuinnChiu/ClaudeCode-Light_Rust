use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 灯效模式 — 与 light-core 的 Mode 对齐，独立定义（daemon 不依赖 firmware）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Demo,
    Thinking,
    Ai,
    Busy,
    Success,
    Error,
    Alarm,
    Off,
    Red,
    Yellow,
    Green,
}

impl Mode {
    /// 从字符串解析（兼容 Python 端的 lowercase 字符串）
    pub fn from_str(s: &str) -> Option<Mode> {
        match s.to_ascii_lowercase().as_str() {
            "demo" => Some(Mode::Demo),
            "thinking" => Some(Mode::Thinking),
            "ai" => Some(Mode::Ai),
            "busy" => Some(Mode::Busy),
            "success" => Some(Mode::Success),
            "error" => Some(Mode::Error),
            "alarm" => Some(Mode::Alarm),
            "off" => Some(Mode::Off),
            "red" => Some(Mode::Red),
            "yellow" => Some(Mode::Yellow),
            "green" => Some(Mode::Green),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Demo => "demo",
            Mode::Thinking => "thinking",
            Mode::Ai => "ai",
            Mode::Busy => "busy",
            Mode::Success => "success",
            Mode::Error => "error",
            Mode::Alarm => "alarm",
            Mode::Off => "off",
            Mode::Red => "red",
            Mode::Yellow => "yellow",
            Mode::Green => "green",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// 状态机中的 turn 阶段
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Idle,
    Thinking,
    Busy,
    /// PreToolUse 等待授权
    PendingAuth,
    /// alarm 等待用户回答
    AwaitingUser,
}

// ---------------------------------------------------------------------------
// 状态文件结构 — 与 Python 端 state_desktop.json 完全兼容
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct State {
    pub last_mode: Mode,
    pub turn_phase: TurnPhase,
    /// epoch ms
    pub last_ts: u64,
    pub turn_started_ms: u64,
    pub pending_since: u64,
    /// ESC 后 2s 绿灯保护
    pub force_green_until: u64,
    pub awaiting_build: bool,
    pub build_started: bool,
    pub plan_touched: bool,
}

impl Default for State {
    fn default() -> Self {
        Self {
            last_mode: Mode::Off,
            turn_phase: TurnPhase::Idle,
            last_ts: 0,
            turn_started_ms: 0,
            pending_since: 0,
            force_green_until: 0,
            awaiting_build: false,
            build_started: false,
            plan_touched: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Daemon 命令 — 从 pipe 收到的 hook 事件
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub enum DaemonCmd {
    /// UserPromptSubmit
    Thinking,
    /// PreToolUse — 等待授权
    PreTool {
        tool_name: String,
        perm_mode: String,
    },
    /// PostToolUse
    PostTool {
        tool_name: String,
    },
    /// completed / error / aborted
    Stop {
        status: String,
    },
    /// SessionEnd
    Idle,
    /// 检测到 plan 文本
    PlanDetect {
        text: String,
    },
    /// plan 文件路径
    PlanFile {
        path: String,
    },
    /// 授权被拒绝
    Denied {
        failure_type: String,
    },
    /// build 事件
    Build,
    /// alarm 事件
    Alarm,
    /// busy 事件
    Busy,
    /// 工具失败 / 报错 → 红灯
    Error,
    /// 存活探针（hook.bat 用来判断 daemon 是否在跑，不改变任何状态）
    Ping,
    /// 会话启动（生命周期管理，按 session_id 去重计数）
    SessionStart { session_id: String },
    /// 会话结束（生命周期管理，活跃会话清空时 daemon 自动退出）
    SessionEnd { session_id: String },
}

// ---------------------------------------------------------------------------
// BLE 发送结果
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum BleResult {
    Ok,
    Disconnected,
    Error(String),
}
