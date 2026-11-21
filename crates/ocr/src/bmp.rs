//! BMP 编码（纯函数，可离线单测）。
//!
//! 为什么不直接喂 `SoftwareBitmap::Create`：那需要手工构造 WinRT `IBuffer`（要
//! `Win32_System_WinRT` 的互操作）。把 BGRA 包成一张 24bpp BMP 再走
//! `InMemoryRandomAccessStream` + `BitmapDecoder` 是纯 WinRT 路径，少一层 interop，
//! 也顺带给了我们一个可以完全离线断言的编码器。

use crate::OcrError;

/// 14 字节文件头 + 40 字节 `BITMAPINFOHEADER`。
const FILE_HEADER_LEN: u32 = 14;
const INFO_HEADER_LEN: u32 = 40;

/// 把 BGRA（`BitBlt`+`GetDIBits` 的 top-down 输出）编成 24bpp BMP。
///
/// - BMP 是 **bottom-up**，所以要倒序写行；
/// - 每行按 4 字节对齐，不足补 0；
/// - 像素顺序 BGR（丢 alpha）。
pub fn encode_bmp_bgra(w: u32, h: u32, stride: u32, data: &[u8]) -> Result<Vec<u8>, OcrError> {
    if w == 0 || h == 0 {
        return Err(OcrError::EmptyFrame { w, h });
    }
    let row_bytes = w.checked_mul(3).ok_or(OcrError::EmptyFrame { w, h })?;
    let padding = (4 - (row_bytes % 4) % 4) % 4;
    let padded_row = row_bytes + padding;
    let pixel_bytes = padded_row
        .checked_mul(h)
        .ok_or(OcrError::EmptyFrame { w, h })?;
    let source_row = (w as usize) * 4;
    let stride = stride.max(source_row as u32) as usize;
    if data.len() < stride * (h as usize) {
        return Err(OcrError::BufferTooShort {
            expected: stride * (h as usize),
            actual: data.len(),
        });
    }

    let mut out = Vec::with_capacity((FILE_HEADER_LEN + INFO_HEADER_LEN + pixel_bytes) as usize);
    let file_size = FILE_HEADER_LEN + INFO_HEADER_LEN + pixel_bytes;
    let offset = FILE_HEADER_LEN + INFO_HEADER_LEN;

    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&file_size.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // bfReserved1
    out.extend_from_slice(&0u16.to_le_bytes()); // bfReserved2
    out.extend_from_slice(&offset.to_le_bytes());

    // BITMAPINFOHEADER
    out.extend_from_slice(&INFO_HEADER_LEN.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // 正高 = bottom-up
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&24u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&pixel_bytes.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes()); // 72 DPI
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    for y in (0..h as usize).rev() {
        let row = &data[y * stride..y * stride + source_row];
        for pixel in row.as_chunks::<4>().0 {
            out.push(pixel[0]); // B
            out.push(pixel[1]); // G
            out.push(pixel[2]); // R
        }
        out.extend(std::iter::repeat_n(0u8, padding as usize));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2x2 四色（BGRA）：验证倒序写行、BGR 顺序、丢 alpha。
    #[test]
    fn encodes_bottom_up_bgr_rows() {
        let data = vec![
            // 第 0 行（top）：蓝、绿
            255, 0, 0, 255, 0, 255, 0, 255, //
            // 第 1 行：红、白
            0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let bmp = encode_bmp_bgra(2, 2, 8, &data).expect("编码");
        assert_eq!(&bmp[0..2], b"BM");
        assert_eq!(
            u32::from_le_bytes(bmp[2..6].try_into().unwrap()) as usize,
            bmp.len(),
            "文件头里的尺寸必须与真实长度一致"
        );
        let offset = u32::from_le_bytes(bmp[10..14].try_into().unwrap()) as usize;
        assert_eq!(offset, 54);

        // 每行 2*3 = 6 字节，对齐到 8（补 2）。
        let pixels = &bmp[offset..];
        assert_eq!(pixels.len(), 16);
        // 底行先写：红、白
        assert_eq!(&pixels[0..3], &[0, 0, 255]);
        assert_eq!(&pixels[3..6], &[255, 255, 255]);
        assert_eq!(&pixels[6..8], &[0, 0], "行内补齐必须为零");
        // 再写顶行：蓝、绿
        assert_eq!(&pixels[8..11], &[255, 0, 0]);
        assert_eq!(&pixels[11..14], &[0, 255, 0]);
    }

    /// 宽度是 4 的倍数时（1920*3 = 5760）不需要补齐。
    #[test]
    fn no_padding_when_row_is_already_aligned() {
        let data = vec![7u8; 4 * 4 * 4];
        let bmp = encode_bmp_bgra(4, 4, 16, &data).expect("编码");
        let offset = 54;
        assert_eq!(bmp.len() - offset, 4 * 4 * 3, "4 像素宽无需补齐");
    }

    #[test]
    fn rejects_empty_and_short_buffers() {
        assert!(matches!(
            encode_bmp_bgra(0, 0, 0, &[]),
            Err(OcrError::EmptyFrame { .. })
        ));
        let error = encode_bmp_bgra(4, 4, 16, &[0u8; 8]).expect_err("缓冲不足");
        assert!(matches!(error, OcrError::BufferTooShort { .. }));
    }

    /// stride > w*4（GDI 行填充）时只取有效像素。
    #[test]
    fn skips_stride_padding() {
        let mut data = Vec::new();
        for _ in 0..2 {
            data.extend_from_slice(&[1, 2, 3, 255]); // 一个像素
            data.extend_from_slice(&[9, 9, 9, 9, 9, 9, 9, 9]); // 行填充
        }
        let bmp = encode_bmp_bgra(1, 2, 12, &data).expect("编码");
        let pixels = &bmp[54..];
        assert_eq!(&pixels[0..3], &[1, 2, 3]);
        assert_eq!(&pixels[4..7], &[1, 2, 3], "填充字节不得混进像素");
    }
}
