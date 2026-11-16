//! 全局热键（spec D6、§2.1 `main.py:82-90` 行）。
//!
//! 设计要点：
//! - `RegisterHotKey` 必须与**消息泵在同一个线程**，所以本模块开一条专用线程，
//!   线程内 `RegisterHotKey` + `GetMessageW` 循环，命中后把事件丢给 `mpsc::Sender`；
//! - 免管理员权限（`RegisterHotKey` 不需要提权，这一点上游用 `keyboard` 库反而是劣势）；
//! - 必须带 `MOD_NOREPEAT`，否则按住 F8 会连发触发一串识别；
//! - 注册失败（键被别的程序占用）必须返回明确错误码，不得静默；
//! - 支持同时注册多个键，v1 只用一个「一键场景识别」（默认 F8）。
//!
//! 唤醒机制：命令行请求走 `pending` 队列 + `PostThreadMessageW(WM_APP_WAKE)` 唤醒消息泵，
//! 不用额外的轮询线程。

use std::collections::BTreeMap;
use std::sync::mpsc::{channel, Receiver, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use thiserror::Error;
use windows::Win32::Foundation::{GetLastError, HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, PeekMessageW, PostQuitMessage, PostThreadMessageW, MSG, PM_NOREMOVE, WM_HOTKEY,
};

/// `MOD_NOREPEAT`：长按不重复触发（spec D6）。
const MOD_NOREPEAT: u32 = 0x4000;
/// 自定义唤醒消息（`WM_APP` 区间不与其他消息冲突）。
const WM_APP_WAKE: u32 = 0x8000 + 1;
/// 等待消息泵应答的上限，超时视为线程失联。
const REPLY_TIMEOUT: Duration = Duration::from_millis(500);

/// 常用的虚拟键码（只列 v1 需要的，其余调用方直接传 u32）。
pub mod vk {
    pub const F8: u32 = 0x77;
    pub const F24: u32 = 0x87;
    pub const F23: u32 = 0x86;
}

/// 一个热键：虚拟键码 + 修饰键位（`MOD_*` 的按位或，`MOD_NOREPEAT` 由本模块强制附加）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Hotkey {
    pub vk: u32,
    pub modifiers: u32,
}

impl Hotkey {
    pub const fn new(vk: u32, modifiers: u32) -> Self {
        Self { vk, modifiers }
    }

    /// 无修饰键的热键。
    pub const fn plain(vk: u32) -> Self {
        Self { vk, modifiers: 0 }
    }

    fn modifiers_with_norepeat(self) -> HOT_KEY_MODIFIERS {
        HOT_KEY_MODIFIERS(self.modifiers | MOD_NOREPEAT)
    }
}

impl Default for Hotkey {
    /// spec D6：默认 F8。
    fn default() -> Self {
        Self::plain(vk::F8)
    }
}

/// 热键事件。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HotkeyEvent {
    pub id: u32,
    pub vk: u32,
}

/// 热键模块错误。每一种失败都有独立变体，不吞错。
#[derive(Debug, Error)]
pub enum HotkeyError {
    #[error("热键 {vk:#06x} 已在本进程注册过")]
    AlreadyRegistered { vk: u32 },
    #[error("注册热键 {vk:#06x} 失败（错误码 {code}）：通常是被其他程序占用")]
    RegisterFailed { vk: u32, code: u32 },
    #[error("注销热键 {id} 失败（错误码 {code}）")]
    UnregisterFailed { id: u32, code: u32 },
    #[error("热键线程未响应（{0:?} 超时）")]
    Unresponsive(Duration),
    #[error("热键线程已退出，事件通道关闭")]
    ThreadGone,
    #[error("无法创建热键线程：{0}")]
    Spawn(#[from] std::io::Error),
}

impl PartialEq for HotkeyError {
    fn eq(&self, other: &Self) -> bool {
        use HotkeyError::*;
        match (self, other) {
            (AlreadyRegistered { vk: a }, AlreadyRegistered { vk: b }) => a == b,
            (RegisterFailed { vk: a, code: c }, RegisterFailed { vk: b, code: d }) => a == b && c == d,
            (Unresponsive(a), Unresponsive(b)) => a == b,
            (ThreadGone, ThreadGone) => true,
            _ => false,
        }
    }
}

enum Command {
    Register {
        hotkey: Hotkey,
        id: u32,
        reply: SyncSender<Result<(), HotkeyError>>,
    },
    Unregister {
        id: u32,
        reply: SyncSender<Result<(), HotkeyError>>,
    },
}

enum Endpoint {
    Thread(u32),
    #[cfg(test)]
    Offline {
        occupied: Arc<Mutex<BTreeMap<Hotkey, u32>>>,
        events: Sender<HotkeyEvent>,
    },
}

/// 热键注册器。`Drop` 会自动注销全部热键并结束消息泵线程。
pub struct Hotkeys {
    pending: Arc<Mutex<Vec<Command>>>,
    events: Receiver<HotkeyEvent>,
    endpoint: Endpoint,
    registered: Arc<Mutex<BTreeMap<u32, Hotkey>>>,
    join: Option<JoinHandle<()>>,
}

impl Hotkeys {
    /// 启动专用消息泵线程。**不需要管理员权限**。
    pub fn new() -> Result<Self, HotkeyError> {
        let (boot_tx, boot_rx) = channel::<Result<u32, HotkeyError>>();
        let (event_tx, events) = channel::<HotkeyEvent>();
        let pending = Arc::new(Mutex::new(Vec::new()));
        let registered = Arc::new(Mutex::new(BTreeMap::new()));

        let thread_pending = Arc::clone(&pending);
        let thread_registered = Arc::clone(&registered);
        let join = std::thread::Builder::new()
            .name("delta-hotkey".to_string())
            .spawn(move || message_loop(boot_tx, event_tx, thread_pending, thread_registered))?;

        let thread_id = boot_rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| HotkeyError::Unresponsive(REPLY_TIMEOUT))??;

        Ok(Self {
            pending,
            events,
            endpoint: Endpoint::Thread(thread_id),
            registered,
            join: Some(join),
        })
    }

    /// 注册一个热键，返回热键 id（用于 [`Hotkeys::unregister`] 与事件关联）。
    pub fn register(&self, hotkey: Hotkey) -> Result<u32, HotkeyError> {
        let mut registered = self
            .registered
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if registered.values().any(|existing| *existing == hotkey) {
            return Err(HotkeyError::AlreadyRegistered { vk: hotkey.vk });
        }
        let id = registered.keys().next_back().copied().unwrap_or(0) + 1;
        // 先占位再请求系统注册，避免并发重复注册同一 id。
        registered.insert(id, hotkey);
        drop(registered);

        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.enqueue(Command::Register {
            hotkey,
            id,
            reply: reply_tx,
        })?;
        let outcome = reply_rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| HotkeyError::Unresponsive(REPLY_TIMEOUT))?;
        if outcome.is_err() {
            self.registered
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .remove(&id);
        }
        outcome.map(|()| id)
    }

    /// 注销一个热键。
    pub fn unregister(&self, id: u32) -> Result<(), HotkeyError> {
        let (reply_tx, reply_rx) = std::sync::mpsc::sync_channel(1);
        self.enqueue(Command::Unregister { id, reply: reply_tx })?;
        let outcome = reply_rx
            .recv_timeout(REPLY_TIMEOUT)
            .map_err(|_| HotkeyError::Unresponsive(REPLY_TIMEOUT))?;
        outcome?;
        self.registered
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(&id);
        Ok(())
    }

    /// 非阻塞取一个事件。
    pub fn try_recv(&self) -> Option<HotkeyEvent> {
        self.events.try_recv().ok()
    }

    /// 带超时阻塞取一个事件（UI 空闲循环用）。
    pub fn next_event(&self, timeout: Duration) -> Option<HotkeyEvent> {
        self.events.recv_timeout(timeout).ok()
    }

    /// 当前已注册的热键快照。
    pub fn registered(&self) -> Vec<(u32, Hotkey)> {
        self.registered
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
            .map(|(id, hotkey)| (*id, *hotkey))
            .collect()
    }

    /// 模拟一次热键触发（向消息泵投递 `WM_HOTKEY`）。
    ///
    /// 这是给自动化测试用的通道：真实热键需要人按键盘，无法在 CI 里复现，
    /// 而消息泵本身的行为（查找 + 派发）与真实触发完全一致。
    pub fn simulate_trigger(&self, id: u32) -> Result<(), HotkeyError> {
        #[cfg(test)]
        if let Endpoint::Offline { events, .. } = &self.endpoint {
            return dispatch_hotkey(&self.registered, events, id);
        }
        let thread_id = match &self.endpoint {
            Endpoint::Thread(id) => *id,
            #[cfg(test)]
            Endpoint::Offline { .. } => unreachable!("offline endpoint handled above"),
        };
        // SAFETY: 线程 id 由消息泵线程自己上报，投递失败会返回错误。
        unsafe {
            PostThreadMessageW(thread_id, WM_HOTKEY, WPARAM(id as usize), LPARAM(0))
        }
        .map_err(|error| HotkeyError::RegisterFailed {
            vk: 0,
            code: error.code().0 as u32,
        })
    }

    fn enqueue(&self, command: Command) -> Result<(), HotkeyError> {
        #[cfg(test)]
        if let Endpoint::Offline { occupied, .. } = &self.endpoint {
            serve_command(command, &self.registered, |id, hotkey| {
                let mut slots = occupied.lock().unwrap_or_else(|poison| poison.into_inner());
                if slots.contains_key(&hotkey) {
                    return Err(HotkeyError::RegisterFailed { vk: hotkey.vk, code: 1409 });
                }
                slots.insert(hotkey, id);
                Ok(())
            }, |id| {
                let hotkey = self.registered.lock().unwrap_or_else(|poison| poison.into_inner())
                    .get(&id).copied();
                if let Some(hotkey) = hotkey {
                    occupied.lock().unwrap_or_else(|poison| poison.into_inner()).remove(&hotkey);
                }
                Ok(())
            });
            return Ok(());
        }
        let thread_id = match &self.endpoint {
            Endpoint::Thread(id) => *id,
            #[cfg(test)]
            Endpoint::Offline { .. } => unreachable!("offline endpoint handled above"),
        };
        self.pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(command);
        // SAFETY: 线程 id 有效；投递失败代表消息泵已退出。
        unsafe { PostThreadMessageW(thread_id, WM_APP_WAKE, WPARAM(0), LPARAM(0)) }.map_err(
            |error| HotkeyError::RegisterFailed {
                vk: 0,
                code: error.code().0 as u32,
            },
        )
    }
}

impl Drop for Hotkeys {
    fn drop(&mut self) {
        // 注销全部热键，然后让消息泵退出。
        let ids: Vec<u32> = self
            .registered
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .keys()
            .copied()
            .collect();
        #[cfg(test)]
        if let Endpoint::Offline { occupied, .. } = &self.endpoint {
            let keys = self.registered();
            let mut slots = occupied.lock().unwrap_or_else(|poison| poison.into_inner());
            for (_, hotkey) in keys {
                slots.remove(&hotkey);
            }
            return;
        }
        let thread_id = match &self.endpoint {
            Endpoint::Thread(id) => *id,
            #[cfg(test)]
            Endpoint::Offline { .. } => unreachable!("offline endpoint handled above"),
        };
        for id in ids {
            // SAFETY: 句柄来自本进程，id 是本模块分配的。
            let _ = unsafe { UnregisterHotKey(HWND(std::ptr::null_mut()), id as i32) };
        }
        // SAFETY: 线程 id 有效。
        let _ = unsafe { PostThreadMessageW(thread_id, 0x0012, WPARAM(0), LPARAM(0)) }; // WM_QUIT
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// 消息泵线程主体。`RegisterHotKey` 与 `GetMessageW` 必须同线程。
fn message_loop(
    boot: Sender<Result<u32, HotkeyError>>,
    events: Sender<HotkeyEvent>,
    pending: Arc<Mutex<Vec<Command>>>,
    registered: Arc<Mutex<BTreeMap<u32, Hotkey>>>,
) {
    // 先建消息队列（PeekMessage 会强制创建），再上报线程 id，
    // 否则主线程的 PostThreadMessageW 可能投到一个还不存在的队列上。
    // SAFETY: MSG 由本函数持有。
    let mut message = MSG::default();
    unsafe {
        let _ = PeekMessageW(&mut message, HWND(std::ptr::null_mut()), 0, 0, PM_NOREMOVE);
    }
    let thread_id = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };
    if boot.send(Ok(thread_id)).is_err() {
        return;
    }

    loop {
        // SAFETY: message 是有效的栈对象。
        let status = unsafe { GetMessageW(&mut message, HWND(std::ptr::null_mut()), 0, 0) };
        if status.0 <= 0 {
            // 0 = WM_QUIT，-1 = 错误；两种情况都结束线程。
            break;
        }
        match message.message {
            WM_HOTKEY => {
                let id = message.wParam.0 as u32;
                if dispatch_hotkey(&registered, &events, id).is_err() {
                    break;
                }
            }
            WM_APP_WAKE => serve_pending(&pending, &registered),
            _ => {}
        }
    }
    // SAFETY: 只影响本线程的消息队列，本函数正运行在该线程上。
    unsafe { PostQuitMessage(0) };
}

fn serve_pending(pending: &Mutex<Vec<Command>>, registered: &Mutex<BTreeMap<u32, Hotkey>>) {
    let commands: Vec<Command> = {
        let mut queue = pending.lock().unwrap_or_else(|poison| poison.into_inner());
        std::mem::take(&mut *queue)
    };
    for command in commands {
        serve_command(command, registered, |id, hotkey| {
            // SAFETY: 绑定本线程消息队列，与消息泵同线程。
            unsafe { RegisterHotKey(HWND(std::ptr::null_mut()), id as i32,
                hotkey.modifiers_with_norepeat(), hotkey.vk) }
                .map_err(|_| HotkeyError::RegisterFailed {
                    vk: hotkey.vk, code: unsafe { GetLastError() }.0,
                })
        }, |id| {
            unsafe { UnregisterHotKey(HWND(std::ptr::null_mut()), id as i32) }
                .map_err(|_| HotkeyError::UnregisterFailed {
                    id, code: unsafe { GetLastError() }.0,
                })
        });
    }
}

fn serve_command(
    command: Command,
    registered: &Mutex<BTreeMap<u32, Hotkey>>,
    register: impl FnOnce(u32, Hotkey) -> Result<(), HotkeyError>,
    unregister: impl FnOnce(u32) -> Result<(), HotkeyError>,
) {
    match command {
        Command::Register { hotkey, id, reply } => {
            let result = register(id, hotkey).map(|()| {
                registered.lock().unwrap_or_else(|poison| poison.into_inner()).insert(id, hotkey);
            });
            let _ = reply.send(result);
        }
        Command::Unregister { id, reply } => {
            let result = unregister(id).map(|()| {
                registered.lock().unwrap_or_else(|poison| poison.into_inner()).remove(&id);
            });
            let _ = reply.send(result);
        }
    }
}

fn dispatch_hotkey(
    registered: &Mutex<BTreeMap<u32, Hotkey>>,
    events: &Sender<HotkeyEvent>,
    id: u32,
) -> Result<(), HotkeyError> {
    let vk = registered.lock().unwrap_or_else(|poison| poison.into_inner())
        .get(&id).map_or(0, |hotkey| hotkey.vk);
    events.send(HotkeyEvent { id, vk }).map_err(|_| HotkeyError::ThreadGone)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 系统热键表是**进程级全局**的：多个测试线程同时注册同一个键会撞
    /// `ERROR_HOTKEY_ALREADY_REGISTERED`（1409）。所有碰系统的用例串行执行。
    static SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    #[test]
    #[ignore = "真实系统热键注册；游戏运行期间禁止执行，须单独授权"]
    fn live_register_then_simulated_post_delivers_event() {
        let _serial = serial();
        let hotkeys = Hotkeys::new().expect("启动热键线程");
        let id = hotkeys
            .register(Hotkey::plain(vk::F24))
            .expect("注册 F24 不应失败（免管理员）");
        assert_eq!(hotkeys.registered().len(), 1);

        hotkeys.simulate_trigger(id).expect("投递 WM_HOTKEY");
        let event = hotkeys
            .next_event(Duration::from_millis(1000))
            .expect("必须收到热键事件");
        assert_eq!(event.id, id);
        assert_eq!(event.vk, vk::F24, "事件必须带出对应的虚拟键码");
    }

    #[test]
    #[ignore = "真实系统热键注册；游戏运行期间禁止执行，须单独授权"]
    fn live_duplicate_registration_is_rejected_with_a_named_error() {
        let _serial = serial();
        let hotkeys = Hotkeys::new().expect("启动热键线程");
        // 用备用键 F23：真实程序在跑时占着 F8（生产默认键），断言会撞成 RegisterFailed。
        let first = hotkeys.register(Hotkey::plain(vk::F23)).expect("首次注册 F23");
        let error = hotkeys
            .register(Hotkey::plain(vk::F23))
            .expect_err("重复注册必须报错");
        assert_eq!(
            error,
            HotkeyError::AlreadyRegistered { vk: vk::F23 },
            "必须是具名错误，不得静默"
        );
        assert_eq!(hotkeys.registered().len(), 1, "失败的注册不得留下残留");
        hotkeys.unregister(first).expect("注销");
        assert!(hotkeys.registered().is_empty());
    }

    #[test]
    #[ignore = "真实系统热键注册；游戏运行期间禁止执行，须单独授权"]
    fn live_multiple_keys_can_be_registered_and_addressed_independently() {
        let _serial = serial();
        let hotkeys = Hotkeys::new().expect("启动热键线程");
        let first = hotkeys.register(Hotkey::plain(vk::F23)).expect("注册 F23");
        let second = hotkeys.register(Hotkey::plain(vk::F24)).expect("注册 F24");
        assert_ne!(first, second, "id 必须互不相同");

        hotkeys.simulate_trigger(second).expect("投递");
        let event = hotkeys.next_event(Duration::from_millis(1000)).expect("事件");
        assert_eq!(event.id, second);
        assert_eq!(event.vk, vk::F24);
    }

    #[test]
    #[ignore = "真实系统热键注册；游戏运行期间禁止执行，须单独授权"]
    fn live_try_recv_is_non_blocking_when_nothing_happened() {
        let _serial = serial();
        let hotkeys = Hotkeys::new().expect("启动热键线程");
        let _ = hotkeys.register(Hotkey::plain(vk::F23)).expect("注册");
        assert_eq!(hotkeys.try_recv(), None, "没有触发时必须立刻返回 None");
    }

    #[test]
    #[ignore = "真实系统热键注册；游戏运行期间禁止执行，须单独授权"]
    fn live_drop_releases_hotkeys_so_they_can_be_registered_again() {
        let _serial = serial();
        {
            let hotkeys = Hotkeys::new().expect("启动热键线程");
            hotkeys.register(Hotkey::plain(vk::F24)).expect("注册");
        }
        // 若 Drop 没注销，这里会被系统拒绝。
        let hotkeys = Hotkeys::new().expect("重启线程");
        hotkeys
            .register(Hotkey::plain(vk::F24))
            .expect("Drop 后必须能重新注册同一热键");
    }

    fn offline(occupied: Arc<Mutex<BTreeMap<Hotkey, u32>>>) -> Hotkeys {
        let (event_tx, events) = channel();
        Hotkeys {
            pending: Arc::new(Mutex::new(Vec::new())),
            events,
            endpoint: Endpoint::Offline { occupied, events: event_tx },
            registered: Arc::new(Mutex::new(BTreeMap::new())),
            join: None,
        }
    }

    #[test]
    fn register_then_simulated_post_delivers_event() {
        let hotkeys = offline(Arc::default());
        let id = hotkeys.register(Hotkey::plain(vk::F24)).expect("注册 fake");
        hotkeys.simulate_trigger(id).expect("fake 消息");
        assert_eq!(hotkeys.try_recv(), Some(HotkeyEvent { id, vk: vk::F24 }));
    }

    #[test]
    fn duplicate_registration_is_rejected_with_a_named_error() {
        let hotkeys = offline(Arc::default());
        let first = hotkeys.register(Hotkey::plain(vk::F23)).expect("首次");
        assert_eq!(hotkeys.register(Hotkey::plain(vk::F23)),
            Err(HotkeyError::AlreadyRegistered { vk: vk::F23 }));
        assert_eq!(hotkeys.registered(), vec![(first, Hotkey::plain(vk::F23))]);
        hotkeys.unregister(first).expect("注销");
        assert_eq!(hotkeys.registered(), Vec::new());
    }

    #[test]
    fn multiple_keys_can_be_registered_and_addressed_independently() {
        let hotkeys = offline(Arc::default());
        let first = hotkeys.register(Hotkey::plain(vk::F23)).expect("首次");
        let second = hotkeys.register(Hotkey::plain(vk::F24)).expect("其次");
        assert_ne!(first, second);
        hotkeys.simulate_trigger(second).expect("fake 消息");
        assert_eq!(hotkeys.try_recv(), Some(HotkeyEvent { id: second, vk: vk::F24 }));
    }

    #[test]
    fn try_recv_is_non_blocking_when_nothing_happened() {
        let hotkeys = offline(Arc::default());
        hotkeys.register(Hotkey::plain(vk::F23)).expect("注册");
        assert_eq!(hotkeys.try_recv(), None);
    }

    #[test]
    fn drop_releases_hotkeys_so_they_can_be_registered_again() {
        let occupied = Arc::default();
        let first = offline(Arc::clone(&occupied));
        first.register(Hotkey::plain(vk::F24)).expect("注册");
        let second = offline(Arc::clone(&occupied));
        assert!(matches!(second.register(Hotkey::plain(vk::F24)),
            Err(HotkeyError::RegisterFailed { vk: vk::F24, code: 1409 })));
        drop(first);
        assert_eq!(second.register(Hotkey::plain(vk::F24)), Ok(1));
    }

    #[test]
    fn modifiers_always_include_norepeat() {
        // spec D6：长按不得连发，`MOD_NOREPEAT` 必须强制附加。
        let modifiers = Hotkey::new(vk::F8, 0x0002).modifiers_with_norepeat();
        assert_eq!(modifiers.0 & MOD_NOREPEAT, MOD_NOREPEAT);
        assert_eq!(modifiers.0 & 0x0002, 0x0002, "调用方的修饰键不能被覆盖");
    }
}
