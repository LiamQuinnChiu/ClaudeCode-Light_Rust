// 单实例保护 — Windows 命名互斥体
//
// 为什么需要它：
//   hook.bat 的存活探针一旦误判（pipe 正在 disconnect→connect 的间隙、
//   客户端被沙箱/权限拒绝等），就会再拉起一个 daemon。
//   两个 daemon 会同时抢同一条 BLE 链路并各自维护一份状态机，
//   直接导致灯效状态不同步。命名互斥体是跨进程、内核级、无竞态的唯一性保证。
//
// 命名空间用 Local\\（当前会话）而不是 Global\\（全局），
// 后者在非管理员/受限令牌下可能因缺少 SeCreateGlobalPrivilege 而失败。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

type Handle = *mut core::ffi::c_void;

/// 互斥体名字（同一 Windows 会话内唯一）
pub const SINGLETON_NAME: &str = "Local\\CursorLightDaemon";

const ERROR_ALREADY_EXISTS: u32 = 183;

unsafe extern "system" {
    fn CreateMutexW(
        attrs: *mut core::ffi::c_void,
        initial_owner: i32,
        name: *const u16,
    ) -> Handle;
    fn CloseHandle(handle: Handle) -> i32;
    fn GetLastError() -> u32;
}

/// 获取结果：拿到锁 / 已有实例 / 无法判断
pub enum InstanceGuard {
    Acquired(SingleInstance),
    AlreadyRunning,
    Unavailable(u32),
}

/// 命名互斥体守卫，Drop 时自动释放
pub struct SingleInstance {
    handle: Handle,
}

// 句柄本身可在进程内跨线程传递
unsafe impl Send for SingleInstance {}
unsafe impl Sync for SingleInstance {}

impl SingleInstance {
    /// 尝试独占该名字。
    fn try_acquire(name: &str) -> InstanceGuard {
        let wide: Vec<u16> = OsStr::new(name)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let handle = unsafe { CreateMutexW(ptr::null_mut(), 0, wide.as_ptr()) };
        if handle.is_null() {
            return InstanceGuard::Unavailable(unsafe { GetLastError() });
        }

        let err = unsafe { GetLastError() };
        if err == ERROR_ALREADY_EXISTS {
            // 已存在同名的互斥体 → 别的 daemon 活着
            unsafe { CloseHandle(handle) };
            return InstanceGuard::AlreadyRunning;
        }

        InstanceGuard::Acquired(SingleInstance { handle })
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// 便捷入口：返回 guard 枚举
pub fn acquire(name: &str) -> InstanceGuard {
    SingleInstance::try_acquire(name)
}
