//! BitBlt 兼容后端：**每次 `grab` 新建 GDI 资源、作用域结束即释放**（spec D2「禁止常驻」）。
//!
//! 单次抓取的生命周期（全部在 [`capture`] 的栈上完成）：
//! `GetDC` → `CreateCompatibleDC` → `CreateCompatibleBitmap` → `SelectObject` → `BitBlt`
//! → `SelectObject` 复位 → `GetDIBits` → `DeleteObject` / `DeleteDC` / `ReleaseDC`。
//!
//! 本模块**没有任何持有 `HDC` / `HBITMAP` 的结构体字段**，因此不可能跨调用累积 GDI 对象：
//! 「程序空闲时零图形资源」是类型结构保证，而不是纪律要求。

use std::ffi::c_void;
use std::time::Instant;

use delta_geom::{MonitorInfo, PxRect};
use windows::Win32::Foundation::{GetLastError, HWND};
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
    SRCCOPY,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

use crate::{dpi, CaptureError, Frame};

/// `BI_RGB`（无压缩）在 Windows SDK 里的字面值；直接用常量避免不同 windows-rs 版本的类型差异。
const BI_RGB_VALUE: u32 = 0;

/// 抓屏器的兼容壳：上层调用点仍写 `Capturer::new(..)` / `grab(..)`，但**没有**任何常驻资源。
#[derive(Debug, Default)]
pub struct Capturer;

impl Capturer {
    /// 只做一次性的 DPI 感知；动态库上限参数在「每次新建」模型下已无意义，仅保留签名兼容。
    pub fn new(_max_width: i32, _max_height: i32) -> Result<Self, CaptureError> {
        dpi::ensure_dpi_awareness();
        Ok(Self)
    }

    /// 抓取一块区域（32bpp top-down BGRA）。
    pub fn grab(&mut self, rect: PxRect) -> Result<Frame, CaptureError> {
        capture(rect)
    }
}

/// 抓取一块区域（32bpp top-down BGRA）；返回前已释放全部 GDI 句柄。
pub fn capture(rect: PxRect) -> Result<Frame, CaptureError> {
    if rect.w <= 0 || rect.h <= 0 {
        return Err(CaptureError::InvalidRect {
            w: rect.w,
            h: rect.h,
        });
    }
    // DPI 感知必须早于任何 GetDC（幂等，OnceLock）。
    dpi::ensure_dpi_awareness();

    let mut scope = GdiScope::acquire(rect.w, rect.h)?;
    unsafe {
        let blended = BitBlt(
            scope.memory,
            0,
            0,
            rect.w,
            rect.h,
            scope.screen,
            rect.x,
            rect.y,
            SRCCOPY,
        );
        // 先脱离 DC 再 GetDIBits：位图被选入 DC 时 GetDIBits 会失败。
        scope.deselect();
        blended.map_err(|_| win32_error("BitBlt"))?;
    }

    let mut info = bitmap_info(rect.w, rect.h);
    let total = Frame::buffer_len(rect.w as u32, rect.h as u32);
    let mut buffer = vec![0u8; total];
    let scanned = unsafe {
        GetDIBits(
            scope.memory,
            scope.bitmap,
            0,
            rect.h as u32,
            Some(buffer.as_mut_ptr().cast::<c_void>()),
            &mut info,
            DIB_RGB_COLORS,
        )
    };
    if scanned == 0 {
        return Err(win32_error("GetDIBits"));
    }
    Frame::new(rect.w as u32, rect.h as u32, buffer, Instant::now())
}

/// 一次抓取内的 GDI 句柄集合：`Drop` 按创建逆序释放，任何提前返回都不会泄漏。
struct GdiScope {
    screen: HDC,
    memory: HDC,
    bitmap: HBITMAP,
    bitmap_previous: HGDIOBJ,
    bitmap_selected: bool,
}

impl GdiScope {
    /// 申请「屏幕 DC + 内存 DC + 位图」并选入内存 DC。
    fn acquire(width: i32, height: i32) -> Result<Self, CaptureError> {
        let screen = unsafe { GetDC(HWND::default()) };
        if screen.0.is_null() {
            return Err(CaptureError::NoScreenDc {
                code: last_error(),
            });
        }
        let memory = unsafe { CreateCompatibleDC(screen) };
        if memory.0.is_null() {
            unsafe {
                let _ = ReleaseDC(HWND::default(), screen);
            }
            return Err(win32_error("CreateCompatibleDC"));
        }
        let bitmap = unsafe { CreateCompatibleBitmap(screen, width, height) };
        if bitmap.0.is_null() {
            unsafe {
                let _ = DeleteDC(memory);
                let _ = ReleaseDC(HWND::default(), screen);
            }
            return Err(win32_error("CreateCompatibleBitmap"));
        }
        let mut scope = Self {
            screen,
            memory,
            bitmap,
            bitmap_previous: HGDIOBJ::default(),
            bitmap_selected: false,
        };
        scope.select();
        Ok(scope)
    }

    fn select(&mut self) {
        self.bitmap_previous = unsafe { SelectObject(self.memory, HGDIOBJ(self.bitmap.0)) };
        self.bitmap_selected = true;
    }

    fn deselect(&mut self) {
        if self.bitmap_selected {
            unsafe { SelectObject(self.memory, self.bitmap_previous) };
            self.bitmap_selected = false;
        }
    }
}

impl Drop for GdiScope {
    fn drop(&mut self) {
        self.deselect();
        // 句柄必然非空（构建失败时不会返回 Self），因此无需判空分支。
        unsafe {
            let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
            let _ = DeleteDC(self.memory);
            let _ = ReleaseDC(HWND::default(), self.screen);
        }
    }
}

/// 主显示器物理分辨率（已应用 DPI 感知，返回物理像素）。
pub fn primary_monitor_size() -> Result<MonitorInfo, CaptureError> {
    dpi::ensure_dpi_awareness();
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    MonitorInfo::new(width, height).map_err(CaptureError::Geom)
}

/// 32bpp top-down BGRA 的 `BITMAPINFO`（`biHeight` 为负 = 顶行为首）。
fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB_VALUE,
            ..BITMAPINFOHEADER::default()
        },
        ..BITMAPINFO::default()
    }
}

fn last_error() -> u32 {
    unsafe { GetLastError().0 }
}

fn win32_error(api: &'static str) -> CaptureError {
    CaptureError::Win32 {
        api,
        code: last_error(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Threading::{GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS};

    fn gdi_object_count() -> u32 {
        unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) }
    }

    #[test]
    fn bitmap_info_is_32bpp_top_down_bgra() {
        let info = bitmap_info(320, 200);
        assert_eq!(
            info.bmiHeader.biSize,
            std::mem::size_of::<BITMAPINFOHEADER>() as u32
        );
        assert_eq!(info.bmiHeader.biBitCount, 32);
        assert_eq!(info.bmiHeader.biPlanes, 1);
        assert_eq!(info.bmiHeader.biCompression, BI_RGB_VALUE);
        assert_eq!(info.bmiHeader.biHeight, -200, "负高度 = top-down");
        assert_eq!(info.bmiHeader.biWidth, 320);
    }

    #[test]
    fn every_grab_returns_a_fresh_buffer_without_resident_state() {
        let mut capturer = Capturer::new(64, 64).expect("创建抓屏器");
        let rect = PxRect {
            x: 0,
            y: 0,
            w: 32,
            h: 24,
        };
        let first = capturer.grab(rect).expect("抓取小区域");
        assert_eq!((first.w, first.h, first.stride), (32, 24, 128));
        assert_eq!(first.data.len(), 32 * 24 * 4);

        // 大小随请求变化 —— 没有「预分配容量」这类跨调用状态可以约束它。
        let second = capturer
            .grab(PxRect {
                x: 0,
                y: 0,
                w: 100,
                h: 80,
            })
            .expect("抓取大区域");
        assert_eq!((second.w, second.h), (100, 80));
        assert_eq!(second.data.len(), 100 * 80 * 4);
    }

    #[test]
    fn empty_rect_is_rejected_without_touching_gdi() {
        let mut capturer = Capturer::new(8, 8).expect("创建抓屏器");
        assert!(matches!(
            capturer.grab(PxRect {
                x: 0,
                y: 0,
                w: 0,
                h: 4
            }),
            Err(CaptureError::InvalidRect { w: 0, h: 4 })
        ));
    }

    #[test]
    fn fifty_grabs_do_not_leak_gdi_objects() {
        let rect = PxRect {
            x: 0,
            y: 0,
            w: 8,
            h: 8,
        };
        // 预热一次，避免把首次惰性分配算进增长。
        capture(rect).expect("抓取");
        let before = gdi_object_count();
        for round in 0..50 {
            capture(rect).unwrap_or_else(|error| panic!("第 {round} 次抓取失败：{error}"));
        }
        let after = gdi_object_count();
        assert!(
            after <= before,
            "常驻 GDI 对象必须零增长，实际 {before} → {after}"
        );
    }

    #[test]
    fn primary_monitor_size_is_positive() {
        let monitor = primary_monitor_size().expect("主显示器尺寸可读");
        assert!(monitor.is_calibrated(), "{monitor:?}");
    }

    #[test]
    #[ignore = "手动运行：抓 100x100 区域 100 次，断言均值 <2ms（每次新建 GDI 资源，比常驻略慢）"]
    fn grab_100x100_benchmark() {
        let rect = PxRect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        };
        let rounds = 100u32;
        let start = Instant::now();
        for _ in 0..rounds {
            let frame = capture(rect).expect("抓取");
            assert_eq!(frame.data.len(), 100 * 100 * 4);
        }
        let per_frame = start.elapsed() / rounds;
        assert!(
            per_frame < std::time::Duration::from_millis(2),
            "单帧均值 {per_frame:?} 超出 2ms 预算"
        );
    }
}
