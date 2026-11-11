//! F 键低级键盘钩子（`WH_KEYBOARD_LL`，spec D3）。
//!
//! # 三条必须先推翻才能改的硬边界
//!
//! 1. **回调里只读 `vkCode` + 按下状态 → 投递一个空事件**。截图、识别、文件 IO、
//!    锁等待一律禁止：低级钩子回调期间**全系统**键盘输入都在等我们返回。
//! 2. **永远返回 `CallNextHookEx`**，绝不吞键 —— 游戏要能收到 F 做交互。
//! 3. **关掉开关 = 卸载钩子**。⚠️ **绝对不要**"顺手补全"成 `RegisterHotKey("F")`：
//!    那是全局独占，会把游戏的 F 交互键吞掉，正是本模块存在的唯一理由（D3 边界 4）。
//!
//! # 为什么用专用线程
//!
//! 低级钩子的回调在**安装它的那个线程**的消息循环里执行。挂在 eframe/winit 的 UI 线程上
//! 能否生效取决于 winit 内部的消息泵实现（属于别人的实现细节），且卸载时机就绑死在
//! 窗口生命期上。这里起一个只跑 `GetMessageW` 的专用线程：回调必然被执行，
//! 卸载只需 `PostThreadMessageW(WM_QUIT)` + `join`，与 UI 线程完全解耦。

use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, GetWindowTextW, GetWindowThreadProcessId,
    PeekMessageW, PostThreadMessageW, SetWindowsHookExW, UnhookWindowsHookEx, KBDLLHOOKSTRUCT,
    MSG, PM_NOREMOVE, WH_KEYBOARD_LL, WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN,
};

/// F 键的虚拟键码。
pub const VK_F: u32 = 0x46;

/// `SetWindowsHookExW` 成功的等待上限（超过 = 钩子没装上，必须显形）。
const INSTALL_TIMEOUT: Duration = Duration::from_secs(2);

/// 一次游戏内 F 键按下的投递通道（容量小：只传递"有一次按键"，不排队按键历史）。
pub type GameKeySink = SyncSender<()>;

/// 钩子共享状态：回调需要它，卸载时清空。
struct HookState {
    sink: GameKeySink,
    keywords: Vec<String>,
}

/// 全局唯一槽位。**只在回调与装卸时短暂加锁**，槽位内没有任何业务逻辑。
fn slot() -> &'static Mutex<Option<HookState>> {
    static SLOT: OnceLock<Mutex<Option<HookState>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 前台窗口标题是否命中关键词（大小写不敏感的子串包含）。
///
/// 空关键词一律忽略；**一条都没有 = 永不响应**（不是"全都响应"）。
pub fn title_is_game(title: &str, keywords: &[String]) -> bool {
    let title = title.to_lowercase();
    keywords
        .iter()
        .map(|keyword| keyword.trim())
        .filter(|keyword| !keyword.is_empty())
        .any(|keyword| title.contains(&keyword.to_lowercase()))
}

/// 当前前台窗口标题（`None` = 没有前台窗口或取不到标题）。
fn foreground_title(window: HWND) -> Option<String> {
    let mut buffer = [0u16; 512];
    let length = unsafe { GetWindowTextW(window, &mut buffer) };
    if length <= 0 {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
}

/// 前台窗口是不是"游戏"：标题命中关键词，且**不是本进程自己的窗口**。
///
/// 后半句是必需的：本程序的窗口标题里也含「三角洲」，若不排除，用户在本程序界面上按 F
/// 也会触发识别（`platform::top_level_windows` 出于同一理由跳过本进程窗口）。
fn foreground_is_game(keywords: &[String]) -> bool {
    let window = unsafe { GetForegroundWindow() };
    if window.is_invalid() {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if pid == unsafe { GetCurrentProcessId() } {
        return false;
    }
    foreground_title(window).is_some_and(|title| title_is_game(&title, keywords))
}

/// 低级键盘回调：只判断"是不是 F 按下"，其余一律交给下一个钩子。
///
/// 这里**不许**出现任何可能阻塞或耗时的调用（见模块文档边界 1）。
unsafe extern "system" fn low_level_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // `code < 0` 时必须原样传下去（微软文档：不要继续处理）。
    if code >= 0 {
        let pressed = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
        if pressed {
            // SAFETY: `code >= 0` 时 `lparam` 指向有效的 `KBDLLHOOKSTRUCT`。
            let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
            if info.vkCode == VK_F {
                // 锁只覆盖"读关键词 + try_send"，`CallNextHookEx` 在锁外返回。
                if let Ok(guard) = slot().lock() {
                    if let Some(state) = guard.as_ref() {
                        if foreground_is_game(&state.keywords) {
                            // 满了就丢：宁可少触发一次，也绝不在回调里阻塞。
                            let _ = state.sink.try_send(());
                        }
                    }
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 已安装的钩子；`Drop` 一定会卸载（绝不泄漏钩子句柄）。
#[derive(Debug)]
pub struct KeyHook {
    thread_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl KeyHook {
    /// 装上钩子。`Err` = 装不上（原因显形，调用方决定是否提示用户）。
    pub fn install(keywords: Vec<String>, sink: GameKeySink) -> Result<Self, String> {
        if slot().lock().map(|guard| guard.is_some()).unwrap_or(false) {
            return Err("F 键钩子已经装过了".to_owned());
        }
        let (ready_tx, ready_rx) = mpsc::channel::<Result<u32, String>>();
        let thread = std::thread::Builder::new()
            .name("delta-keyhook".to_owned())
            .spawn(move || hook_thread(keywords, sink, ready_tx))
            .map_err(|error| format!("F 键钩子线程启动失败：{error}"))?;
        match ready_rx.recv_timeout(INSTALL_TIMEOUT) {
            Ok(Ok(thread_id)) => Ok(Self {
                thread_id,
                thread: Some(thread),
            }),
            Ok(Err(reason)) => {
                let _ = thread.join();
                Err(reason)
            }
            Err(error) => {
                // 线程没回话：让它自己跑完（不做阻塞等待），但状态机保持一致。
                Err(format!("F 键钩子未在限时内就绪：{error}"))
            }
        }
    }

    /// 热更新前台关键词（设置页改完立即生效，不必重装钩子）。
    pub fn set_keywords(&self, keywords: Vec<String>) {
        if let Ok(mut guard) = slot().lock() {
            if let Some(state) = guard.as_mut() {
                state.keywords = keywords;
            }
        }
    }

    /// 卸载（`Drop` 也会调用，重复调用安全）。
    pub fn uninstall(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        // 专用线程先 `PeekMessageW` 建过消息队列，因此这条一定能投到。
        unsafe { PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) }.ok();
        let _ = thread.join();
    }
}

impl Drop for KeyHook {
    fn drop(&mut self) {
        self.uninstall();
    }
}

/// 钩子线程：装钩子 → 跑消息泵 → 退出时卸载。
fn hook_thread(keywords: Vec<String>, sink: GameKeySink, ready: mpsc::Sender<Result<u32, String>>) {
    // 先把消息队列建出来（`GetMessageW` 之前先 Peek 一次），
    // 之后 `PostThreadMessageW(WM_QUIT)` 才一定能唤醒本线程。
    let mut message = MSG::default();
    let _ = unsafe { PeekMessageW(&mut message, None, 0, 0, PM_NOREMOVE) };

    if let Ok(mut guard) = slot().lock() {
        *guard = Some(HookState { sink, keywords });
    } else {
        let _ = ready.send(Err("F 键钩子状态锁已损坏".to_owned()));
        return;
    }

    // 低级钩子在本进程执行，不需要 DLL 注入，`hmod` 传 `None` 即可。
    let hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(low_level_proc), None, 0) } {
        Ok(hook) => hook,
        Err(error) => {
            clear_slot();
            let _ = ready.send(Err(format!("F 键钩子安装失败：{error}")));
            return;
        }
    };
    let _ = ready.send(Ok(unsafe { GetCurrentThreadId() }));

    loop {
        let got = unsafe { GetMessageW(&mut message, None, 0, 0) };
        // 0 = WM_QUIT；-1 = 出错。两者都退出循环。
        if got.0 <= 0 {
            break;
        }
    }

    unsafe { UnhookWindowsHookEx(hook) }.ok();
    clear_slot();
}

/// 清空共享槽位（回调随即变成空转，只 `CallNextHookEx`）。
fn clear_slot() {
    if let Ok(mut guard) = slot().lock() {
        *guard = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{keybd_event, KEYEVENTF_KEYUP};

    fn keywords(list: &[&str]) -> Vec<String> {
        list.iter().map(|keyword| (*keyword).to_owned()).collect()
    }

    #[test]
    fn title_matching_is_case_insensitive_substring() {
        let keywords = keywords(&["三角洲", "Delta Force"]);
        assert!(title_is_game("三角洲行动 - 客户端", &keywords));
        assert!(title_is_game("DELTA FORCE Open Beta", &keywords));
        assert!(title_is_game("playing delta force now", &keywords));
        assert!(!title_is_game("记事本", &keywords));
        assert!(!title_is_game("", &keywords));
    }

    #[test]
    fn blank_keywords_are_ignored_and_an_empty_list_never_matches() {
        // 空关键词不能变成"匹配一切"（否则清空输入框就会在桌面任意窗口响应 F）。
        assert!(!title_is_game("三角洲行动", &keywords(&["", "   "])));
        assert!(!title_is_game("任何标题", &[]));
        // 首尾空格由解析侧负责去掉，不要求用户精确输入。
        assert!(title_is_game("三角洲行动", &keywords(&[" 三角洲 "])));
    }

    #[test]
    #[ignore = "向真实桌面注入 F 键，默认跳过；需要时 cargo test -p delta-app --lib -- --ignored"]
    fn hook_lifecycle_reports_f_keeps_passing_it_and_stops_when_uninstalled() {
        // 钩子是进程级单例：装/卸/重装的完整生命周期只在这一个用例里跑，
        // 否则两个用例会互相抢全局槽位。
        let foreground = {
            let window = unsafe { GetForegroundWindow() };
            foreground_title(window)
        };
        let Some(title) = foreground else {
            panic!("没有前台窗口，无法验证钩子（必须在前台有窗口的会话里跑）");
        };

        let (tx, rx) = mpsc::sync_channel(8);
        let mut hook = KeyHook::install(vec![title.clone()], tx).expect("低级键盘钩子必须能装上");

        press_f();
        assert!(
            rx.recv_timeout(Duration::from_secs(2)).is_ok(),
            "装上钩子后按 F 必须投递一次事件"
        );

        // 重复安装必须显形报错，而不是偷偷泄漏第二个钩子句柄。
        let (dup_tx, _dup_rx) = mpsc::sync_channel(8);
        assert!(
            KeyHook::install(vec![title], dup_tx).is_err(),
            "重复安装必须报错"
        );

        hook.uninstall();
        press_f();
        assert!(
            rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "卸载后按 F 不得再有任何反应"
        );

        // 关掉再打开（设置页的开关路径）必须还能装上。
        let (again_tx, _again_rx) = mpsc::sync_channel(8);
        let again = KeyHook::install(keywords(&["三角洲"]), again_tx).expect("卸载后必须能重新安装");
        drop(again);
    }

    /// 合成一次 F 键按下 + 抬起（`keybd_event` 与游戏走同一条输入队列）。
    fn press_f() {
        unsafe {
            keybd_event(VK_F as u8, 0, Default::default(), 0);
            keybd_event(VK_F as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        // 给系统一点时间把事件送进钩子链（同进程内通常 1ms 内到）。
        std::thread::sleep(Duration::from_millis(50));
    }
}
