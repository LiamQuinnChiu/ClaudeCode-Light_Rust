// 日志模块 — 初始化 env_logger + 文件日志
//
// 两条硬约束：
// 1. **绝不 panic**。旧实现用 println!/eprintln!，当 daemon 被无控制台方式拉起、
//    父控制台随后销毁时写 stdout 会失败，宏会 unwrap 这个失败直接 panic；
//    而日志调用在 pipe 任务里，一次 panic 就让 pipe 任务静默死掉 → 僵尸 daemon。
// 2. **不拖慢关键路径**。日志调用位于 hook → LED 的路径上（一条命令 3~4 行），
//    旧实现每行都 open/append/close（3 次系统调用）。这里复用一个文件句柄。

use chrono::Local;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 复用的日志文件句柄（懒打开；写失败就丢，绝不影响主逻辑）
static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

/// 返回日志文件的完整路径（daemon 可执行文件同目录下的 cursorlight_daemon.log）
pub fn log_path() -> PathBuf {
    std::env::current_exe()
        .unwrap_or_else(|_| Path::new(".").to_path_buf())
        .parent()
        .unwrap_or(Path::new("."))
        .join("cursorlight_daemon.log")
}

/// 追加一行到日志文件（复用句柄）
fn append_to_file(line: &str) {
    let mut guard = match LOG_FILE.lock() {
        Ok(g) => g,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.is_none() {
        *guard = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path())
            .ok();
    }
    if let Some(file) = guard.as_mut() {
        let _ = writeln!(file, "{}", line);
    }
}

/// 写一行到 stdout/stderr + 追加到日志文件；任何失败都被忽略
fn emit(line: &str, to_stderr: bool) {
    let _ = if to_stderr {
        writeln!(std::io::stderr(), "{}", line)
    } else {
        writeln!(std::io::stdout(), "{}", line)
    };
    append_to_file(line);
}

/// 写一条 INFO 日志：`[2026-09-12 14:30:00] message`
pub fn log(msg: &str) {
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    emit(&format!("[{}] {}", ts, msg), false);
}

/// 初始化 env_logger（级别默认 info，可用 RUST_LOG 覆盖）
pub fn init_logger() {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_secs()
        .init();
}

/// 初始化日志系统（向后兼容别名）
pub fn init() {
    init_logger();
}

/// 信息日志（stdout + 文件）
pub fn info(msg: &str) {
    log(msg);
}

/// 警告日志（stderr + 文件）
pub fn warn(msg: &str) {
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    emit(&format!("[{}] WARN: {}", ts, msg), true);
}

/// 错误日志（stderr + 文件）
pub fn error(msg: &str) {
    let ts = Local::now().format("%Y-%m-%d %H:%M:%S");
    emit(&format!("[{}] ERROR: {}", ts, msg), true);
}
