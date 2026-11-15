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

use std::cell::Cell;
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
///
/// **后 3 项当前永不生效**：`images/ui/` 目前只有 `morse.png` / `keys.png` / `line.png`，
/// 而 [`UiGuard::load`] 只遍历目录里**真实存在**的文件 ⇒ 表里有、盘上没有的键不会被匹配到。
/// 它们属于**预留位**：一旦有人放进同名 PNG（例如把 `fingerprint.png` 拷进 `images/ui/`），
/// 这个界面就会路由到 `Scene::Fingerprint` —— **这是预期行为，不是 bug**，不需要改代码。
/// （`timing` / `pins` 是 AZ3 操作台内的两种玩法，都归 `Scene::Console`。）
const SCENE_OF_TEMPLATE: [(&str, Scene); 6] = [
    ("morse", Scene::Morse),
    ("keys", Scene::Console),
    ("line", Scene::Console),
    ("timing", Scene::Console),
    ("pins", Scene::Console),
    ("fingerprint", Scene::Fingerprint),
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
///
/// **当前无生产调用点**：就绪轮询已改成「模板命中当帧即锁定、不补抓第二帧」——
/// 两帧复核会让玩家镜头移动时把**已经确认**的面板撤回（实测录屏 92.00~92.75s
/// 模板分掉到 <0.60，正好把 90s 已确认的黄键台推翻，表现为「按一次 F 没用」）。
/// 实测分离度（面板在 0.894~0.955 vs 其余 p90 0.206）足以支撑单帧结论。
/// 本函数与它的用例**保留**：注释里这套判据随时可能被「多帧确认」重新采用。
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
    /// 日志**只在结论变化时**打一行（见 [`should_log`]）：本函数落在 40ms 轮询里，
    /// 逐帧打印会让一次 F8 刷 30 行。
    pub fn verdict(&self, frame: &Frame) -> UiVerdict {
        let Some(thumb) = thumbnail(frame) else {
            return UiVerdict::Gray;
        };
        let Some((name, best, second)) = self.best_of(&thumb) else {
            return UiVerdict::Gray;
        };
        let verdict = judge(name, best, second);
        if should_log(LogSignature::of(verdict, name)) {
            eprintln!("{}", log_line(name, best, second));
        }
        verdict
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

/// 分数 → 结论（纯函数：不碰帧、不碰盘、不打印）。
fn judge(name: &str, best: f32, second: f32) -> UiVerdict {
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

/// 一行标定日志（`verdict` 打印的就是这一行，测试直接数它）。
fn log_line(name: &str, best: f32, second: f32) -> String {
    format!("[ui_guard] best={best:.3} second={second:.3} hit={name}")
}

/// 一次判定的日志签名：**类别** + 命中名（只在 `Target` 时才算数）。
///
/// `Reject` / `Gray` 下的「最高分模板名」不是结论而是噪声（分数都在 0.3 以下来回抖），
/// 把它算进签名会让灰带里的抖动反复触发打印。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LogSignature {
    /// `UiVerdict` 的类别：`Reject` = 0、`Gray` = 1、`Target` = 2。
    class: u8,
    /// 命中名哈希（非 `Target` 时为 0）。
    name: u32,
}

impl LogSignature {
    fn of(verdict: UiVerdict, name: &str) -> Self {
        let class = match verdict {
            UiVerdict::Reject(_) => 0,
            UiVerdict::Gray => 1,
            UiVerdict::Target(_) => 2,
        };
        Self {
            class,
            name: if class == 2 { hash_name(name) } else { 0 },
        }
    }
}

/// 名字的稳定哈希（只为比较，不落盘）。
fn hash_name(name: &str) -> u32 {
    name.bytes().fold(2166136261u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(16777619)
    })
}

thread_local! {
    /// 本线程上一次**已打印**的结论签名。轮询线程里它就是「上一帧的结论」。
    static LAST_LOG: Cell<Option<LogSignature>> = const { Cell::new(None) };
}

/// 结论变化才放行打印（`true` = 这次该打）。
///
/// 一次 F8 ≈ 30 次轮询，而结论只会经历「不像 → 灰带 → 命中」两三次跃迁，
/// 因此 stderr 行数与**结论跃迁次数**同阶，与轮询次数无关 —— 这是刷屏的解药，
/// 也是标定数据的来源：切换瞬间前后两个结论都留在日志里。
fn should_log(signature: LogSignature) -> bool {
    LAST_LOG.with(|slot| {
        if slot.get() == Some(signature) {
            return false;
        }
        slot.set(Some(signature));
        true
    })
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
        let entries = UiGuard::load(&templates_dir()).entries;
        // **点名断言**，不数张数：加一张模板不该让这里红，缺一张必须一眼看出缺的是谁。
        for name in ["morse", "keys", "line", "fingerprint"] {
            assert!(
                entries.iter().any(|(entry, _)| entry == name),
                "images/ui 缺少 {name}.png 正样本，现有：{:?}",
                entries.iter().map(|(entry, _)| entry.as_str()).collect::<Vec<_>>()
            );
        }
        // 下限兜底（防目录被清空/污染），但不封顶：以后再加模板不必回来改这里。
        assert!(
            entries.len() >= 4,
            "images/ui 至少要装下 morse/keys/line/fingerprint 四张正样本，实际 {} 张",
            entries.len()
        );
        entries
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
    fn a_fingerprint_screen_routes_to_its_own_scene() {
        // 指纹台有了自己的模板之后，必须路由到**它自己的** Scene：只断言 `is_target`
        // 证明不了这件事（被 keys 抢走也会是 target）。这才是「路由正确」的证明。
        let guard = UiGuard::load(&templates_dir());
        assert_eq!(
            guard.verdict(&frame_showing(&template_of("fingerprint"))),
            UiVerdict::Target(Scene::Fingerprint),
            "指纹台画面必须命中 Fingerprint；若命中 Console，说明被别的模板抢走了"
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
    fn reserved_template_names_route_to_their_scenes() {
        // 预留位：盘上没有这几张图，但映射必须先写好 —— 放进同名 PNG 即生效，不必改代码。
        assert_eq!(scene_of("fingerprint"), Some(Scene::Fingerprint));
        assert_eq!(scene_of("timing"), Some(Scene::Console));
        assert_eq!(scene_of("pins"), Some(Scene::Console));
        assert_eq!(scene_of("keys"), Some(Scene::Console));
        assert_eq!(scene_of("没有登记的名字"), None, "未登记的名字必须安全跳过");
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

    // ── 轮询日志门控（一次 F8 的 stderr 行数必须与轮询次数脱钩）──

    /// 清空本线程的「上一帧结论」，让每条门控测试都从干净状态开始。
    fn reset_log_gate() {
        LAST_LOG.with(|slot| slot.set(None));
    }

    /// 过一遍门控并把**真正会打出去的行**收集起来（与 `verdict` 同源：同签名、同格式）。
    fn collect_lines(trajectory: &[(UiVerdict, &str, f32, f32)]) -> Vec<String> {
        reset_log_gate();
        trajectory
            .iter()
            .filter(|(verdict, name, _, _)| should_log(LogSignature::of(*verdict, name)))
            .map(|(_, name, best, second)| log_line(name, *best, *second))
            .collect()
    }

    #[test]
    fn one_press_never_prints_more_than_five_lines() {
        // 一次 F8 ≈ 30 次轮询（40ms × 1.2s 预算）。模拟真实轨迹：
        // 淡入期不像（Reject）→ 灰带（Gray）→ 命中 morse（Target）→ 复核期间界面切到 keys。
        // 全程 30 帧，门控后落地行数必须 ≤ 5（修复前是 30 行）。
        let mut trajectory: Vec<(UiVerdict, &str, f32, f32)> = Vec::new();
        for index in 0..12 {
            // 淡入期：最高分在 0.02~0.12 抖，最高分模板名在 morse/keys 之间乱跳。
            let name = if index % 2 == 0 { "morse" } else { "keys" };
            trajectory.push((
                UiVerdict::Reject(NOT_UNLOCK_SCREEN),
                name,
                0.02 + index as f32 * 0.008,
                0.0,
            ));
        }
        for index in 0..6 {
            let name = if index % 2 == 0 { "keys" } else { "line" };
            trajectory.push((UiVerdict::Gray, name, 0.31 + index as f32 * 0.05, 0.20));
        }
        for index in 0..10 {
            trajectory.push((
                UiVerdict::Target(Scene::Morse),
                "morse",
                0.80 + index as f32 * 0.02,
                0.05,
            ));
        }
        for index in 0..2 {
            trajectory.push((
                UiVerdict::Target(Scene::Console),
                "keys",
                0.90 + index as f32 * 0.02,
                0.05,
            ));
        }
        assert_eq!(trajectory.len(), 30, "30 次轮询 = 一次 F8 的真实量级");

        let lines = collect_lines(&trajectory);
        assert!(
            lines.len() <= 5,
            "一次 F8 的 stderr 必须 ≤ 5 行，实际 {} 行：{lines:#?}",
            lines.len()
        );
        assert!(
            lines.len() >= 2,
            "结论跃迁不能全被吞掉，至少要留下前后两个结论：{lines:#?}"
        );
        // 可诊断性：第一行是「不像」（低分），最后一行是命中 keys（高分）。
        assert!(
            lines[0].contains("best=0.020") && lines[0].contains("hit=morse"),
            "第一行必须是最初的「不像」：{}",
            lines[0]
        );
        let last = lines.last().expect("至少两行");
        assert!(
            last.contains("hit=keys") && last.contains("best=0.9"),
            "最后一行必须是切换后的命中：{last}"
        );
    }

    #[test]
    fn a_stable_conclusion_is_printed_exactly_once() {
        // 结论不变就不打：同一结论连打 30 帧只留 1 行，标定数据照样在。
        let stable: Vec<(UiVerdict, &str, f32, f32)> = (0..30)
            .map(|_| (UiVerdict::Target(Scene::Morse), "morse", 0.95, 0.04))
            .collect();
        let lines = collect_lines(&stable);
        assert_eq!(lines.len(), 1, "结论没变就必须只打一行：{lines:#?}");
        assert!(lines[0].contains("hit=morse"));
    }

    #[test]
    fn a_verdict_class_change_always_leaves_a_line() {
        // 三个类别之间互相跃迁，每一类都必须留下证据（否则「切换瞬间看不见」）。
        let lines = collect_lines(&[
            (UiVerdict::Reject(NOT_UNLOCK_SCREEN), "morse", 0.05, 0.0),
            (UiVerdict::Gray, "morse", 0.42, 0.30),
            (UiVerdict::Target(Scene::Morse), "morse", 0.88, 0.05),
            (UiVerdict::Gray, "morse", 0.42, 0.30),
            (UiVerdict::Reject(NOT_UNLOCK_SCREEN), "morse", 0.05, 0.0),
        ]);
        assert_eq!(lines.len(), 5, "每一次类别跃迁都要留一行：{lines:#?}");
    }
}
