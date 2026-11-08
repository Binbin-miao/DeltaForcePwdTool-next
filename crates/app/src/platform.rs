//! 平台贴边层：单实例互斥量、找窗激活、热键名解析。
//!
//! 所有 `unsafe` Win32 调用都收敛在本文件，业务层只看到 `Result<(), String>`。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use delta_hotkey::Hotkey;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, BOOL, ERROR_ALREADY_EXISTS, HWND, LPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SetForegroundWindow, ShowWindow, SW_RESTORE,
};

/// 单实例互斥量名（`Local\` 前缀 = 仅本会话，不跨用户）。
pub const INSTANCE_MUTEX: &str = "Local\\delta-pwd-tool-single-instance";

/// spec §2.2 不变量 8：找窗标题白名单，按顺序试。
pub const GAME_TITLES: [&str; 3] = ["三角洲", "Delta Force", "游戏"];

/// 转 UTF-16 并补 NUL。
fn wide(text: &str) -> Vec<u16> {
    OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

/// 单实例守卫：持有互斥量句柄直到进程退出。
pub struct SingleInstance {
    _handle: windows::Win32::Foundation::HANDLE,
}

/// 尝试取得单实例；返回 `None` 表示已有实例在运行。
pub fn acquire_instance() -> Option<SingleInstance> {
    let name = wide(INSTANCE_MUTEX);
    let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.ok()?;
    let already_exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_exists {
        return None;
    }
    Some(SingleInstance { _handle: handle })
}

/// 枚举顶层窗口，返回 `(窗口句柄, 标题)`；跳过本进程窗口（自己的标题里也含「三角洲」）。
fn top_level_windows() -> Vec<(HWND, String)> {
    let mut found: Vec<(HWND, String)> = Vec::new();
    let context = &mut found as *mut Vec<(HWND, String)> as isize;
    unsafe {
        let _ = EnumWindows(Some(collect_window), LPARAM(context));
    }
    found
}

/// `EnumWindows` 回调：收集可见且属于别的进程的窗口标题。
unsafe extern "system" fn collect_window(window: HWND, lparam: LPARAM) -> BOOL {
    let sink = &mut *(lparam.0 as *mut Vec<(HWND, String)>);
    if !IsWindowVisible(window).as_bool() {
        return BOOL(1);
    }
    let mut holder = [0u16; 512];
    let length = GetWindowTextW(window, &mut holder);
    if length <= 0 {
        return BOOL(1);
    }
    let title = String::from_utf16_lossy(&holder[..length as usize]);
    if title.is_empty() {
        return BOOL(1);
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(window, Some(&mut pid));
    if pid == std::process::id() {
        return BOOL(1);
    }
    sink.push((window, title));
    BOOL(1)
}

/// 按标题片段找第一个窗口。
pub fn find_window(title_part: &str) -> Option<HWND> {
    top_level_windows()
        .into_iter()
        .find(|(_, title)| title.contains(title_part))
        .map(|(window, _)| window)
}

/// 把窗口提到前台：最小化先还原，再 `SetForegroundWindow`。
fn bring_to_front(window: HWND) -> bool {
    unsafe {
        if IsIconic(window).as_bool() {
            let _ = ShowWindow(window, SW_RESTORE);
        }
        SetForegroundWindow(window).as_bool()
    }
}

/// spec §2.2 不变量 8：任何截图/点击之前必须先激活游戏窗口（点击是全局光标事件）。
pub fn focus_game_window() -> Result<String, String> {
    for title in GAME_TITLES {
        if let Some(window) = find_window(title) {
            if bring_to_front(window) {
                return Ok(title.to_owned());
            }
            return Err("游戏窗口无法置前".to_owned());
        }
    }
    Err("检查游戏".to_owned())
}

/// 激活已存在的本程序窗口（第二实例启动时用）。
///
/// 重试 2s：第一个实例可能刚建窗、标题还没写入，一次性查找会漏掉。
pub fn activate_existing_app(title: &str) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(window) = find_window(title) {
            return bring_to_front(window);
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(80));
    }
}

/// 热键名 → `Hotkey`。支持 `F1..F24`、`A..Z`、`0..9`（大小写不敏感）。
///
/// 配置文件里存的是 `"F8"` 这样的短名（`delta-config::Hotkeys`），本函数是唯一解析点：
/// 解析不出来就回落到 spec D6 的默认 F8，绝不 panic。
pub fn parse_hotkey(name: &str) -> Hotkey {
    let name = name.trim().to_ascii_uppercase();
    if let Some(digits) = name.strip_prefix('F') {
        if let Ok(index) = digits.parse::<u32>() {
            if (1..=24).contains(&index) {
                // F1 = 0x70，F24 = 0x87。
                return Hotkey::plain(0x70 + index - 1);
            }
        }
    }
    let mut characters = name.chars();
    if let (Some(single), None) = (characters.next(), characters.next()) {
        if single.is_ascii_alphanumeric() {
            return Hotkey::plain(single as u32);
        }
    }
    Hotkey::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_hotkey::vk;

    #[test]
    fn parses_function_keys() {
        assert_eq!(parse_hotkey("F8").vk, vk::F8);
        assert_eq!(parse_hotkey("f8").vk, vk::F8);
        assert_eq!(parse_hotkey(" F1 ").vk, 0x70);
        assert_eq!(parse_hotkey("F24").vk, 0x87);
    }

    #[test]
    fn parses_letters_and_digits() {
        assert_eq!(parse_hotkey("A").vk, u32::from(b'A'));
        assert_eq!(parse_hotkey("7").vk, u32::from(b'7'));
    }

    #[test]
    fn unknown_names_fall_back_to_f8() {
        assert_eq!(parse_hotkey("").vk, vk::F8);
        assert_eq!(parse_hotkey("Hyper").vk, vk::F8);
        assert_eq!(parse_hotkey("F99").vk, vk::F8);
    }

    #[test]
    fn mutex_name_is_wide_terminated() {
        let encoded = wide(INSTANCE_MUTEX);
        assert_eq!(encoded.last().copied(), Some(0));
        assert!(encoded.len() > 1);
    }
}
