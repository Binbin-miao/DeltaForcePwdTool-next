//! `Windows.Media.Ocr` 调用（**只在本模块的专用 MTA 线程里执行**）。
//!
//! 线程约束（浮光结论）：
//! - `OcrEngine` 绑定创建它的线程，必须常驻专用线程，**绝不能放在 UI 线程**；
//! - 线程内先 `CoInitializeEx(COINIT_MULTITHREADED)`，再 `RoInitialize`（`Foundation::Initialize`），
//!   顺序不能反；
//! - `IAsyncOperation::get()` 在 MTA 线程上可以阻塞取值；在 STA 上会死等消息泵。
//!
//! 图片通路：BGRA → 24bpp BMP → `InMemoryRandomAccessStream` → `BitmapDecoder`
//! → `SoftwareBitmap`（强制转 Bgra8/Premultiplied）→ `RecognizeAsync`。

use delta_capture::Frame;
use windows::core::HSTRING;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapPixelFormat};
use windows::Media::Ocr::OcrEngine as WinOcrEngine;
use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::bmp::encode_bmp_bgra;

use crate::OcrError;

/// 在调用线程上完成 COM(MTA) + WinRT 初始化。
pub(crate) fn init_apartment() -> Result<(), OcrError> {
    // SAFETY: 只影响当前线程的套间状态；S_FALSE（已初始化）视为成功。
    let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    if hr.is_err() {
        return Err(OcrError::WinRt {
            stage: "CoInitializeEx",
            code: hr.0,
        });
    }
    // SAFETY: 只影响当前线程的 WinRT 套间状态，本函数运行在专用线程上。
    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|error| OcrError::WinRt {
        stage: "RoInitialize",
        code: error.code().0,
    })
}

/// 按优先级创建引擎：指定语言 → `zh-Hans-CN` → `zh-CN` → 用户配置首选语言。
pub(crate) fn create_engine(preferred: Option<&str>) -> Result<WinOcrEngine, OcrError> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(language) = preferred {
        candidates.push(language);
    }
    candidates.extend(crate::FALLBACK_LANGUAGES);

    let mut last_error = None;
    for language in candidates {
        match WinOcrEngine::TryCreateFromLanguage(&language_tag(language)?) {
            Ok(engine) => return Ok(engine),
            Err(error) => last_error = Some(error),
        }
    }
    // 最后回落到系统默认第一项。
    match WinOcrEngine::TryCreateFromUserProfileLanguages() {
        Ok(engine) => Ok(engine),
        Err(_) => {
            Err(OcrError::NoLanguage {
                tried: preferred.unwrap_or_default().to_string(),
                code: last_error.map_or(0, |error| error.code().0),
            })
        }
    }
}

/// 只试一个语言，不做任何回落（供 UI「该语言是否可用」探测与测试使用）。
pub(crate) fn create_engine_exact(language: &str) -> Result<WinOcrEngine, OcrError> {
    // 语言标签非法与「语言包没装」对调用方是同一件事：该语言不可用。
    let not_available = |code: i32| OcrError::NoLanguage {
        tried: language.to_string(),
        code,
    };
    let tag = Language::CreateLanguage(&HSTRING::from(language))
        .map_err(|error| not_available(error.code().0))?;
    WinOcrEngine::TryCreateFromLanguage(&tag).map_err(|error| not_available(error.code().0))
}

fn language_tag(language: &str) -> Result<Language, OcrError> {
    Language::CreateLanguage(&HSTRING::from(language)).map_err(|error| OcrError::WinRt {
        stage: "Language::CreateLanguage",
        code: error.code().0,
    })
}

/// 识别一帧，返回所有行用空格拼接、去首尾空白的文本。
pub(crate) fn recognize(engine: &WinOcrEngine, frame: &Frame) -> Result<String, OcrError> {
    let bmp = encode_bmp_bgra(frame.w, frame.h, frame.stride, &frame.data)?;

    let stream = InMemoryRandomAccessStream::new().map_err(|_| winrt("InMemoryRandomAccessStream"))?;
    let writer =
        DataWriter::CreateDataWriter(&stream).map_err(|_| winrt("DataWriter::CreateDataWriter"))?;
    writer
        .WriteBytes(&bmp)
        .map_err(|_| winrt("DataWriter::WriteBytes"))?;
    // 阻塞取值：本函数运行在专用 MTA 线程上，不会卡 UI。
    writer
        .StoreAsync()
        .and_then(|operation| operation.get())
        .map_err(|_| winrt("DataWriter::StoreAsync"))?;
    writer
        .DetachStream()
        .map_err(|_| winrt("DataWriter::DetachStream"))?;
    stream
        .Seek(0)
        .map_err(|_| winrt("InMemoryRandomAccessStream::Seek"))?;

    let decoder = BitmapDecoder::CreateAsync(&stream)
        .and_then(|operation| operation.get())
        .map_err(|_| winrt("BitmapDecoder::CreateAsync"))?;
    // 解码即转成 Bgra8 / Premultiplied：`RecognizeAsync` 对像素格式有硬要求。
    let bitmap = decoder
        .GetSoftwareBitmapConvertedAsync(BitmapPixelFormat::Bgra8, BitmapAlphaMode::Premultiplied)
        .and_then(|operation| operation.get())
        .map_err(|_| winrt("BitmapDecoder::GetSoftwareBitmapConvertedAsync"))?;

    let result = engine
        .RecognizeAsync(&bitmap)
        .and_then(|operation| operation.get())
        .map_err(|_| winrt("OcrEngine::RecognizeAsync"))?;

    let lines = result.Lines().map_err(|_| winrt("OcrResult::Lines"))?;
    let mut parts = Vec::new();
    for line in lines {
        let text = line.Text().map_err(|_| winrt("OcrLine::Text"))?;
        let text = text.to_string();
        if !text.is_empty() {
            parts.push(text);
        }
    }
    Ok(parts.join(" ").trim().to_string())
}

fn winrt(stage: &'static str) -> OcrError {
    OcrError::WinRt { stage, code: 0 }
}
