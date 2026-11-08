//! Per-Monitor V2 DPI 感知（t2 任务 1，spec D9）。
//!
//! 刻意**不**在 Cargo 里开 `Win32_UI_HiDpi`：三个 API 全部用 `LoadLibrary` + `GetProcAddress`
//! 动态解析，从而既能实现「Per-Monitor V2 → Per-Monitor → System」的回落链，
//! 也不会因为目标系统缺少某个符号而让进程启动失败。

use std::ffi::c_void;
use std::sync::OnceLock;

use windows::core::{w, PCSTR, PCWSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

/// 进程 DPI 感知级别（用于日志与校准提示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DpiAwareness {
    /// 每个显示器单独感知（V2，最佳）。
    PerMonitorV2,
    /// 每个显示器单独感知（V1）。
    PerMonitor,
    /// 系统级感知（老系统回落）。
    System,
    /// 三个 API 都失败（例如 manifest 已声明感知级别），沿用现状。
    Unchanged,
}

type SetDpiAwarenessContextFn = unsafe extern "system" fn(*mut c_void) -> i32;
type SetDpiAwarenessFn = unsafe extern "system" fn(i32) -> i32;
type SetDpiAwareFn = unsafe extern "system" fn() -> i32;

/// `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2`
const PER_MONITOR_AWARE_V2: isize = -4;
/// `PROCESS_PER_MONITOR_DPI_AWARE`
const PROCESS_PER_MONITOR_DPI_AWARE: i32 = 2;

static AWARENESS: OnceLock<DpiAwareness> = OnceLock::new();

/// 在**任何** `GetDC` 之前调用；幂等（`OnceLock` 保证只真正设置一次）。
pub fn ensure_dpi_awareness() -> DpiAwareness {
    *AWARENESS.get_or_init(|| unsafe { apply() })
}

unsafe fn apply() -> DpiAwareness {
    if let Some(set_context) =
        resolve::<SetDpiAwarenessContextFn>(w!("user32.dll"), b"SetProcessDpiAwarenessContext\0")
    {
        if set_context(PER_MONITOR_AWARE_V2 as *mut c_void) != 0 {
            return DpiAwareness::PerMonitorV2;
        }
    }
    if let Some(set_awareness) =
        resolve::<SetDpiAwarenessFn>(w!("shcore.dll"), b"SetProcessDpiAwareness\0")
    {
        if set_awareness(PROCESS_PER_MONITOR_DPI_AWARE) == 0 {
            return DpiAwareness::PerMonitor;
        }
    }
    if let Some(set_aware) = resolve::<SetDpiAwareFn>(w!("user32.dll"), b"SetProcessDPIAware\0") {
        if set_aware() != 0 {
            return DpiAwareness::System;
        }
    }
    DpiAwareness::Unchanged
}

/// 取符号地址并转成目标函数指针；任一环节缺失都返回 `None`（交由下一级回落）。
unsafe fn resolve<T: Copy>(library: PCWSTR, symbol: &'static [u8]) -> Option<T> {
    let module: HMODULE = LoadLibraryW(library).ok()?;
    let address = GetProcAddress(module, PCSTR(symbol.as_ptr()))?;
    Some(std::mem::transmute_copy(&address))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awareness_is_idempotent() {
        let first = ensure_dpi_awareness();
        let second = ensure_dpi_awareness();
        assert_eq!(first, second);
        assert!(matches!(
            first,
            DpiAwareness::PerMonitorV2
                | DpiAwareness::PerMonitor
                | DpiAwareness::System
                | DpiAwareness::Unchanged
        ));
    }
}
