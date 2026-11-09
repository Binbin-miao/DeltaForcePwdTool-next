//! Windows Graphics Capture 后端（spec §2「WGC 实现链路」）。
//!
//! ```text
//! D3D11CreateDevice(HARDWARE, BGRA) → ID3D11Device + ID3D11DeviceContext
//!   → QueryInterface<IDXGIDevice> → CreateDirect3D11DeviceFromDXGIDevice → IDirect3DDevice
//!   → IGraphicsCaptureItemInterop::CreateForMonitor(主屏) → GraphicsCaptureItem
//!   → Direct3D11CaptureFramePool::CreateFreeThreaded(.., B8G8R8A8UIntNormalized, 2, item.Size)
//!   → pool.CreateCaptureSession(item) → SetIsCursorCaptureEnabled(false) → StartCapture
//! grab(rect) → TryGetNextFrame → Surface → ID3D11Texture2D
//!   → staging texture(CPU_READ) → CopyResource → Map → 逐行拷 rect → Unmap
//! ```
//!
//! 生命周期契约（spec D3）：设备 / 帧池 / 采集会话全部由 [`WgcSession`] 持有，`Drop` 时关闭；
//! 初始化失败返回带 HRESULT 的可读错误，**绝不静默回退 BitBlt**（spec D5 边界）。

use std::ffi::c_void;
use std::time::{Duration, Instant};

use delta_geom::PxRect;
use windows::core::{factory, Interface};
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::{IDirect3DDevice, IDirect3DSurface};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Foundation::{E_POINTER, HMODULE, POINT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11DeviceFromDXGIDevice;
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::{CaptureError, Frame};

/// 帧池双缓冲即可：我们只按需单次取帧，不追帧率。
const FRAME_POOL_BUFFERS: i32 = 2;
/// 无新帧时的重试次数与每次等待（spec §2「已知风险」：允许重试 3 次、每次 30ms）。
const FRAME_WAIT_ATTEMPTS: u32 = 3;
const FRAME_WAIT_MS: u64 = 30;

// WGC 帧的 `IDirect3DSurface` 不能直接 QueryInterface 成 `ID3D11Texture2D`（返回 E_NOINTERFACE
// 0x80004002），必须经 interop 接口 `IDirect3DDxgiInterfaceAccess::GetInterface` 取底层资源。
// windows-rs 0.58 没有生成这个接口，因此在这里按 IID 补一份最小声明（只有第 4 个虚函数有用）。
#[repr(transparent)]
#[derive(PartialEq, Eq, Clone)]
struct IDirect3DDxgiInterfaceAccess(windows::core::IUnknown);

unsafe impl Interface for IDirect3DDxgiInterfaceAccess {
    type Vtable = IDirect3DDxgiInterfaceAccess_Vtbl;
    const IID: windows::core::GUID =
        windows::core::GUID::from_u128(0xa9b3d012_3df2_4ee3_b8d1_8695f457d3c1);
}

impl std::ops::Deref for IDirect3DDxgiInterfaceAccess {
    type Target = windows::core::IUnknown;

    fn deref(&self) -> &Self::Target {
        unsafe { std::mem::transmute(self) }
    }
}

// COM 虚表字段名必须与原生方法名一致，才能与头文件逐行对照。
#[allow(non_snake_case)]
#[repr(C)]
struct IDirect3DDxgiInterfaceAccess_Vtbl {
    pub base__: windows::core::IUnknown_Vtbl,
    pub GetInterface: unsafe extern "system" fn(
        *mut c_void,
        *const windows::core::GUID,
        *mut *mut c_void,
    ) -> windows::core::HRESULT,
}

impl IDirect3DDxgiInterfaceAccess {
    /// 取回底层 DXGI/D3D11 接口（`T` 通常是 `ID3D11Texture2D`）。
    unsafe fn get_interface<T: Interface>(&self) -> windows::core::Result<T> {
        let mut result = std::ptr::null_mut();
        (Interface::vtable(self).GetInterface)(Interface::as_raw(self), &T::IID, &mut result)
            .and_then(|| unsafe { windows::core::Type::from_abi(result) })
    }
}

/// 从一个 WinRT 表面拿到底层 D3D11 纹理。
fn dxgi_texture(surface: &IDirect3DSurface) -> windows::core::Result<ID3D11Texture2D> {
    let access: IDirect3DDxgiInterfaceAccess = surface.cast()?;
    unsafe { access.get_interface() }
}

/// 一次任务内的 WGC 采集对象集合。
pub struct WgcSession {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
}

impl WgcSession {
    /// 拉起 D3D11 设备与主屏采集会话；任何一步失败都返回带 HRESULT 的可读错误。
    pub fn new() -> Result<Self, CaptureError> {
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        init_step(
            "D3D11CreateDevice",
            unsafe {
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            },
        )?;
        let device = device.ok_or_else(|| invalid_pointer("D3D11CreateDevice 未返回设备"))?;
        let context =
            context.ok_or_else(|| invalid_pointer("D3D11CreateDevice 未返回立即上下文"))?;

        let dxgi_device: IDXGIDevice = cast_step("QueryInterface<IDXGIDevice>", &device)?;
        let inspectable = init_step(
            "CreateDirect3D11DeviceFromDXGIDevice",
            unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device) },
        )?;
        let winrt_device: IDirect3DDevice = cast_step("IInspectable→IDirect3DDevice", &inspectable)?;

        let interop: IGraphicsCaptureItemInterop = factory::<GraphicsCaptureItem, _>().map_err(
            |error| CaptureError::WgcInit {
                step: "GraphicsCaptureItem 工厂",
                code: error.code().0,
            },
        )?;
        let monitor = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
        let item: GraphicsCaptureItem = init_step(
            "IGraphicsCaptureItemInterop::CreateForMonitor",
            unsafe { interop.CreateForMonitor(monitor) },
        )?;
        let size = init_step("GraphicsCaptureItem::Size", item.Size())?;
        let pool = init_step(
            "Direct3D11CaptureFramePool::CreateFreeThreaded",
            Direct3D11CaptureFramePool::CreateFreeThreaded(
                &winrt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                FRAME_POOL_BUFFERS,
                size,
            ),
        )?;
        let session = init_step(
            "CreateCaptureSession",
            pool.CreateCaptureSession(&item),
        )?;
        // 必须关闭光标：否则光标被画进帧里污染识别。
        init_step(
            "SetIsCursorCaptureEnabled",
            session.SetIsCursorCaptureEnabled(false),
        )?;
        init_step("StartCapture", session.StartCapture())?;

        Ok(Self {
            device,
            context,
            pool,
            session,
        })
    }

    /// 取一帧并拷出 `rect` 区域（32bpp top-down BGRA）。
    pub fn grab(&mut self, rect: PxRect) -> Result<Frame, CaptureError> {
        if rect.w <= 0 || rect.h <= 0 {
            return Err(CaptureError::InvalidRect {
                w: rect.w,
                h: rect.h,
            });
        }
        let frame = self.next_frame()?;
        let surface: IDirect3DSurface =
            grab_step("Direct3D11CaptureFrame::Surface", frame.Surface())?;
        let texture: ID3D11Texture2D = grab_step(
            "IDirect3DDxgiInterfaceAccess::GetInterface",
            dxgi_texture(&surface),
        )?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(CaptureError::WgcUnsupported {
                detail: "帧纹理格式不是 B8G8R8A8_UNORM",
            });
        }
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;

        let mut staging: Option<ID3D11Texture2D> = None;
        grab_step(
            "CreateTexture2D(staging)",
            unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut staging)) },
        )?;
        let staging = staging.ok_or_else(|| invalid_grab("CreateTexture2D 未返回纹理"))?;

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context.CopyResource(&staging, &texture);
            grab_step(
                "Map",
                self.context
                    .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)),
            )?;
        }
        let copied = copy_region(&mapped, desc.Width, desc.Height, rect);
        unsafe {
            self.context.Unmap(&staging, 0);
        }
        // 立刻把帧还给帧池，避免下一次取帧时池里全是未释放的旧帧。
        let _ = frame.Close();

        Frame::new(
            rect.w as u32,
            rect.h as u32,
            copied?,
            Instant::now(),
        )
    }

    /// 取下一帧：无新帧时按 spec 允许的节奏重试，仍无帧则返回明确错误（不 panic）。
    fn next_frame(&mut self) -> Result<Direct3D11CaptureFrame, CaptureError> {
        for attempt in 1..=FRAME_WAIT_ATTEMPTS {
            if let Ok(frame) = self.pool.TryGetNextFrame() {
                return Ok(frame);
            }
            if attempt < FRAME_WAIT_ATTEMPTS {
                // 流程等待（等游戏侧出帧），不是「轮询截图」：单次最多 60ms（2 次间隔），且不抓任何像素。
                std::thread::sleep(Duration::from_millis(FRAME_WAIT_MS));
            }
        }
        Err(CaptureError::WgcNoFrame {
            attempts: FRAME_WAIT_ATTEMPTS,
            // 只在重试之间等待，最后一次失败后不再 sleep。
            wait_ms: FRAME_WAIT_MS * u64::from(FRAME_WAIT_ATTEMPTS - 1),
        })
    }
}

impl Drop for WgcSession {
    fn drop(&mut self) {
        // 关掉采集会话与帧池：程序空闲时不得持有任何采集对象（spec D3）。
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

/// 把映射出来的整屏缓冲按行拷出 `rect` 区域（越界即报错，不做静默裁剪）。
fn copy_region(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    frame_w: u32,
    frame_h: u32,
    rect: PxRect,
) -> Result<Vec<u8>, CaptureError> {
    let inside = rect.x >= 0
        && rect.y >= 0
        && rect.w > 0
        && rect.h > 0
        && rect.x + rect.w <= frame_w as i32
        && rect.y + rect.h <= frame_h as i32;
    if !inside {
        return Err(CaptureError::RectOutOfBounds {
            x: rect.x,
            y: rect.y,
            w: rect.w,
            h: rect.h,
            frame_w,
            frame_h,
        });
    }
    let row_bytes = Frame::buffer_len(rect.w as u32, 1);
    let mut data = vec![0u8; row_bytes * rect.h as usize];
    let source = mapped.pData.cast::<u8>();
    for row in 0..rect.h as usize {
        let offset = (rect.y as usize + row) * mapped.RowPitch as usize + rect.x as usize * 4;
        let target = &mut data[row * row_bytes..(row + 1) * row_bytes];
        unsafe {
            std::ptr::copy_nonoverlapping(source.add(offset), target.as_mut_ptr(), row_bytes);
        }
    }
    Ok(data)
}

fn init_step<T>(step: &'static str, result: windows::core::Result<T>) -> Result<T, CaptureError> {
    result.map_err(|error| CaptureError::WgcInit {
        step,
        code: error.code().0,
    })
}

fn grab_step<T>(step: &'static str, result: windows::core::Result<T>) -> Result<T, CaptureError> {
    result.map_err(|error| CaptureError::WgcGrab {
        step,
        code: error.code().0,
    })
}

fn cast_step<T, U>(step: &'static str, source: &T) -> Result<U, CaptureError>
where
    T: Interface,
    U: Interface,
{
    source.cast::<U>().map_err(|error| CaptureError::WgcInit {
        step,
        code: error.code().0,
    })
}

fn invalid_pointer(step: &'static str) -> CaptureError {
    CaptureError::WgcInit {
        step,
        code: E_POINTER.0,
    }
}

fn invalid_grab(step: &'static str) -> CaptureError {
    CaptureError::WgcGrab {
        step,
        code: E_POINTER.0,
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::c_void;

    use super::*;

    /// 造一块「整屏」BGRA 缓冲，按行号填充，便于校验逐行拷贝的偏移。
    fn mapped_canvas(width: u32, height: u32, row_pitch: u32) -> (Vec<u8>, D3D11_MAPPED_SUBRESOURCE) {
        let mut canvas = vec![0u8; row_pitch as usize * height as usize];
        for row in 0..height as usize {
            for column in 0..width as usize {
                let offset = row * row_pitch as usize + column * 4;
                // B = 行号, G = 列号, R/A 固定，便于断言「取到的是哪一行哪一列」。
                canvas[offset] = row as u8;
                canvas[offset + 1] = column as u8;
                canvas[offset + 2] = 0x7F;
                canvas[offset + 3] = 0xFF;
            }
        }
        let mapped = D3D11_MAPPED_SUBRESOURCE {
            pData: canvas.as_mut_ptr().cast::<c_void>(),
            RowPitch: row_pitch,
            DepthPitch: row_pitch * height,
        };
        (canvas, mapped)
    }

    #[test]
    fn copy_region_copies_the_requested_window_row_by_row() {
        // 整屏 16×8，行距刻意比 16*4 更宽（模拟 GPU 的对齐填充）。
        let (_canvas, mapped) = mapped_canvas(16, 8, 16 * 4 + 32);
        let rect = PxRect {
            x: 3,
            y: 2,
            w: 5,
            h: 4,
        };
        let data = copy_region(&mapped, 16, 8, rect).expect("区域在帧内");
        assert_eq!(data.len(), 5 * 4 * 4);
        // 逐行校验：行首像素的 B 通道是该像素在整屏里的行号，G 通道是列号。
        for row in 0..4usize {
            let row_start = row * 5 * 4;
            assert_eq!(data[row_start], (2 + row) as u8, "第 {row} 行行号错位");
            assert_eq!(data[row_start + 1], 3u8, "第 {row} 行列号错位");
            let last = row_start + 4 * 4;
            assert_eq!(data[last + 1], 7u8, "第 {row} 行末像素列号错位");
        }
    }

    #[test]
    fn copy_region_rejects_windows_outside_the_frame() {
        let (_canvas, mapped) = mapped_canvas(16, 8, 16 * 4);
        let error = copy_region(
            &mapped,
            16,
            8,
            PxRect {
                x: 12,
                y: 0,
                w: 8,
                h: 4,
            },
        )
        .expect_err("越界必须报错而不是静默裁剪");
        assert!(
            matches!(error, CaptureError::RectOutOfBounds { frame_w: 16, frame_h: 8, .. }),
            "{error}"
        );
        assert!(error.to_string().contains("超出帧范围"), "{error}");
    }

    #[test]
    fn wgc_session_either_starts_or_reports_an_hresult() {
        match WgcSession::new() {
            Ok(_session) => {}
            Err(error) => {
                let text = error.to_string();
                assert!(text.contains("HRESULT 0x"), "{text}");
            }
        }
    }
}
