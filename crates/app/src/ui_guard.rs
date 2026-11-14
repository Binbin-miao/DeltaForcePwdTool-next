//! Q5 第二道闸：**解锁界面 UI 判定模板**（用户拍板：「解锁密码界面用 UI 判定模板」）。
//!
//! 语义是**正样本**：命中一张模板 = 「这就是某个解锁界面」，可以直接按该场景开跑；
//! 全部模板都远不相似 = 「这不是解锁界面」，零注入当场拒绝。
//! （早期版本是负样本——命中背包就拒绝——方向反了：`scene` 的结构阈值本来就分不开
//! 「背包/仓库/结算」和「密码台」，负样本只能一个个补补给站，永远补不完。）
//!
//! 机制（一条流水线，无分支）：
//! 1. 整帧按同一 ROI（`x[25..75%], y[8..62%]`）裁出 —— 这块**故意避开左下角昵称/UID**；
//! 2. 缩放到固定 [`TEMPLATE_W`]×[`TEMPLATE_H`] 灰度缩略图（模板就是按同一口径切的）；
//! 3. 与 `images/ui/*.png` 逐个做归一化互相关（NCC），取最高分 `best` 与次高分 `second`：
//!    `best ≥ MIN_TARGET_NCC` 且 `best - second ≥ MIN_MARGIN` ⇒ [`UiVerdict::Target`]；
//!    `best < MAX_REJECT_NCC` ⇒ [`UiVerdict::Reject`]；中间灰带 ⇒ [`UiVerdict::Gray`]。
//!
//! 阈值实测标定（1280×720 录屏逐帧，模板取自 `images/ui/`）：
//!
//! | 类别 | 样本 | best NCC |
//! |---|---|---|
//! | 同界面·面板在时 | morse 1.8/3.2/4.5/6.0/7.5s | 0.825 ~ 1.000 |
//! | 同界面·面板在时 | keys 83.0/84.0/85.5s | 0.962 ~ 1.000 |
//! | 同界面·面板在时 | line 26.5/27.5/29.0s | 0.803 ~ 1.000 |
//! | 交叉异界面 | morse↔keys↔line | -0.035 ~ 0.053 |
//! | 真实负样本 | `evidence/` 3 张失败截图 | 0.040 ~ 0.110 |
//! | 视频非面板时刻 | 0.03/8.6/9.2/55/70s | 0.020 ~ 0.136 |
//! | 面板已消失 | morse 8.0s / keys 86.5s / line 31.0s | -0.060 ~ 0.193 |
//!
//! ⇒ [`MIN_TARGET_NCC`] = 0.75（同界面下限 0.803，留 0.053 余量）；
//! ⇒ [`MAX_REJECT_NCC`] = 0.30（负样本上限 0.136，留 0.164 余量）；
//! ⇒ [`MIN_MARGIN`] = 0.05（异界面最高才 0.053，两界面同时命中就交回结构判别）。
//!
//! 失败即放行：模板目录不存在 / 为空 / 全是没登记的文件名 → [`UiVerdict::Gray`]，
//! **绝不允许因为模板缺失而拒绝识别**（那会把工具变成永久失效）。

use std::path::Path;

use delta_capture::Frame;
use delta_scene::Scene;
use delta_templates::decode_png_gray;
use delta_vision::{GrayImage, Region};

/// 缩略图尺寸：所有模板与运行时缩略图都是这个口径。
pub const TEMPLATE_W: u32 = 200;
/// 缩略图尺寸（高）。
pub const TEMPLATE_H: u32 = 112;
/// 整帧 ROI（左、上、右、下，均为整帧比例）：避开昵称/UID 与底部 HUD。
pub const ROI: (f32, f32, f32, f32) = (0.25, 0.08, 0.75, 0.62);
/// 认定「就是这个解锁界面」的下限（NCC）：同界面实测最低 0.803。
pub const MIN_TARGET_NCC: f32 = 0.75;
/// 认定「绝不是解锁界面」的上限（NCC）：真实负样本实测最高 0.136。
pub const MAX_REJECT_NCC: f32 = 0.30;
/// 冠亚军最小差距：低于它就说不清是哪个界面，交回结构判别。
pub const MIN_MARGIN: f32 = 0.05;
/// 拒绝文案（对外只说这一句：不点名背包/仓库，免得又变成穷举）。
pub const NOT_UNLOCK_SCREEN: &str = "这不是解锁界面";

/// 模板文件名 → 场景。新增解锁界面 = 这张表加一行 + 往 `images/ui/` 放一张图。
const SCENE_OF_TEMPLATE: [(&str, Scene); 3] = [
    ("morse", Scene::Morse),
    ("keys", Scene::Console),
    ("line", Scene::Console),
];

/// 模板文件名对应的场景（`None` = 没登记，跳过该文件，绝不 panic）。
fn scene_of(name: &str) -> Option<Scene> {
    SCENE_OF_TEMPLATE
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, scene)| *scene)
}

/// 一道闸的三种结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiVerdict {
    /// 这就是某个解锁界面 ⇒ 免掉场景投票，直接按它开跑。
    Target(Scene),
    /// 一张都不像 ⇒ 不是解锁界面，零注入。
    Reject(&'static str),
    /// 灰带 / 空库 / 冠亚军分不开 ⇒ 交回 `scene::detect`。
    Gray,
}

/// 两帧复核的结论（spec B15/B17：强证据路由之后，再用同一套模板判一次）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recheck {
    /// 两帧命中同一个界面 ⇒ 继续。
    Keep(Scene),
    /// 第二帧判不清（灰带 / 反向不像）⇒ 降级交回 `detect` 重判。
    Downgrade,
    /// 第二帧命中了另一个界面 ⇒ 界面在识别过程中变了，零注入。
    Changed,
}

/// 复核裁决：只比较两次 [`UiVerdict`]（纯函数，微秒级，不碰帧也不碰盘）。
///
/// 第二帧直接用 `run_once` 本来就要用的那一帧，所以这是**零额外抓帧**的保险：
/// `Strong` 只在单帧上成立时，一次画面变化就能让「认出来的界面」变成误点。
pub fn recheck(first: UiVerdict, second: UiVerdict) -> Recheck {
    let UiVerdict::Target(expected) = first else {
        // 第一次就没靠模板（灰带 / 拒绝）⇒ 没有「双重确认」可言。
        return Recheck::Downgrade;
    };
    match second {
        UiVerdict::Target(seen) if seen == expected => Recheck::Keep(expected),
        UiVerdict::Target(_) => Recheck::Changed,
        UiVerdict::Gray | UiVerdict::Reject(_) => Recheck::Downgrade,
    }
}

/// 模板库。空库 = 「没有可用模板」，[`UiGuard::verdict`] 恒 [`UiVerdict::Gray`]。
#[derive(Debug, Default)]
pub struct UiGuard {
    entries: Vec<(String, GrayImage)>,
}

impl UiGuard {
    /// 从目录加载 `*.png`（文件名即界面名）。任何一层失败都只让库变小，不报错。
    pub fn load(dir: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return Self::default();
        };
        let mut templates: Vec<(String, GrayImage)> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| is_png(path))
            .filter_map(|path| {
                let name = path.file_stem()?.to_str()?.to_owned();
                Some((name, decode_png_gray(&path).ok()?))
            })
            .collect();
        // 顺序稳定：命中多个时按名字定，日志可复现。
        templates.sort_by(|left, right| left.0.cmp(&right.0));
        Self { entries: templates }
    }

    /// 一张**登记过场景**的模板都没有（⇒ 这道闸不生效，连帧都不必抓）。
    pub fn is_empty(&self) -> bool {
        !self.entries.iter().any(|(name, _)| scene_of(name).is_some())
    }

    /// 判定这一帧是哪个解锁界面。
    ///
    /// 只读：不写盘、不改内部状态（读路径零副作用）。
    pub fn verdict(&self, frame: &Frame) -> UiVerdict {
        let Some(thumb) = thumbnail(frame) else {
            return UiVerdict::Gray;
        };
        let Some((name, best, second)) = self.best_of(&thumb) else {
            return UiVerdict::Gray;
        };
        eprintln!("[ui_guard] best={best:.3} second={second:.3} hit={name}");
        if best < MAX_REJECT_NCC {
            return UiVerdict::Reject(NOT_UNLOCK_SCREEN);
        }
        match scene_of(name) {
            Some(scene) if best >= MIN_TARGET_NCC && best - second >= MIN_MARGIN => {
                UiVerdict::Target(scene)
            }
            _ => UiVerdict::Gray,
        }
    }

    /// 打分最高且登记过场景的模板：`(名字, 最高分, 次高分)`。
    ///
    /// 次高分下限按 0 计（异界面实测为负），宁可比真实值偏大一点，让灰带更宽。
    fn best_of(&self, thumb: &GrayImage) -> Option<(&str, f32, f32)> {
        let mut top: Option<(&str, f32)> = None;
        let mut runner_up = 0.0f32;
        for (name, template) in &self.entries {
            if scene_of(name).is_none() {
                continue;
            }
            let value = score(thumb, template);
            if top.is_none_or(|(_, best)| value > best) {
                runner_up = top.map_or(runner_up, |(_, best)| runner_up.max(best));
                top = Some((name.as_str(), value));
            } else {
                runner_up = runner_up.max(value);
            }
        }
        top.map(|(name, best)| (name, best, runner_up))
    }
}

/// 整帧 ROI → 像素矩形 `(x0, y0, x1, y1)`，右下开区间。
fn roi_rect(w: u32, h: u32) -> (u32, u32, u32, u32) {
    let (left, top, right, bottom) = ROI;
    let x0 = (w as f32 * left) as u32;
    let y0 = (h as f32 * top) as u32;
    let x1 = ((w as f32 * right) as u32).clamp(x0 + 1, w);
    let y1 = ((h as f32 * bottom) as u32).clamp(y0 + 1, h);
    (x0, y0, x1, y1)
}

/// 整帧 ROI → 200×112 灰度缩略图。
fn thumbnail(frame: &Frame) -> Option<GrayImage> {
    let (x0, y0, x1, y1) = roi_rect(frame.w, frame.h);
    let region = Region {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    };
    frame
        .luma()
        .crop(region)
        .ok()?
        .resize_to(TEMPLATE_W, TEMPLATE_H)
        .ok()
}

/// 模板与缩略图的相似度：先把模板归一化到缩略图尺寸，再算一次 Pearson 相关系数。
///
/// 不用 `delta_templates::match_cell`：那是「小模板在大图上滑窗」的口径
/// （200×112 的核在大图上滑一遍 ≈2.5 亿次乘加，几十毫秒级），
/// 这里两者**已经**对齐到同一尺寸、同一 ROI，只需要一个位置的相关系数。
fn score(thumb: &GrayImage, template: &GrayImage) -> f32 {
    let Ok(template) = template.resize_to(thumb.w, thumb.h) else {
        return 0.0;
    };
    ncc(thumb, &template)
}

/// 同尺寸两张灰度图的归一化互相关（-1..1）；任一张方差为 0 时返回 0（不判命中）。
fn ncc(left: &GrayImage, right: &GrayImage) -> f32 {
    if left.w != right.w || left.h != right.h {
        return 0.0;
    }
    let count = f64::from(left.w * left.h);
    if count <= 0.0 {
        return 0.0;
    }
    let (mut sum_l, mut sum_r) = (0.0f64, 0.0f64);
    for (l, r) in left.as_slice().iter().zip(right.as_slice()) {
        sum_l += f64::from(*l);
        sum_r += f64::from(*r);
    }
    let (mean_l, mean_r) = (sum_l / count, sum_r / count);
    let (mut cov, mut var_l, mut var_r) = (0.0f64, 0.0f64, 0.0f64);
    for (l, r) in left.as_slice().iter().zip(right.as_slice()) {
        let (dl, dr) = (f64::from(*l) - mean_l, f64::from(*r) - mean_r);
        cov += dl * dr;
        var_l += dl * dl;
        var_r += dr * dr;
    }
    let denominator = (var_l * var_r).sqrt();
    if denominator <= f64::EPSILON {
        return 0.0;
    }
    (cov / denominator) as f32
}

fn is_png(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::Instant;

    const W: u32 = 400;
    const H: u32 = 300;

    /// 仓库里真实的 `images/ui/`：用编译期 `CARGO_MANIFEST_DIR` 定位，
    /// 不依赖 exe 旁边的副本（测试不跑打包流程），也不碰 `.scratch/`。
    fn templates_dir() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../images/ui")
    }

    fn real_templates() -> Vec<(String, GrayImage)> {
        let guard = UiGuard::load(&templates_dir());
        assert_eq!(guard.entries.len(), 3, "images/ui 里应有三张正样本模板");
        guard.entries
    }

    fn template_of(name: &str) -> GrayImage {
        real_templates()
            .into_iter()
            .find(|(entry, _)| entry == name)
            .map(|(_, image)| image)
            .unwrap_or_else(|| panic!("缺少模板 {name}.png"))
    }

    /// 把 200×112 的模板按 [`ROI`] 铺回一整帧（等价于「屏幕上就是这张界面」）。
    fn frame_showing(template: &GrayImage) -> Frame {
        let (x0, y0, x1, y1) = roi_rect(W, H);
        let source = template.resize_to(TEMPLATE_W, TEMPLATE_H).expect("模板口径");
        let mut data = vec![0u8; (W * H * 4) as usize];
        for ty in 0..TEMPLATE_H {
            for tx in 0..TEMPLATE_W {
                let value = source.as_slice()[(ty * TEMPLATE_W + tx) as usize];
                let xa = x0 + tx * (x1 - x0) / TEMPLATE_W;
                let xb = x0 + (tx + 1) * (x1 - x0) / TEMPLATE_W;
                let ya = y0 + ty * (y1 - y0) / TEMPLATE_H;
                let yb = y0 + (ty + 1) * (y1 - y0) / TEMPLATE_H;
                for y in ya..yb {
                    for x in xa..xb {
                        let index = ((y * W + x) * 4) as usize;
                        data[index..index + 4].copy_from_slice(&[value, value, value, 255]);
                    }
                }
            }
        }
        Frame::new(W, H, data, Instant::now()).expect("合成帧")
    }

    /// 均匀纯色帧（方差为 0）。
    fn flat_frame(value: u8) -> Frame {
        Frame::new(64, 64, vec![value; 64 * 64 * 4], Instant::now()).expect("合成帧")
    }

    #[test]
    fn every_registered_template_name_maps_to_its_scene() {
        assert_eq!(scene_of("morse"), Some(Scene::Morse));
        assert_eq!(scene_of("keys"), Some(Scene::Console));
        assert_eq!(scene_of("line"), Some(Scene::Console));
        assert_eq!(scene_of("背包"), None, "没登记的名字不许映射");
    }

    #[test]
    fn the_three_real_templates_share_one_decode_and_size_convention() {
        for (name, template) in real_templates() {
            assert_eq!(
                (template.w, template.h),
                (TEMPLATE_W, TEMPLATE_H),
                "{name}.png 必须是 {TEMPLATE_W}×{TEMPLATE_H} 灰度图"
            );
            let same = ncc(&template, &template);
            assert!(same >= 0.95, "{name} 自比 {same:.3}，说明解码/口径不一致");
        }
    }

    #[test]
    fn templates_of_different_screens_barely_correlate() {
        let templates = real_templates();
        for (left_name, left) in &templates {
            for (right_name, right) in &templates {
                if left_name == right_name {
                    continue;
                }
                let value = score(left, right);
                assert!(
                    value < MAX_REJECT_NCC,
                    "{left_name} vs {right_name} = {value:.3}，异界面必须远低于拒绝线"
                );
            }
        }
    }

    #[test]
    fn a_real_template_on_screen_becomes_a_target() {
        let guard = UiGuard::load(&templates_dir());
        assert_eq!(
            guard.verdict(&frame_showing(&template_of("morse"))),
            UiVerdict::Target(Scene::Morse)
        );
        assert_eq!(
            guard.verdict(&frame_showing(&template_of("keys"))),
            UiVerdict::Target(Scene::Console)
        );
        assert_eq!(
            guard.verdict(&frame_showing(&template_of("line"))),
            UiVerdict::Target(Scene::Console)
        );
    }

    #[test]
    fn templates_of_another_size_are_normalised_before_scoring() {
        // 模板来自另一台机器/另一次缩放：只要版面对得上，尺寸不同也要命中。
        let guard = UiGuard {
            entries: vec![(
                "morse".to_owned(),
                template_of("morse").resize_to(100, 56).expect("缩到一半"),
            )],
        };
        assert_eq!(
            guard.verdict(&frame_showing(&template_of("morse"))),
            UiVerdict::Target(Scene::Morse)
        );
    }

    #[test]
    fn an_unknown_template_name_is_skipped_without_breaking_the_rest() {
        let guard = UiGuard {
            entries: vec![
                ("unknown".to_owned(), template_of("morse")),
                ("morse".to_owned(), template_of("morse")),
            ],
        };
        assert!(!guard.is_empty(), "还有一张登记过的模板");
        assert_eq!(
            guard.verdict(&frame_showing(&template_of("morse"))),
            UiVerdict::Target(Scene::Morse),
            "没登记的文件名跳过，不许影响 morse"
        );
    }

    #[test]
    fn a_library_of_only_unknown_names_disables_the_gate() {
        let guard = UiGuard {
            entries: vec![("unknown".to_owned(), template_of("morse"))],
        };
        assert!(guard.is_empty(), "没有登记过场景的模板 ⇒ 这道闸不生效");
        assert_eq!(
            guard.verdict(&frame_showing(&template_of("morse"))),
            UiVerdict::Gray
        );
    }

    #[test]
    fn an_empty_library_falls_back_to_structure_detection() {
        let guard = UiGuard::load(Path::new("这个目录不存在/也不会有模板"));
        assert!(guard.is_empty());
        assert_eq!(
            guard.verdict(&frame_showing(&template_of("morse"))),
            UiVerdict::Gray,
            "没有模板就一切照旧"
        );
        assert_eq!(
            UiGuard::default().verdict(&frame_showing(&template_of("morse"))),
            UiVerdict::Gray
        );
    }

    #[test]
    fn a_flat_screen_never_becomes_a_target() {
        let guard = UiGuard::load(&templates_dir());
        // 零方差 ⇒ 相似度恒 0 ⇒ 明确落在拒绝区，但绝不是 Target。
        assert_eq!(
            guard.verdict(&flat_frame(128)),
            UiVerdict::Reject(NOT_UNLOCK_SCREEN)
        );
    }

    #[test]
    fn a_low_variance_screen_never_becomes_a_target() {
        let guard = UiGuard::load(&templates_dir());
        let mut data = vec![0u8; (W * H * 4) as usize];
        for y in 0..H {
            for x in 0..W {
                // 4 级灰度、无版面结构：只够制造方差，不足以像任何界面。
                let value = 126 + ((x / 7 + y / 5) % 4) as u8;
                let index = ((y * W + x) * 4) as usize;
                data[index..index + 4].copy_from_slice(&[value, value, value, 255]);
            }
        }
        let frame = Frame::new(W, H, data, Instant::now()).expect("合成帧");
        let verdict = guard.verdict(&frame);
        assert!(
            matches!(verdict, UiVerdict::Gray | UiVerdict::Reject(_)),
            "低方差画面只能进灰带或拒绝区，实际 {verdict:?}"
        );
    }

    #[test]
    fn roi_stays_away_from_the_bottom_left_nickname() {
        // 昵称/UID 在左下角：ROI 的下边界必须明显高于它，左边界也要往里收。
        assert!(ROI.1 >= 0.08 && ROI.3 <= 0.62, "纵向不覆盖底部 HUD：{ROI:?}");
        assert!(ROI.0 >= 0.25 && ROI.2 <= 0.75, "横向不覆盖最左侧：{ROI:?}");
        let (x0, y0, x1, y1) = roi_rect(1280, 720);
        assert_eq!((x0, y0, x1, y1), (320, 57, 960, 446), "720p 下的实际 ROI");
    }

    #[test]
    fn the_same_screen_twice_keeps_the_route() {
        assert_eq!(
            recheck(
                UiVerdict::Target(Scene::Morse),
                UiVerdict::Target(Scene::Morse)
            ),
            Recheck::Keep(Scene::Morse)
        );
    }

    #[test]
    fn a_black_second_frame_downgrades_to_structure_detection() {
        // 第二帧判不清 ⇒ 不许按「刚才认出来了」去注入，交回 `detect` 重判。
        let guard = UiGuard::load(&templates_dir());
        let second = guard.verdict(&flat_frame(0));
        assert_eq!(
            recheck(UiVerdict::Target(Scene::Morse), second),
            Recheck::Downgrade
        );
    }

    #[test]
    fn a_second_frame_that_hits_another_screen_is_rejected() {
        assert_eq!(
            recheck(
                UiVerdict::Target(Scene::Morse),
                UiVerdict::Target(Scene::Console)
            ),
            Recheck::Changed,
            "两帧认出的界面不一致 ⇒ 界面在识别过程中变了，零注入"
        );
    }

    #[test]
    fn a_first_frame_that_never_hit_a_template_has_nothing_to_recheck() {
        assert_eq!(
            recheck(UiVerdict::Gray, UiVerdict::Target(Scene::Morse)),
            Recheck::Downgrade,
            "灰带本来就没靠模板 ⇒ 复核不改变它的路径"
        );
        assert_eq!(
            recheck(UiVerdict::Reject(NOT_UNLOCK_SCREEN), UiVerdict::Gray),
            Recheck::Downgrade
        );
    }
}
