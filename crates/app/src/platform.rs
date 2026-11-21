//! 平台贴边层：单实例互斥量、找窗激活、热键名解析。
//!
//! 所有 `unsafe` Win32 调用都收敛在本文件，业务层只看到 `Result<(), String>`。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use delta_hotkey::Hotkey;
use delta_ui::KeyChord;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, BOOL, ERROR_ALREADY_EXISTS, HWND, LPARAM};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowW, GetForegroundWindow, GetWindowTextW, GetWindowThreadProcessId,
    IsIconic, IsWindowVisible, SetForegroundWindow, SetWindowPos, ShowWindow, HWND_NOTOPMOST,
    HWND_TOPMOST, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE,
};

#[link(name = "user32")]
extern "system" {
    fn GetClassNameW(
        hwnd: *mut core::ffi::c_void,
        lp_class_name: *mut u16,
        n_max_count: i32,
    ) -> i32;
    fn SetWindowDisplayAffinity(hwnd: *mut core::ffi::c_void, dw_affinity: u32) -> i32;
}

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

/// 读取窗口类名（用于排除资源管理器 `CabinetWClass`、终端、浏览器等非游戏窗口）。
fn window_class(window: HWND) -> String {
    let mut holder = [0u16; 256];
    let length = unsafe { GetClassNameW(window.0, holder.as_mut_ptr(), holder.len() as i32) };
    if length <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&holder[..length as usize])
}

/// 判断窗口是否为真正的游戏窗口：
/// 过滤掉路径含「三角洲密码识别」的文件夹资源管理器、浏览器、编辑器与命令行窗口，
/// 防止 `focus_game_window` 把文件夹窗口置前抢走游戏焦点。
pub fn is_real_game_window(window: HWND, title: &str) -> bool {
    let class = window_class(window);
    if class == "UnrealWindow" {
        return true;
    }
    if matches!(
        class.as_str(),
        "CabinetWClass"
            | "ExploreWClass"
            | "ConsoleWindowClass"
            | "CASCADIA_HOSTING_WINDOW_CLASS"
            | "Chrome_WidgetWin_1"
            | "MozillaWindowClass"
            | "Notepad"
    ) {
        return false;
    }
    let lower = title.to_lowercase();
    for excluded in [
        "密码",
        "助手",
        "识别",
        "explorer",
        "visual studio",
        "vscode",
        "cmd.exe",
        "powershell",
    ] {
        if lower.contains(excluded) {
            return false;
        }
    }
    GAME_TITLES
        .iter()
        .any(|part| lower.contains(&part.to_lowercase()))
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

/// spec §2.2 不变量 8：任何截图/点击之前必须先确保游戏窗口在前台（点击是全局光标事件）。
pub fn focus_game_window() -> Result<String, String> {
    exclude_app_from_capture();
    let fg = unsafe { GetForegroundWindow() };
    if !fg.is_invalid() {
        let mut holder = [0u16; 512];
        let length = unsafe { GetWindowTextW(fg, &mut holder) };
        if length > 0 {
            let fg_title = String::from_utf16_lossy(&holder[..length as usize]);
            if is_real_game_window(fg, &fg_title) {
                return Ok(fg_title);
            }
        }
    }
    let windows = top_level_windows();
    if let Some((window, title)) = windows
        .iter()
        .find(|(w, title)| window_class(*w) == "UnrealWindow" && is_real_game_window(*w, title))
    {
        if bring_to_front(*window) {
            return Ok(title.clone());
        }
        return Err("游戏窗口无法置前".to_owned());
    }
    for title_part in ["Delta Force", "三角洲行动", "三角洲", "游戏"] {
        let needle = title_part.to_lowercase();
        if let Some((window, _)) = windows
            .iter()
            .find(|(w, title)| title.to_lowercase().contains(&needle) && is_real_game_window(*w, title))
        {
            if bring_to_front(*window) {
                return Ok(title_part.to_owned());
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

/// 本程序主窗口句柄。
///
/// eframe 0.37 的 `egui::ViewportInfo` 不暴露原生句柄（拿它要再引入
/// `raw-window-handle` 依赖，体积预算不允许），因此退回按标题精确定位。
/// 注意不能用 [`find_window`]：那个函数按设计跳过本进程窗口（它服务于「激活已有实例」），
/// 这里要找的恰恰是本进程自己的窗口，所以直接 `FindWindowW` 精确匹配标题。
pub fn app_window() -> Option<HWND> {
    let title = wide(delta_ui::APP_TITLE);
    unsafe { FindWindowW(PCWSTR::null(), PCWSTR(title.as_ptr())) }.ok()
}

/// `SetWindowPos` 的锚点：置顶 → `HWND_TOPMOST`，取消 → `HWND_NOTOPMOST`。
fn topmost_anchor(always_on_top: bool) -> HWND {
    if always_on_top {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    }
}

/// 立即改窗口置顶：不动位置、不改大小、不抢焦点（`SWP_NOMOVE|SWP_NOSIZE|SWP_NOACTIVATE`）。
///
/// 与 `ViewportCommand::WindowLevel` 等价，但同步生效 —— 图钉点下去这一帧就能看到结果，
/// 不必等 winit 的下一轮事件。
/// 将本程序窗口设为截图不可见（`WDA_EXCLUDEFROMCAPTURE = 0x11`）：
/// 避免置顶小窗压在摩斯格或横线台 CRT 屏幕上时污染截图。
pub fn exclude_app_from_capture() {
    const WDA_EXCLUDEFROMCAPTURE: u32 = 0x0000_0011;
    if let Some(window) = app_window() {
        unsafe {
            let _ = SetWindowDisplayAffinity(window.0, WDA_EXCLUDEFROMCAPTURE);
        }
    }
}

pub fn apply_always_on_top(always_on_top: bool) -> Result<(), String> {
    let window = app_window()
        .ok_or_else(|| format!("找不到窗口「{}」，置顶未生效", delta_ui::APP_TITLE))?;
    exclude_app_from_capture();
    unsafe {
        SetWindowPos(
            window,
            topmost_anchor(always_on_top),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        )
    }
    .map_err(|error| format!("置顶切换失败：{error}"))
}

/// 修饰键位（与 Win32 `MOD_ALT` / `MOD_CONTROL` / `MOD_SHIFT` 同值，注册时直接按位或）。
pub const MOD_ALT: u32 = 0x1;
pub const MOD_CONTROL: u32 = 0x2;
pub const MOD_SHIFT: u32 = 0x4;

/// 键名 → 虚拟键码的**唯一词汇表**（spec D1 的可用键集合）。
///
/// `delta_ui::state::CAPTURABLE` 是同一套短名的 egui 侧映射，
/// `capturable_vocabulary_matches_the_parser` 用例把两张表钉在一起。
pub const KEY_TABLE: [(&str, u32); 82] = [
    ("F1", 0x70),
    ("F2", 0x71),
    ("F3", 0x72),
    ("F4", 0x73),
    ("F5", 0x74),
    ("F6", 0x75),
    ("F7", 0x76),
    ("F8", 0x77),
    ("F9", 0x78),
    ("F10", 0x79),
    ("F11", 0x7A),
    ("F12", 0x7B),
    ("F13", 0x7C),
    ("F14", 0x7D),
    ("F15", 0x7E),
    ("F16", 0x7F),
    ("F17", 0x80),
    ("F18", 0x81),
    ("F19", 0x82),
    ("F20", 0x83),
    ("F21", 0x84),
    ("F22", 0x85),
    ("F23", 0x86),
    ("F24", 0x87),
    ("A", 0x41),
    ("B", 0x42),
    ("C", 0x43),
    ("D", 0x44),
    ("E", 0x45),
    ("F", 0x46),
    ("G", 0x47),
    ("H", 0x48),
    ("I", 0x49),
    ("J", 0x4A),
    ("K", 0x4B),
    ("L", 0x4C),
    ("M", 0x4D),
    ("N", 0x4E),
    ("O", 0x4F),
    ("P", 0x50),
    ("Q", 0x51),
    ("R", 0x52),
    ("S", 0x53),
    ("T", 0x54),
    ("U", 0x55),
    ("V", 0x56),
    ("W", 0x57),
    ("X", 0x58),
    ("Y", 0x59),
    ("Z", 0x5A),
    ("0", 0x30),
    ("1", 0x31),
    ("2", 0x32),
    ("3", 0x33),
    ("4", 0x34),
    ("5", 0x35),
    ("6", 0x36),
    ("7", 0x37),
    ("8", 0x38),
    ("9", 0x39),
    ("Space", 0x20),
    ("Insert", 0x2D),
    ("Delete", 0x2E),
    ("Home", 0x24),
    ("End", 0x23),
    ("PageUp", 0x21),
    ("PageDown", 0x22),
    ("ArrowUp", 0x26),
    ("ArrowDown", 0x28),
    ("ArrowLeft", 0x25),
    ("ArrowRight", 0x27),
    // spec D4：11 个 OEM 符号键。规范写法就是**符号本身** —— 与 `0`~`9` 一致，
    // 用户在 `settings.json` 里写的正是 `/`、`` ` ``，界面上看到的也是它。
    ("/", 0xBF),
    ("\\", 0xDC),
    (";", 0xBA),
    ("'", 0xDE),
    (",", 0xBC),
    (".", 0xBE),
    ("-", 0xBD),
    ("=", 0xBB),
    ("[", 0xDB),
    ("]", 0xDD),
    ("`", 0xC0),
];

/// 符号键的**英文别名**：只用于解析，绝不用于输出（输出一律走 [`KEY_TABLE`] 的字面量）。
///
/// 存在的理由：用户从旧配置/上游 Python 版抄过来的写法可能是 `Slash`；两条路都能解析，
/// 但归一化结果唯一 —— 否则同一个键会有两种写法在配置里漂移。
pub const KEY_ALIASES: [(&str, u32); 11] = [
    ("Slash", 0xBF),
    ("Backslash", 0xDC),
    ("Semicolon", 0xBA),
    ("Quote", 0xDE),
    ("Comma", 0xBC),
    ("Period", 0xBE),
    ("Minus", 0xBD),
    ("Equal", 0xBB),
    ("BracketLeft", 0xDB),
    ("BracketRight", 0xDD),
    ("Backquote", 0xC0),
];

/// 热键写法解析失败的原因。**每一种失败都必须显形**，不允许静默回落（spec D3）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyParseError {
    /// 空串或只有空白。
    Empty,
    /// 不认识的主键名（`sb250` / `F99` / `Win` / `Hyper`）。
    UnknownKey,
    /// 修饰键位写了主键名（`F9+Ctrl`）—— 顺序写反了，是非法写法而非可纠正的笔误。
    UnknownModifier,
    /// 只有修饰键，没有主键（`Ctrl` / `Ctrl+Alt`）。
    NoMainKey,
}

impl std::fmt::Display for HotkeyParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Empty => "热键为空",
            Self::UnknownKey => "热键里有不认识的键名",
            Self::UnknownModifier => "修饰键要写在主键前面",
            Self::NoMainKey => "热键缺少主键",
        };
        f.write_str(text)
    }
}

impl std::error::Error for HotkeyParseError {}

/// 修饰键短名 → 位；不是修饰键返回 `None`。
fn modifier_bit(token: &str) -> Option<u32> {
    if token.eq_ignore_ascii_case("Ctrl") || token.eq_ignore_ascii_case("Control") {
        return Some(MOD_CONTROL);
    }
    if token.eq_ignore_ascii_case("Alt") {
        return Some(MOD_ALT);
    }
    if token.eq_ignore_ascii_case("Shift") {
        return Some(MOD_SHIFT);
    }
    None
}

/// 主键短名 → 虚拟键码（大小写不敏感）。符号键额外接受英文别名。
fn key_code(token: &str) -> Option<u32> {
    KEY_TABLE
        .iter()
        .chain(KEY_ALIASES.iter())
        .find(|(name, _)| name.eq_ignore_ascii_case(token))
        .map(|(_, code)| *code)
}

/// 热键写法 → `Hotkey`。支持 `Ctrl`/`Alt`/`Shift` 任意组合 + 一个主键（spec D1）。
///
/// 配置文件里存的是 `"Ctrl+Shift+A"` 这样的规范短名，本函数是唯一解析点。
/// **失败返回 `Err`，绝不回落 F8** —— 回落只允许发生在启动读配置那一处（D3）。
pub fn parse_hotkey(name: &str) -> Result<Hotkey, HotkeyParseError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(HotkeyParseError::Empty);
    }
    let tokens: Vec<&str> = trimmed.split('+').map(str::trim).collect();
    // 空槽（`Ctrl+` / `Ctrl++A`）说明主键位没给出来。
    if tokens.iter().any(|token| token.is_empty()) {
        return Err(HotkeyParseError::NoMainKey);
    }
    // 全是修饰键 = 没有主键（纯修饰键不构成热键，D1 显式边界）。
    if tokens.iter().all(|token| modifier_bit(token).is_some()) {
        return Err(HotkeyParseError::NoMainKey);
    }
    let (main, modifiers) = tokens.split_last().ok_or(HotkeyParseError::NoMainKey)?;
    let mut bits = 0;
    for token in modifiers {
        match modifier_bit(token) {
            Some(bit) => bits |= bit,
            // 修饰键位写了已知主键 → 顺序写反；不认识的词 → 压根不是键名。
            None if key_code(token).is_some() => return Err(HotkeyParseError::UnknownModifier),
            None => return Err(HotkeyParseError::UnknownKey),
        }
    }
    let code = key_code(main).ok_or(HotkeyParseError::UnknownKey)?;
    Ok(Hotkey::new(code, bits))
}

/// 启动读配置：spec D3 **唯一**允许回落 F8 的地方，且必须 `eprintln` 留痕。
///
/// 返回**实际生效**的 `Hotkey`：热键线程注册它，`initial_state` 用 [`format_hotkey`]
/// 显示它 —— 因此面板永远不会显示一个没生效的名字（复审 S2-1）。
pub fn startup_hotkey(name: &str) -> Hotkey {
    // 配置层已拒绝无修饰的 `F`（`RegisterHotKey` 会全局独占，吞掉游戏交互键）。
    // 老配置里万一写着，这里也**绝不注册**它 —— 否则界面说在监听、游戏里按 F 却全失灵。
    if let Some(reason) = delta_config::single_key_rejection(name) {
        eprintln!("[hotkey] 配置里的热键 {name:?} 被拒（{reason}），本会话回落 F8");
        return Hotkey::default();
    }
    parse_hotkey(name).unwrap_or_else(|error| {
        eprintln!("[hotkey] 配置里的热键 {name:?} 无法解析（{error}），本会话回落 F8");
        Hotkey::default()
    })
}

/// 捕获结果 → `Hotkey`：直接用捕获到的虚拟键码与修饰位，**不走字符串往返**。
///
/// 词汇表两张表由 `capturable_vocabulary_matches_the_parser` 钉死，因此这里的 `None`
/// 只可能出现在不变量被破坏时；调用方按「注册失败」显形，绝不设默认值糊过去。
pub fn hotkey_from_chord(chord: KeyChord) -> Option<Hotkey> {
    key_code(chord.main_name()).map(|vk| Hotkey::new(vk, chord.modifiers()))
}

/// `Hotkey` → 规范写法：固定 `Ctrl+Alt+Shift+主键` 顺序（spec D1）。
///
/// 词汇表外的虚拟键码显形为 `0x6A`，**不得**假装成某个已知键。
pub fn format_hotkey(hotkey: &Hotkey) -> String {
    let main = KEY_TABLE
        .iter()
        .find(|(_, code)| *code == hotkey.vk)
        .map_or_else(|| format!("0x{:02X}", hotkey.vk), |(name, _)| (*name).to_owned());
    let mut text = String::new();
    for (bit, label) in [
        (MOD_CONTROL, "Ctrl"),
        (MOD_ALT, "Alt"),
        (MOD_SHIFT, "Shift"),
    ] {
        if hotkey.modifiers & bit != 0 {
            text.push_str(label);
            text.push('+');
        }
    }
    text.push_str(&main);
    text
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
    fn symbol_keys_parse_from_literals_and_english_names() {
        // spec D4：11 个 OEM 符号键。用户在界面/配置里写的是符号字面量（`/`、`` ` ``），
        // 英文标识符只是兼容写法 —— 两者必须解析到同一个虚拟键码，且归一化到字面量。
        for (literal, english, vk) in [
            ("/", "Slash", 0xBF),
            ("\\", "Backslash", 0xDC),
            (";", "Semicolon", 0xBA),
            ("'", "Quote", 0xDE),
            (",", "Comma", 0xBC),
            (".", "Period", 0xBE),
            ("-", "Minus", 0xBD),
            ("=", "Equal", 0xBB),
            ("[", "BracketLeft", 0xDB),
            ("]", "BracketRight", 0xDD),
            ("`", "Backquote", 0xC0),
        ] {
            assert_eq!(parse_hotkey(literal), Ok(Hotkey::plain(vk)), "{literal}");
            assert_eq!(parse_hotkey(english), Ok(Hotkey::plain(vk)), "{english}");
            assert_eq!(
                format_hotkey(&Hotkey::plain(vk)),
                literal,
                "{literal} 的规范写法必须是符号本身"
            );
        }
        assert_eq!(parse_hotkey("Ctrl+/"), Ok(Hotkey::new(0xBF, MOD_CONTROL)));
        assert_eq!(
            format_hotkey(&parse_hotkey("Ctrl+Slash").expect("英文名可解析")),
            "Ctrl+/"
        );
    }

    #[test]
    fn vocabulary_grew_to_82_keys() {
        // spec §4 第一条门禁：两张表各有 82 项。
        assert_eq!(KEY_TABLE.len(), 82, "71 项 + 11 个符号键");
        assert_eq!(delta_ui::state::CAPTURABLE.len(), 82);
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
    fn plain_f_is_never_registered_as_a_global_hotkey() {
        // 复审 S2 遗留③的最后一层保险：解析能力保留（F 键监听要用），
        // 但启动注册路径绝不注册它 —— 否则 RegisterHotKey 会吞掉游戏交互键。
        assert_eq!(parse_hotkey("F"), Ok(Hotkey::plain(0x46)), "解析能力不能砍");
        assert_eq!(startup_hotkey("F"), Hotkey::default(), "F 必须回落 F8");
        assert_eq!(startup_hotkey("ctrl+f"), Hotkey::new(0x46, MOD_CONTROL), "带修饰键允许");
        assert_eq!(startup_hotkey("F8"), Hotkey::plain(0x77));
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
            assert_eq!(parse_hotkey(name), Ok(Hotkey::plain(vk)), "{name}");
            let hotkey = Hotkey::new(vk, MOD_CONTROL | MOD_SHIFT);
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
        // 这条用例把两张表**双向**钉死：任何一张多一项/少一项都会红（复审 S2-3）。
        assert_eq!(
            KEY_TABLE.len(),
            delta_ui::state::CAPTURABLE.len(),
            "两张表条数必须一致：多出来的键要么解析不了、要么捕获不到"
        );
        // 无修饰的存活检查：保留键（`F`）之外的每一个键都必须能单独捕获并原样往返。
        for (name, key) in delta_ui::state::CAPTURABLE {
            if delta_config::single_key_rejection(name).is_some() {
                continue;
            }
            let chord = delta_ui::state::KeyChord::capture(key, egui::Modifiers::default())
                .unwrap_or_else(|| panic!("{name} 必须可捕获"));
            assert_eq!(chord.main_name(), name, "捕获结果必须用同一套短名");
            let hotkey = hotkey_from_chord(chord).unwrap_or_else(|| panic!("{name} 必须可解析"));
            assert_eq!(format_hotkey(&hotkey), name, "{name} 归一化必须唯一");
            assert_eq!(parse_hotkey(name), Ok(hotkey), "{name} 必须回得去");
        }
        // 保留键（GameKey F）走带修饰的组合验证：词汇与往返都不受影响。
        for (name, key) in delta_ui::state::CAPTURABLE {
            let ctrl = egui::Modifiers {
                ctrl: true,
                ..egui::Modifiers::default()
            };
            let chord = delta_ui::state::KeyChord::capture(key, ctrl)
                .unwrap_or_else(|| panic!("Ctrl+{name} 必须可捕获"));
            assert_eq!(chord.main_name(), name);
            let hotkey = hotkey_from_chord(chord).unwrap_or_else(|| panic!("Ctrl+{name} 必须可解析"));
            assert_eq!(format_hotkey(&hotkey), format!("Ctrl+{name}"));
            assert_eq!(parse_hotkey(&format!("Ctrl+{name}")), Ok(hotkey));
        }
    }

    #[test]
    fn only_the_startup_reader_falls_back_to_f8() {
        // spec D3：启动读配置是唯一允许回落的地方，其他调用点一律 Err 且必须显形。
        assert_eq!(startup_hotkey("sb250"), Hotkey::default());
        assert_eq!(format_hotkey(&startup_hotkey("sb250")), "F8", "面板显示生效值");
        assert_eq!(startup_hotkey("F9"), Hotkey::plain(0x78));
        assert_eq!(
            startup_hotkey("Ctrl+Shift+A"),
            Hotkey::new(0x41, MOD_CONTROL | MOD_SHIFT)
        );
        // 规范化：老配置写成小写也能显示成唯一写法（复审 S2-1）。
        assert_eq!(format_hotkey(&startup_hotkey("ctrl+f8")), "Ctrl+F8");
    }

    #[test]
    fn mutex_name_is_wide_terminated() {
        let encoded = wide(INSTANCE_MUTEX);
        assert_eq!(encoded.last().copied(), Some(0));
        assert!(encoded.len() > 1);
    }

    #[test]
    fn topmost_toggle_anchors_to_the_two_insert_after_handles() {
        // 置顶与取消必须是两个**不同**的锚点：写反了图钉就变成「点一下还是置顶」。
        assert_eq!(topmost_anchor(true), HWND_TOPMOST);
        assert_eq!(topmost_anchor(false), HWND_NOTOPMOST);
        assert_ne!(topmost_anchor(true), topmost_anchor(false));
    }
}
