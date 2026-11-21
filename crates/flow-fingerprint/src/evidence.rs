//! 失败必留证（spec D10 缺陷③：上游失败分支部分无日志）。
//!
//! 约定：写 `logs/fail_<ts>.png`（画面）+ `logs/fail_<ts>.txt`（结构化原因）。
//! **落盘失败绝不让流程崩** —— 只返回 `None`，由调用方把「留证失败」记进 warnings。

use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use delta_capture::Frame;

/// 默认留证目录（相对当前工作目录）。
pub const EVIDENCE_DIR: &str = "logs";

/// 失败留证结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub image: PathBuf,
    pub note: PathBuf,
}

/// 保存失败证据。任何 IO / 编码错误都只返回 `None`。
pub fn save_failure(frame: &Frame, dir: &Path, reason: &str, detail: &str) -> Option<Evidence> {
    let stamp = timestamp();
    if fs::create_dir_all(dir).is_err() {
        return None;
    }
    let image = dir.join(format!("fail_{stamp}.png"));
    write_png(frame, &image).ok()?;

    let note = dir.join(format!("fail_{stamp}.txt"));
    let body = format!(
        "reason: {reason}\ndetail: {detail}\nsize: {}x{}\nat: {stamp}\n",
        frame.w, frame.h
    );
    // 文本写失败不影响图片证据可用性。
    let _ = fs::write(&note, body);
    Some(Evidence { image, note })
}

fn write_png(frame: &Frame, path: &Path) -> Result<(), String> {
    let file = fs::File::create(path).map_err(|error| error.to_string())?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), frame.w, frame.h);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().map_err(|error| error.to_string())?;

    let mut rgba = Vec::with_capacity((frame.w as usize) * (frame.h as usize) * 4);
    for pixel in frame.data.as_chunks::<4>().0 {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
    }
    writer
        .write_image_data(&rgba)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn frame(w: u32, h: u32) -> Frame {
        let mut data = vec![0; (w * h * 4) as usize];
        for (index, pixel) in data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            pixel[0] = (index % 251) as u8;
            pixel[3] = 255;
        }
        Frame::new(w, h, data, Instant::now()).expect("合法帧")
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("delta-fp-{tag}-{}", std::process::id()))
    }

    #[test]
    fn writes_png_and_note_into_fresh_directory() {
        let dir = temp_dir("evidence-ok");
        let _ = fs::remove_dir_all(&dir);

        let evidence = save_failure(&frame(16, 8), &dir, "NoRoleMatched", "库里没有角色")
            .expect("留证必须成功");

        assert!(evidence.image.exists(), "{}", evidence.image.display());
        assert!(evidence.note.exists());
        let bytes = fs::read(&evidence.image).expect("可读");
        assert!(bytes.len() > 50, "PNG 有内容：{} 字节", bytes.len());
        let note = fs::read_to_string(&evidence.note).expect("可读");
        assert!(note.contains("NoRoleMatched"), "{note}");
        assert!(note.contains("16x8"), "{note}");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unwritable_directory_returns_none_instead_of_panicking() {
        // 用一个「已存在的文件」当目录，create_dir_all 必失败。
        let path = std::env::temp_dir().join(format!("delta-fp-blocked-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::write(&path, b"x").expect("先造一个文件");

        assert_eq!(save_failure(&frame(4, 4), &path, "X", "Y"), None);
        let _ = fs::remove_file(&path);
    }
}
