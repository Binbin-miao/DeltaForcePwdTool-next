//! GDI 常驻资源与抓屏实现（t2 任务 2/3/4）。
//!
//! 生命周期：`Capturer::new` 建立 屏幕 DC + 内存 DC + 位图，之后按需扩容量但**不重复创建**；
//! 位图在两次 `grab` 之间保持「未选入 DC」状态，满足 `GetDIBits` 的前置条件。

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

/// 常驻 DC + 位图的抓屏器。**单线程独占**（GDI DC 不是线程安全的）。
#[derive(Debug)]
pub struct Capturer {
    screen_dc: HDC,
    mem_dc: HDC,
    bitmap: Option<HBITMAP>,
    capacity: (i32, i32),
    buffer: Vec<u8>,
    info: BITMAPINFO,
}

impl Capturer {
    /// 建立常驻资源，并按 `max_width × max_height` 预分配位图。
    pub fn new(max_width: i32, max_height: i32) -> Result<Self, CaptureError> {
        // DPI 感知必须在任何 GetDC 之前完成（spec D9）。
        dpi::ensure_dpi_awareness();
        let screen_dc = unsafe { GetDC(HWND::default()) };
        if screen_dc.0.is_null() {
            return Err(CaptureError::NoScreenDc {
                code: last_error(),
            });
        }
        let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
        if mem_dc.0.is_null() {
            unsafe {
                let _ = ReleaseDC(HWND::default(), screen_dc);
            }
            return Err(win32_error("CreateCompatibleDC"));
        }
        let mut capturer = Self {
            screen_dc,
            mem_dc,
            bitmap: None,
            capacity: (0, 0),
            buffer: Vec::new(),
            info: BITMAPINFO::default(),
        };
        capturer.ensure_capacity(max_width.max(1), max_height.max(1))?;
        Ok(capturer)
    }

    /// 当前位图容量（宽, 高），用于验证「复用而不重建」。
    pub fn capacity(&self) -> (i32, i32) {
        self.capacity
    }

    /// 抓取一块区域（32bpp top-down BGRA）。
    pub fn grab(&mut self, rect: PxRect) -> Result<Frame, CaptureError> {
        if rect.w <= 0 || rect.h <= 0 {
            return Err(CaptureError::InvalidRect {
                w: rect.w,
                h: rect.h,
            });
        }
        self.ensure_capacity(rect.w, rect.h)?;
        let bitmap = self
            .bitmap
            .ok_or(CaptureError::InvalidRect { w: rect.w, h: rect.h })?;

        unsafe {
            let previous = SelectObject(self.mem_dc, HGDIOBJ(bitmap.0));
            let blended = BitBlt(
                self.mem_dc,
                0,
                0,
                rect.w,
                rect.h,
                self.screen_dc,
                rect.x,
                rect.y,
                SRCCOPY,
            );
            // 先脱离 DC 再 GetDIBits：位图被选入 DC 时 GetDIBits 会失败。
            SelectObject(self.mem_dc, previous);
            blended.map_err(|_| win32_error("BitBlt"))?;
        }

        let row_bytes = Frame::buffer_len(rect.w as u32, 1);
        let total = Frame::buffer_len(rect.w as u32, rect.h as u32);
        debug_assert!(row_bytes <= total);
        let scanned = unsafe {
            GetDIBits(
                self.mem_dc,
                bitmap,
                0,
                rect.h as u32,
                Some(self.buffer.as_mut_ptr().cast::<c_void>()),
                &mut self.info,
                DIB_RGB_COLORS,
            )
        };
        if scanned == 0 {
            return Err(win32_error("GetDIBits"));
        }
        Frame::new(
            rect.w as u32,
            rect.h as u32,
            self.buffer[..total].to_vec(),
            Instant::now(),
        )
    }

    /// 扩容位图（仅在请求区域大于现有容量时发生）。
    fn ensure_capacity(&mut self, width: i32, height: i32) -> Result<(), CaptureError> {
        let reusable = self.bitmap.is_some() && self.capacity.0 >= width && self.capacity.1 >= height;
        if reusable {
            return Ok(());
        }
        let width = width.max(self.capacity.0).max(1);
        let height = height.max(self.capacity.1).max(1);
        let bitmap = unsafe { CreateCompatibleBitmap(self.screen_dc, width, height) };
        if bitmap.0.is_null() {
            return Err(win32_error("CreateCompatibleBitmap"));
        }
        if let Some(previous) = self.bitmap.replace(bitmap) {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(previous.0));
            }
        }
        self.buffer = vec![0u8; Frame::buffer_len(width as u32, height as u32)];
        self.info = bitmap_info(width, height);
        self.capacity = (width, height);
        Ok(())
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        // 空句柄上的 Delete/Release 只会返回错误码，直接忽略即可（无需判空分支）。
        unsafe {
            if let Some(bitmap) = self.bitmap.take() {
                let _ = DeleteObject(HGDIOBJ(bitmap.0));
            }
            let _ = DeleteDC(self.mem_dc);
            let _ = ReleaseDC(HWND::default(), self.screen_dc);
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

    #[test]
    fn bitmap_info_is_32bpp_top_down_bgra() {
        let info = bitmap_info(320, 200);
        assert_eq!(info.bmiHeader.biSize, std::mem::size_of::<BITMAPINFOHEADER>() as u32);
        assert_eq!(info.bmiHeader.biBitCount, 32);
        assert_eq!(info.bmiHeader.biPlanes, 1);
        assert_eq!(info.bmiHeader.biCompression, BI_RGB_VALUE);
        assert_eq!(info.bmiHeader.biHeight, -200, "负高度 = top-down");
        assert_eq!(info.bmiHeader.biWidth, 320);
    }

    #[test]
    fn capturer_allocates_once_and_reuses_bitmap() {
        let mut capturer = Capturer::new(64, 64).expect("创建常驻 GDI 资源");
        assert_eq!(capturer.capacity(), (64, 64));

        let first = capturer
            .grab(PxRect {
                x: 0,
                y: 0,
                w: 32,
                h: 32,
            })
            .expect("抓取小区域");
        assert_eq!((first.w, first.h, first.stride), (32, 32, 128));
        assert_eq!(first.data.len(), 32 * 32 * 4);
        assert_eq!(capturer.capacity(), (64, 64), "小区域不得重建位图");

        // 扩到大区域：只扩一次，之后再次复用。
        capturer
            .grab(PxRect {
                x: 0,
                y: 0,
                w: 128,
                h: 96,
            })
            .expect("抓取大区域");
        assert_eq!(capturer.capacity(), (128, 96));
        capturer
            .grab(PxRect {
                x: 0,
                y: 0,
                w: 100,
                h: 80,
            })
            .expect("复用扩容后的位图");
        assert_eq!(capturer.capacity(), (128, 96));
    }

    #[test]
    fn empty_rect_is_rejected_without_touching_gdi() {
        let mut capturer = Capturer::new(8, 8).expect("创建常驻 GDI 资源");
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
    fn primary_monitor_size_is_positive() {
        let monitor = primary_monitor_size().expect("主显示器尺寸可读");
        assert!(monitor.is_calibrated(), "{monitor:?}");
    }

    #[test]
    #[ignore = "手动运行：抓 100x100 区域 100 次，断言均值 <1ms（t2 任务 8）"]
    fn grab_100x100_benchmark() {
        let mut capturer = Capturer::new(100, 100).expect("创建常驻 GDI 资源");
        let rect = PxRect {
            x: 0,
            y: 0,
            w: 100,
            h: 100,
        };
        let rounds = 100u32;
        let start = Instant::now();
        for _ in 0..rounds {
            let frame = capturer.grab(rect).expect("抓取");
            assert_eq!(frame.data.len(), 100 * 100 * 4);
        }
        let per_frame = start.elapsed() / rounds;
        assert!(
            per_frame < std::time::Duration::from_millis(1),
            "单帧均值 {per_frame:?} 超出 1ms 预算"
        );
    }
}
