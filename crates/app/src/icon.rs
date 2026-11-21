//! 窗口图标：源图在编译期嵌入，解码成 `egui::IconData`。
//!
//! 图标是**装饰性**的：解码失败就返回 `None`，窗口照常启动 ——
//! 不能因为一张图片让整个工具打不开。

/// 源图（300×300）。`include_bytes!` 保证运行时不需要任何外部文件。
const ICON_PNG: &[u8] = include_bytes!("../../../assets/icon.png");

/// 任务栏 / 窗口左上角图标；失败返回 `None`（调用方跳过 `.with_icon`）。
pub fn window_icon() -> Option<egui::IconData> {
    let (rgba, width, height) = decode_rgba8(ICON_PNG)?;
    Some(egui::IconData {
        rgba,
        width,
        height,
    })
}

/// PNG → RGBA8；任何异常都以 `None` 表示，不 panic。
fn decode_rgba8(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
    // 调色板/低位深/16 位统一归一成 8 位，下游只需处理四种 8 位色型。
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().ok()?;
    let mut buffer = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buffer).ok()?;
    let rgba = to_rgba8(frame.color_type, &buffer[..frame.buffer_size()])?;
    Some((rgba, frame.width, frame.height))
}

/// 补齐成 4 通道（缺 alpha 的补 255）。
fn to_rgba8(color_type: png::ColorType, pixels: &[u8]) -> Option<Vec<u8>> {
    // `normalize_to_color8` 之后索引色已被展开，出现即是前提被破坏。
    if color_type == png::ColorType::Indexed {
        return None;
    }
    let mut rgba = Vec::with_capacity(pixels.len() / channels(color_type) * 4);
    match color_type {
        png::ColorType::Rgba => rgba.extend_from_slice(pixels),
        png::ColorType::Rgb => {
            for rgb in pixels.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for &[luma, alpha] in pixels.as_chunks::<2>().0 {
                rgba.extend_from_slice(&[luma, luma, luma, alpha]);
            }
        }
        png::ColorType::Grayscale => {
            for &luma in pixels {
                rgba.extend_from_slice(&[luma, luma, luma, 255]);
            }
        }
        png::ColorType::Indexed => return None,
    }
    Some(rgba)
}

/// 每种色型的通道数（只用于预估容量）。
fn channels(color_type: png::ColorType) -> usize {
    match color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_icon_decodes_to_square_rgba() {
        let (rgba, width, height) = decode_rgba8(ICON_PNG).expect("内置图标必须能解码");
        assert_eq!((width, height), (300, 300), "源图尺寸");
        assert_eq!(rgba.len(), 300 * 300 * 4, "RGBA 字节数");
        let mut alphas = rgba.as_chunks::<4>().0.iter().map(|pixel| pixel[3]);
        assert!(alphas.any(|a| a == 0), "必须保留透明像素（圆角外）");
        assert!(alphas.any(|a| a == 255), "必须有不透明的主体");
    }

    #[test]
    fn broken_png_yields_none_instead_of_panicking() {
        assert!(decode_rgba8(b"not a png at all").is_none());
        assert!(window_icon().is_some(), "正常源图必须出图");
    }
}
