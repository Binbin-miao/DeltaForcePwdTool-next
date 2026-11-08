//! `Windows.Media.Ocr` 系统 OCR 封装（spec D2 兜底引擎，**零模型**）。
//!
//! 契约：
//! - 引擎是**进程级单例**，常驻一条专用 MTA 线程（浮光结论：`OcrEngine` 绑线程，
//!   `IAsyncOperation::get()` 在 STA 上会死等消息泵）。调用方永远不在自己线程碰 WinRT；
//! - **每个进程只允许一条 WinRT 线程**：实测第二次 `RoInitialize` + 建引擎会触发
//!   `STATUS_ACCESS_VIOLATION`（0xC0000005），所以换语言也在这条常驻线程上重建引擎，
//!   绝不新开线程；
//! - 调用方通过 channel 请求/应答，**>500ms 直接返回 [`OcrError::Timeout`]**，
//!   线程即使卡死也不传染 UI；
//! - 引擎不可用 / 超时 / 任何 WinRT 错误都返回 [`OcrError`]，不 panic；
//!   指纹流程据此退回纯模板投票（spec D2）；
//! - 不引入 `tesseract` / `ocrs` / `rapidocr` 等任何模型依赖。

mod bmp;
mod winrt;

use std::sync::mpsc::{sync_channel, Receiver, Sender, SyncSender};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use delta_capture::Frame;
use thiserror::Error;

pub use bmp::encode_bmp_bgra;

/// 默认语言（spec D2）。
pub const DEFAULT_LANGUAGE: &str = "zh-Hans-CN";
/// 语言回落链：请求的语言 → `zh-Hans-CN` → `zh-CN` → 系统首选。
pub const FALLBACK_LANGUAGES: [&str; 2] = ["zh-Hans-CN", "zh-CN"];
/// 单次识别超时上限（spec D2 / t6 任务 4）。
pub const RECOGNIZE_TIMEOUT: Duration = Duration::from_millis(500);
/// 引擎初始化超时（首次 WinRT 激活要加载 OCR 语言包，比识别慢）。
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(5);

/// OCR 错误。每种退化路径都有独立变体，调用方必须能继续跑。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum OcrError {
    #[error("没有可用的 OCR 语言包（尝试过 {tried}，错误码 {code}）")]
    NoLanguage { tried: String, code: i32 },
    #[error("空帧：{w}x{h}")]
    EmptyFrame { w: u32, h: u32 },
    #[error("帧缓冲不足：需要 {expected} 字节，实际 {actual}")]
    BufferTooShort { expected: usize, actual: usize },
    #[error("WinRT 调用 {stage} 失败（错误码 {code}）")]
    WinRt { stage: &'static str, code: i32 },
    #[error("OCR 超时（{0:?}）")]
    Timeout(Duration),
    #[error("OCR 线程已退出，无法再识别")]
    EngineGone,
}

enum Bootstrap {
    Preferred(String),
    Exact(String),
}

struct Request {
    w: u32,
    h: u32,
    stride: u32,
    data: Vec<u8>,
    reply: SyncSender<Result<String, OcrError>>,
}

enum Command {
    Switch {
        requested: Bootstrap,
        reply: SyncSender<Result<String, OcrError>>,
    },
    Recognize(Request),
}

/// 常驻线程句柄（进程内唯一）。
struct EngineCore {
    commands: Sender<Command>,
}

static CORE: OnceLock<Result<EngineCore, OcrError>> = OnceLock::new();

impl EngineCore {
    /// 首次调用时启动专用 MTA 线程；失败结果同样被缓存（探测只做一次）。
    fn shared() -> Result<&'static Self, OcrError> {
        CORE.get_or_init(Self::boot).as_ref().map_err(Clone::clone)
    }

    fn boot() -> Result<Self, OcrError> {
        let (boot_tx, boot_rx) = sync_channel::<Result<String, OcrError>>(1);
        let (command_tx, commands) = std::sync::mpsc::channel::<Command>();
        std::thread::Builder::new()
            .name("delta-ocr".to_string())
            .spawn(move || engine_thread(boot_tx, commands))
            .map_err(|_| OcrError::EngineGone)?;
        match boot_rx.recv_timeout(BOOT_TIMEOUT) {
            Ok(Ok(_language)) => Ok(Self {
                commands: command_tx,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => Err(OcrError::Timeout(BOOT_TIMEOUT)),
        }
    }

    /// 在常驻线程上重建引擎（同一线程，不新开线程）。
    fn switch(&self, requested: Bootstrap) -> Result<String, OcrError> {
        let (reply_tx, reply_rx) = sync_channel(1);
        self.commands
            .send(Command::Switch {
                requested,
                reply: reply_tx,
            })
            .map_err(|_| OcrError::EngineGone)?;
        reply_rx
            .recv_timeout(BOOT_TIMEOUT)
            .map_err(|_| OcrError::Timeout(BOOT_TIMEOUT))?
    }

    fn recognize(&self, frame: &Frame) -> Result<String, OcrError> {
        let (reply_tx, reply_rx) = sync_channel(1);
        self.commands
            .send(Command::Recognize(Request {
                w: frame.w,
                h: frame.h,
                stride: frame.stride,
                data: frame.data.clone(),
                reply: reply_tx,
            }))
            .map_err(|_| OcrError::EngineGone)?;
        reply_rx
            .recv_timeout(RECOGNIZE_TIMEOUT)
            .map_err(|_| OcrError::Timeout(RECOGNIZE_TIMEOUT))?
    }
}

/// OCR 引擎句柄。**不拥有线程**：线程随进程存活，句柄可随意 clone / drop。
#[derive(Clone)]
pub struct OcrEngine {
    core: &'static EngineCore,
    language: String,
}

impl OcrEngine {
    /// 按 spec D2 的回落链取引擎。`lang` 传 [`DEFAULT_LANGUAGE`] 即为默认行为。
    pub fn new(lang: &str) -> Result<Self, OcrError> {
        let core = EngineCore::shared()?;
        let language = core.switch(Bootstrap::Preferred(lang.to_string()))?;
        Ok(Self { core, language })
    }

    /// 只尝试这一个语言，**不回落**（UI 探测与测试用）。
    pub fn new_exact(lang: &str) -> Result<Self, OcrError> {
        let core = EngineCore::shared()?;
        let language = core.switch(Bootstrap::Exact(lang.to_string()))?;
        Ok(Self { core, language })
    }

    /// 实际生效的语言标签（UI 留证：到底回落到了哪个语言包）。
    pub fn language(&self) -> &str {
        &self.language
    }

    /// 识别一帧。超过 [`RECOGNIZE_TIMEOUT`] 立即返回 [`OcrError::Timeout`]。
    pub fn recognize(&self, frame: &Frame) -> Result<String, OcrError> {
        self.core.recognize(frame)
    }

    /// 一次性缓存可用性探测，供 UI 展示「OCR 兜底：可用/不可用」。
    pub fn is_available() -> bool {
        EngineCore::shared().is_ok()
    }
}

/// 专用 MTA 线程主体：初始化套间 → 建引擎 → 常驻服务请求。
fn engine_thread(boot: SyncSender<Result<String, OcrError>>, commands: Receiver<Command>) {
    if let Err(error) = winrt::init_apartment() {
        let _ = boot.send(Err(error));
        return;
    }
    let mut engine = match winrt::create_engine(Some(DEFAULT_LANGUAGE)) {
        Ok(engine) => engine,
        Err(error) => {
            let _ = boot.send(Err(error));
            return;
        }
    };
    let mut language = actual_language(&engine, DEFAULT_LANGUAGE);
    if boot.send(Ok(language.clone())).is_err() {
        return;
    }

    while let Ok(command) = commands.recv() {
        match command {
            Command::Switch { requested, reply } => {
                let candidate = match requested {
                    Bootstrap::Preferred(requested) => winrt::create_engine(Some(&requested)),
                    Bootstrap::Exact(requested) => winrt::create_engine_exact(&requested),
                };
                let outcome = match candidate {
                    Ok(fresh) => {
                        language = actual_language(&fresh, &language);
                        engine = fresh;
                        Ok(language.clone())
                    }
                    // 换语言失败时保留原引擎，调用方只收到错误。
                    Err(error) => Err(error),
                };
                let _ = reply.send(outcome);
            }
            Command::Recognize(request) => {
                let outcome =
                    recognize_parts(&engine, request.w, request.h, request.stride, &request.data);
                let _ = request.reply.send(outcome);
            }
        }
    }
}

fn actual_language(engine: &windows::Media::Ocr::OcrEngine, fallback: &str) -> String {
    engine
        .RecognizerLanguage()
        .and_then(|language| language.LanguageTag())
        .map_or_else(|_| fallback.to_string(), |tag| tag.to_string())
}

fn recognize_parts(
    engine: &windows::Media::Ocr::OcrEngine,
    w: u32,
    h: u32,
    stride: u32,
    data: &[u8],
) -> Result<String, OcrError> {
    // `Frame` 按 w*4 紧密排列；GDI 的行填充必须先剥掉，否则解码结果会斜切。
    let row_bytes = (w as usize) * 4;
    let stride = (stride as usize).max(row_bytes);
    let mut packed = Vec::with_capacity(row_bytes * h as usize);
    for row in data.chunks_exact(stride).take(h as usize) {
        packed.extend_from_slice(&row[..row_bytes]);
    }
    let frame =
        Frame::new(w, h, packed, Instant::now()).map_err(|_| OcrError::EmptyFrame { w, h })?;
    winrt::recognize(engine, &frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 可用性探测绝不能 panic —— 缺语言包的机器上必须安静地返回 false。
    #[test]
    fn is_available_never_panics_and_is_cached() {
        let first = OcrEngine::is_available();
        let second = OcrEngine::is_available();
        assert_eq!(first, second, "命中 OnceLock 缓存，两次结果必须一致");
    }

    /// 不存在的语言必须返回具名错误 `NoLanguage`，不得 panic 也不得静默成功。
    #[test]
    fn unavailable_language_returns_named_error() {
        let error = OcrEngine::new_exact("zz-ZZ")
            .err()
            .expect("不存在的语言必须报错");
        assert!(
            matches!(error, OcrError::NoLanguage { .. }),
            "必须是 NoLanguage，实际 {error:?}"
        );
    }

    /// 换语言失败之后，默认引擎必须仍然可用（失败不得污染已有引擎）。
    #[test]
    fn failed_switch_keeps_the_previous_engine_usable() {
        if !OcrEngine::is_available() {
            eprintln!("跳过：本机没有可用的 OCR 语言包");
            return;
        }
        let _ = OcrEngine::new_exact("zz-ZZ");
        let engine = OcrEngine::new(DEFAULT_LANGUAGE).expect("默认引擎必须仍可用");
        assert!(!engine.language().is_empty());
    }

    /// 空帧/坏帧在任何引擎状态下都必须是普通错误，不能崩。
    #[test]
    fn empty_frame_is_an_error_not_a_panic() {
        let error = encode_bmp_bgra(0, 0, 0, &[]).expect_err("空帧");
        assert!(matches!(error, OcrError::EmptyFrame { .. }));
    }

    /// 引擎可用时，完整识别链路（BMP 编码 → 流 → 解码 → Bgra8 → RecognizeAsync）必须跑通。
    ///
    /// **实测留证**：Windows OCR 对合成七段数字图返回空串（它按文本行切分，不认"灯管"形状），
    /// 因此这里只断言「跑通且不 panic」。字符级准确率需要真实游戏截图，属于 spec §4 的整体
    /// 验收门禁（t7/flow 层），不在 t6 的夹具能力范围内。
    #[test]
    fn recognizes_synthetic_frame_without_error() {
        if !OcrEngine::is_available() {
            eprintln!("跳过：本机没有可用的 OCR 语言包");
            return;
        }
        let engine = OcrEngine::new(DEFAULT_LANGUAGE).expect("引擎必须可用");
        let (w, h, data) = render_digits("123");
        let frame = Frame::new(w, h, data, Instant::now()).expect("合成帧");
        let text = engine.recognize(&frame).expect("识别链路必须跑通");
        eprintln!("合成七段图识别结果：{text:?}");
    }

    /// 纯白帧必须能跑通而不报错（零字符也要正常返回空串）。
    #[test]
    fn blank_frame_returns_ok_when_engine_is_available() {
        if !OcrEngine::is_available() {
            eprintln!("跳过：本机没有可用的 OCR 语言包");
            return;
        }
        let engine = OcrEngine::new(DEFAULT_LANGUAGE).expect("引擎");
        let (w, h) = (200u32, 100u32);
        let frame = Frame::new(w, h, vec![255u8; (w * h * 4) as usize], Instant::now()).expect("帧");
        let text = engine.recognize(&frame).expect("纯白帧不应报错");
        assert!(text.trim().is_empty(), "纯白帧不该识别出文字：{text:?}");
    }

    /// 七段数码管风格的合成图（比 5x7 点阵更好被 OCR 读出来）。
    fn render_digits(text: &str) -> (u32, u32, Vec<u8>) {
        const DIGIT_W: u32 = 50;
        const DIGIT_H: u32 = 90;
        const BAR: u32 = 10;
        const GAP: u32 = 25;
        const MARGIN: u32 = 20;
        let count = text.chars().count() as u32;
        let w = MARGIN * 2 + DIGIT_W * count + GAP * (count.saturating_sub(1));
        let h = MARGIN * 2 + DIGIT_H;
        let mut canvas = vec![255u8; (w * h * 4) as usize]; // 白底

        let mut fill = |x0: u32, y0: u32, x1: u32, y1: u32| {
            for y in y0..y1.min(h) {
                for x in x0..x1.min(w) {
                    let base = ((y * w + x) * 4) as usize;
                    canvas[base..base + 4].copy_from_slice(&[0, 0, 0, 255]);
                }
            }
        };

        for (index, digit) in text.chars().enumerate() {
            let ox = MARGIN + index as u32 * (DIGIT_W + GAP);
            let oy = MARGIN;
            let segments: &[u8] = match digit {
                '0' => b"abcdef",
                '1' => b"bc",
                '2' => b"abdeg",
                '3' => b"abcdg",
                '4' => b"bcfg",
                '5' => b"acdfg",
                '6' => b"acdefg",
                '7' => b"abc",
                '8' => b"abcdefg",
                '9' => b"abcdfg",
                other => panic!("不支持的字符 {other:?}"),
            };
            for segment in segments {
                let (x0, y0, x1, y1) = match segment {
                    b'a' => (ox + BAR, oy, ox + DIGIT_W - BAR, oy + BAR),
                    b'b' => (
                        ox + DIGIT_W - BAR,
                        oy + BAR,
                        ox + DIGIT_W,
                        oy + DIGIT_H / 2,
                    ),
                    b'c' => (
                        ox + DIGIT_W - BAR,
                        oy + DIGIT_H / 2,
                        ox + DIGIT_W,
                        oy + DIGIT_H - BAR,
                    ),
                    b'd' => (ox + BAR, oy + DIGIT_H - BAR, ox + DIGIT_W - BAR, oy + DIGIT_H),
                    b'e' => (ox, oy + DIGIT_H / 2, ox + BAR, oy + DIGIT_H - BAR),
                    b'f' => (ox, oy + BAR, ox + BAR, oy + DIGIT_H / 2),
                    b'g' => (
                        ox + BAR,
                        oy + DIGIT_H / 2 - BAR / 2,
                        ox + DIGIT_W - BAR,
                        oy + DIGIT_H / 2 + BAR / 2,
                    ),
                    _ => unreachable!(),
                };
                fill(x0, y0, x1, y1);
            }
        }
        (w, h, canvas)
    }

    #[test]
    fn render_digits_produces_valid_bgra_frame() {
        let (w, h, data) = render_digits("07");
        assert_eq!(data.len(), (w * h * 4) as usize);
        assert!(
            data.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 255),
            "alpha 必须不透明"
        );
        assert!(
            data.as_chunks::<4>().0.iter().any(|pixel| pixel[0] == 0),
            "必须真的画了黑像素"
        );
    }
}
