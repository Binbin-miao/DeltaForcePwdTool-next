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

/// 是否以管理员身份运行。
///
/// **为什么必须要管理员**：游戏通常以管理员启动，Windows 的 UIPI 会静默丢弃
/// 低权限进程发往高权限窗口的 `SendInput`。也就是说「权限不够时点击看起来发出去了，
/// 游戏却一点反应都没有」—— 上游 Python 版同样要求管理员，这里保持一致的体验。
pub fn is_elevated() -> bool {
    unsafe { windows::Win32::UI::Shell::IsUserAnAdmin().as_bool() }
}

/// 以管理员身份重启自身（弹 UAC）。返回 `true` = 已交给新进程，旧进程应退出。
///
/// 用户拒绝 UAC 时返回 `false`：程序仍以普通权限运行，界面可用、热键可用，
/// 但点进游戏可能无效 —— 由调用方在界面上给出提示，而不是直接拒绝启动。
pub fn relaunch_elevated() -> bool {
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info).is_ok() }
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

    #[test]
    fn parses_single_keys() {
        assert_eq!(parse_hotkey("F8"), Ok(Hotkey::plain(0x77)));
        // 小写等价（D1）。
        assert_eq!(parse_hotkey("f24"), Ok(Hotkey::plain(0x87)));
        assert_eq!(parse_hotkey(" F1 "), Ok(Hotkey::plain(0x70)));
        assert_eq!(parse_hotkey("a"), Ok(Hotkey::plain(0x41)));
        assert_eq!(parse_hotkey("7"), Ok(Hotkey::plain(0x37)));
        assert_eq!(parse_hotkey("Space"), Ok(Hotkey::plain(0x20)));
        assert_eq!(parse_hotkey("PageUp"), Ok(Hotkey::plain(0x21)));
        assert_eq!(parse_hotkey("ArrowLeft"), Ok(Hotkey::plain(0x25)));
    }

    #[test]
    fn parses_modifier_combinations() {
        assert_eq!(parse_hotkey("Ctrl+F8"), Ok(Hotkey::new(0x77, MOD_CONTROL)));
        assert_eq!(
            parse_hotkey("Ctrl+Alt+Shift+A"),
            Ok(Hotkey::new(0x41, MOD_CONTROL | MOD_ALT | MOD_SHIFT))
        );
    }

    #[test]
    fn modifier_order_is_normalised() {
        // 顺序不敏感地进，只以一种写法的串出（D1 边界）。
        assert_eq!(
            parse_hotkey("Shift+Ctrl+A"),
            parse_hotkey("Ctrl+Shift+A")
        );
        assert_eq!(
            format_hotkey(&parse_hotkey("Shift+Ctrl+A").expect("合法组合")),
            "Ctrl+Shift+A"
        );
    }

    #[test]
    fn unknown_names_are_errors_not_f8() {
        // 假保存的源头：这里绝不允许再回落 F8。
        assert_eq!(parse_hotkey("sb250"), Err(HotkeyParseError::UnknownKey));
        assert_eq!(parse_hotkey("F99"), Err(HotkeyParseError::UnknownKey));
        assert_eq!(parse_hotkey("Hyper"), Err(HotkeyParseError::UnknownKey));
        assert_eq!(parse_hotkey("Win+A"), Err(HotkeyParseError::UnknownKey));
        assert_eq!(
            parse_hotkey("Ctrl+Hyper+A"),
            Err(HotkeyParseError::UnknownKey)
        );
        assert_eq!(parse_hotkey(""), Err(HotkeyParseError::Empty));
        assert_eq!(parse_hotkey("   "), Err(HotkeyParseError::Empty));
        assert_eq!(parse_hotkey("Ctrl"), Err(HotkeyParseError::NoMainKey));
        assert_eq!(parse_hotkey("Ctrl+Alt"), Err(HotkeyParseError::NoMainKey));
        // 修饰键必须写在主键前面，写反了是非法写法而不是静默纠正。
        assert_eq!(
            parse_hotkey("F9+Ctrl"),
            Err(HotkeyParseError::UnknownModifier)
        );
    }

    #[test]
    fn every_table_entry_round_trips() {
        for (name, vk) in KEY_TABLE {
            assert_eq!(parse_hotkey(name), Ok(Hotkey::plain(*vk)), "{name}");
            let hotkey = Hotkey::new(*vk, MOD_CONTROL | MOD_SHIFT);
            assert_eq!(format_hotkey(&hotkey), format!("Ctrl+Shift+{name}"));
            assert_eq!(parse_hotkey(&format_hotkey(&hotkey)), Ok(hotkey));
        }
    }

    #[test]
    fn format_orders_modifiers_and_flags_unknown_keys() {
        assert_eq!(
            format_hotkey(&Hotkey::new(0x77, MOD_SHIFT | MOD_ALT | MOD_CONTROL)),
            "Ctrl+Alt+Shift+F8"
        );
        assert_eq!(format_hotkey(&Hotkey::default()), "F8");
        assert_eq!(
            format_hotkey(&Hotkey::plain(0x6A)),
            "0x6A",
            "未知键必须显形，不得假装成某个已知键"
        );
    }

    #[test]
    fn capturable_vocabulary_matches_the_parser() {
        // ui 侧负责 egui::Key → 短名，本文件负责短名 → 虚拟键码。
        // 这条用例把两张表钉在一起：任何一张漂移都会红。
        for (name, key) in delta_ui::state::CAPTURABLE {
            let chord = delta_ui::state::KeyChord::capture(*key, egui::Modifiers::default())
                .unwrap_or_else(|| panic!("{name} 必须可捕获"));
            assert_eq!(chord.label(), name, "捕获结果必须用同一套短名");
            let hotkey =
                parse_hotkey(&chord.label()).unwrap_or_else(|error| panic!("{name}: {error}"));
            assert_eq!(format_hotkey(&hotkey), chord.label(), "{name} 归一化必须唯一");
        }
    }

    #[test]
    fn mutex_name_is_wide_terminated() {
        let encoded = wide(INSTANCE_MUTEX);
        assert_eq!(encoded.last().copied(), Some(0));
        assert!(encoded.len() > 1);
    }
}
