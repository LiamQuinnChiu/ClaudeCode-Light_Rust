//! 灯效模式枚举与线协议字符串。

/// 灯效模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    pub const ALL: [Mode; 11] = [
        Mode::Demo,
        Mode::Thinking,
        Mode::Ai,
        Mode::Busy,
        Mode::Success,
        Mode::Error,
        Mode::Alarm,
        Mode::Off,
        Mode::Red,
        Mode::Yellow,
        Mode::Green,
    ];

    /// 解析 BLE 写入的字符串，非法值返回 `None`（大小写敏感、不做裁剪）。
    pub fn from_str(s: &str) -> Option<Mode> {
        match s {
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

    pub const fn as_str(self) -> &'static str {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_modes() {
        for m in Mode::ALL {
            assert_eq!(Mode::from_str(m.as_str()), Some(m));
        }
    }

    #[test]
    fn rejects_unknown() {
        assert_eq!(Mode::from_str("blink"), None);
        assert_eq!(Mode::from_str(""), None);
        assert_eq!(Mode::from_str("RED"), None);
        assert_eq!(Mode::from_str(" green"), None);
    }
}
