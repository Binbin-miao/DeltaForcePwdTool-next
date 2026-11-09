//! 操作台标定参数 + LED 目标序列解析（spec D7 / §2.3 / §7）。
//!
//! 所有像素阈值、时序与自举边界都收敛到 [`ConsoleParams`]：状态机里**不允许**出现裸
//! 像素常数（t8 硬约束「禁止写死任何像素灵敏度/方向符号/停留时长」）。默认值来自
//! spec §7 的实测标定，可被校准结果或实机自举覆盖。

use delta_config::Timings;
use thiserror::Error;

/// 操作台列数（spec D7：4 列 × 4 行）。
pub const CONSOLE_COLUMNS: usize = 4;
/// 操作台行数（spec D7）。
pub const CONSOLE_ROWS: usize = 4;
/// 行标签自上而下（spec D7）：`10 / 11 / 01 / 00`。
pub const ROW_LABELS: [&str; CONSOLE_ROWS] = ["10", "11", "01", "00"];
/// LED 目标序列的分组数（spec §7.1：`NN-NN-NN-NN`）。
pub const LED_GROUP_COUNT: usize = 4;
/// 每组字符数（spec §7.1）。
pub const LED_GROUP_LEN: usize = 2;

/// OpenCV 8 位标度的 HSV（`h ∈ 0..180`，`s/v ∈ 0..255`）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hsv {
    pub h: f32,
    pub s: f32,
    pub v: f32,
}

/// 8 位 RGB → OpenCV 8 位 HSV。
///
/// spec §7 的所有色域阈值（青绿面板、米黄块、亮黄边框）都是 OpenCV 标度，
/// 因此这里必须产出同一标度，否则阈值不可移植。
pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> Hsv {
    let (rf, gf, bf) = (f32::from(r), f32::from(g), f32::from(b));
    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;
    let saturation = if max <= 0.0 { 0.0 } else { 255.0 * delta / max };
    let raw_hue = if delta <= 0.0 {
        0.0
    } else if max == rf {
        30.0 * ((gf - bf) / delta)
    } else if max == gf {
        30.0 * (2.0 + (bf - rf) / delta)
    } else {
        30.0 * (4.0 + (rf - gf) / delta)
    };
    Hsv {
        h: if raw_hue < 0.0 { raw_hue + 180.0 } else { raw_hue },
        s: saturation,
        v: max,
    }
}

/// HSV 闭区间（两端都含）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HsvRange {
    pub h_min: f32,
    pub h_max: f32,
    pub s_min: f32,
    pub s_max: f32,
    pub v_min: f32,
    pub v_max: f32,
}

impl HsvRange {
    /// 单侧下限（上限取满标度）。
    pub const fn at_least(h_min: f32, h_max: f32, s_min: f32, v_min: f32) -> Self {
        Self {
            h_min,
            h_max,
            s_min,
            s_max: 255.0,
            v_min,
            v_max: 255.0,
        }
    }

    pub fn contains(&self, color: Hsv) -> bool {
        (self.h_min..=self.h_max).contains(&color.h)
            && (self.s_min..=self.s_max).contains(&color.s)
            && (self.v_min..=self.v_max).contains(&color.v)
    }
}

/// LED 目标序列解析错误（全部是「零值边界」，必须直接失败而不是猜）。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum LedParseError {
    #[error("LED 区域未读到任何字符")]
    Empty,
    #[error("LED 分组数 {found} ≠ {expected}")]
    GroupCount { found: usize, expected: usize },
    #[error("第 {group} 组长度 {len} ≠ {expected}")]
    GroupLen {
        group: usize,
        len: usize,
        expected: usize,
    },
    #[error("第 {group} 组含非法字符 {character:?}（仅允许 0/1）")]
    IllegalChar { group: usize, character: char },
}

/// 4 列各自的目标行索引（`0` = 行标签 `10`，自上而下）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TargetSequence {
    rows: [usize; CONSOLE_COLUMNS],
}

impl TargetSequence {
    /// 第 `column` 列的目标行索引。`column` 由 [`CONSOLE_COLUMNS`] 界定。
    pub fn row_for(&self, column: usize) -> usize {
        self.rows[column]
    }

    /// 第 `column` 列对应的行标签（`10/11/01/00`）。
    pub fn label_for(&self, column: usize) -> &'static str {
        ROW_LABELS[self.rows[column]]
    }

    pub fn rows(&self) -> [usize; CONSOLE_COLUMNS] {
        self.rows
    }
}

/// 解析 LED 读数为 4 组 × 2 位目标序列（spec §7.1）。
///
/// 只有「恰好 4 组、每组恰好 2 位、且每位只可能是 `0` 或 `1`」才成功。因为
/// `10/11/01/00` 穷尽了所有 2 位 0/1 组合，所以解析通过就必然能映射到合法行标签 ——
/// 不需要额外的「未知标签」分支。
pub fn parse_led_sequence(raw: &str) -> Result<TargetSequence, LedParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(LedParseError::Empty);
    }
    let groups: Vec<&str> = trimmed.split('-').collect();
    if groups.len() != LED_GROUP_COUNT {
        return Err(LedParseError::GroupCount {
            found: groups.len(),
            expected: LED_GROUP_COUNT,
        });
    }
    let mut rows = [0usize; CONSOLE_COLUMNS];
    for (group, text) in groups.iter().enumerate() {
        if text.chars().count() != LED_GROUP_LEN {
            return Err(LedParseError::GroupLen {
                group,
                len: text.chars().count(),
                expected: LED_GROUP_LEN,
            });
        }
        for character in text.chars() {
            if character != '0' && character != '1' {
                return Err(LedParseError::IllegalChar { group, character });
            }
        }
        rows[group] = ROW_LABELS
            .iter()
            .position(|label| *label == *text)
            .expect("两位 0/1 组合必然落在 10/11/01/00 之内");
    }
    Ok(TargetSequence { rows })
}

/// 操作台全量参数（阈值 + 时序 + 自举边界）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ConsoleParams {
    /// 米黄色旋钮块（spec §7.2「旋钮中心（红色圆钮）」，颜色以实测标定为准）。
    pub knob_hsv: HsvRange,
    /// 面板青绿色结构特征（spec §7.4）。
    pub panel_hsv: HsvRange,
    /// 亮黄色边框（spec §7.3）。
    pub overlay_hsv: HsvRange,
    /// 面板消失判据：青色占比 < 该值（spec §7.4：8%，留 3 倍安全边界）。
    pub panel_gone_cyan_pct: f32,
    /// 旋钮块最少像素数（滤掉单像素噪点）。
    pub knob_min_pixels: u32,
    /// 旋钮块最多像素数：超过这个量级说明帧里混进了斜纹浮层/黄框等大面积黄色元素，
    /// 质心会被拉偏。上下限成对存在，缺了上限就无法判断「这块到底是不是一个旋钮」。
    pub knob_max_pixels: u32,
    /// 旋钮质心与 `col_x/row_y` 的就近匹配最大偏差。
    pub grid_snap_max_px: i32,
    /// 斜纹浮层包围盒的最小宽/高（浮层约 250×70，见 spec §7.3）。
    pub overlay_min_w: u32,
    pub overlay_min_h: u32,
    /// 灵敏度自举试探位移（spec §2.3：首次拖动先走 20px）。
    pub probe_px: i32,
    /// 后续步长夹取区间（spec §2.3 / t8：`[4, 20]px`）。
    pub min_step_px: i32,
    pub max_step_px: i32,
    /// 自举增益的合法区间（防止除零/失控）。
    pub min_gain: f32,
    pub max_gain: f32,
    /// 行对齐容差（t8：默认 6px）。
    pub row_tolerance_px: i32,
    /// 单列最大步数（t8：默认 60）。
    pub max_steps: u32,
    /// 连续无位移多少次判定拖动无效。
    pub stall_limit: u32,
    /// 横拖初值 ±12px（t8）。
    pub horizontal_step_px: i32,
    /// 横拖多少次列索引不变 → 判定「不支持横拖切列」。
    pub horizontal_attempts: u32,
    /// 「等自动跳列」分支的轮询次数与间隔（spec D7：500ms / 最多 2 次 —— 直接把截图压进配额）。
    pub auto_advance_polls: u32,
    pub auto_advance_poll_ms: u64,
    /// 停留时长与其递增（spec §2.3：默认 900ms，未见推进 +300ms，上限 2400ms）。
    pub dwell_ms: u64,
    pub dwell_step_ms: u64,
    pub dwell_max_ms: u64,
    /// 单列重试次数（spec §2.3：≤3 次）。
    pub column_retries: u32,
    /// Parse 重试帧数（spec §2.3：3 帧）。
    pub parse_frames: u32,
    pub parse_retry_ms: u64,
    /// 超时（spec §2.3：Parse 3s / 单列 12s / 全局 90s）。
    pub parse_timeout_ms: u64,
    pub column_timeout_ms: u64,
    pub global_timeout_ms: u64,
    /// 取焦点击后等待游戏把焦点交到面板上的时间。
    pub focus_settle_ms: u64,
}

impl Default for ConsoleParams {
    fn default() -> Self {
        Self {
            // spec §7.2：米黄色块。
            knob_hsv: HsvRange::at_least(12.0, 42.0, 60.0, 140.0),
            // spec §7.4：青绿色调。
            panel_hsv: HsvRange {
                h_min: 75.0,
                h_max: 105.0,
                s_min: 40.0,
                s_max: 160.0,
                v_min: 40.0,
                v_max: 120.0,
            },
            // spec §7.3：亮黄色边框。
            overlay_hsv: HsvRange::at_least(20.0, 35.0, 120.0, 180.0),
            panel_gone_cyan_pct: 8.0,
            knob_min_pixels: 20,
            knob_max_pixels: 2_000,
            grid_snap_max_px: 160,
            overlay_min_w: 80,
            overlay_min_h: 24,
            probe_px: 20,
            min_step_px: 4,
            max_step_px: 20,
            min_gain: 0.05,
            max_gain: 20.0,
            row_tolerance_px: 6,
            max_steps: 60,
            stall_limit: 3,
            horizontal_step_px: 12,
            horizontal_attempts: 3,
            auto_advance_polls: 2,
            auto_advance_poll_ms: 500,
            dwell_ms: 900,
            dwell_step_ms: 300,
            dwell_max_ms: 2_400,
            column_retries: 3,
            parse_frames: 3,
            parse_retry_ms: 50,
            parse_timeout_ms: 3_000,
            column_timeout_ms: 12_000,
            global_timeout_ms: 90_000,
            focus_settle_ms: 120,
        }
    }
}

impl ConsoleParams {
    /// 用时序配置覆盖超时与停留时长（spec §2.2 不变量 6：常量转配置但默认一致）。
    pub fn with_timings(mut self, timings: &Timings) -> Self {
        self.dwell_ms = timings.console_dwell_ms;
        self.parse_timeout_ms = timings.console_parse_timeout_ms;
        self.column_timeout_ms = timings.console_column_timeout_ms;
        self.global_timeout_ms = timings.console_global_timeout_ms;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_matches_opencv_scale() {
        // OpenCV 8 位标度：纯红 H=0、纯绿 H=60、纯青 H=90、纯蓝 H=120。
        assert_eq!(rgb_to_hsv(255, 0, 0).h, 0.0);
        assert_eq!(rgb_to_hsv(0, 255, 0).h, 60.0);
        assert!((rgb_to_hsv(0, 255, 255).h - 90.0).abs() < 1e-3);
        assert!((rgb_to_hsv(0, 0, 255).h - 120.0).abs() < 1e-3);
        // spec §7.4 的面板青色必须落在阈值内。
        let panel = rgb_to_hsv(55, 95, 120);
        assert!(ConsoleParams::default().panel_hsv.contains(panel), "{panel:?}");
        // spec §7.2 的米黄块、§7.3 的亮黄边框必须各自落在自己的阈值内。
        let knob = rgb_to_hsv(230, 200, 120);
        assert!(ConsoleParams::default().knob_hsv.contains(knob), "{knob:?}");
        let overlay = rgb_to_hsv(250, 220, 60);
        assert!(ConsoleParams::default().overlay_hsv.contains(overlay), "{overlay:?}");
        // 旋钮色域更宽，必然也覆盖亮黄；因此浮层判别不能只靠颜色，必须再验斜纹方向性。
        assert!(ConsoleParams::default().knob_hsv.contains(overlay));
    }

    #[test]
    fn parses_the_measured_sample() {
        // spec §7.1 的实测样例：01-00-01-11。
        let sequence = parse_led_sequence("01-00-01-11").expect("合法读数");
        assert_eq!(sequence.rows(), [2, 3, 2, 1]);
        assert_eq!(sequence.label_for(0), "01");
        assert_eq!(sequence.row_for(3), 1);
        assert_eq!(parse_led_sequence(" 10-11-01-00 ").expect("容忍首尾空白").rows(), [0, 1, 2, 3]);
    }

    #[test]
    fn zero_value_boundaries_fail_instead_of_guessing() {
        assert_eq!(parse_led_sequence(""), Err(LedParseError::Empty));
        assert_eq!(parse_led_sequence("   "), Err(LedParseError::Empty));
        assert_eq!(
            parse_led_sequence("01-00"),
            Err(LedParseError::GroupCount {
                found: 2,
                expected: 4
            })
        );
        assert_eq!(
            parse_led_sequence("01-00-01-11-10"),
            Err(LedParseError::GroupCount {
                found: 5,
                expected: 4
            })
        );
        assert_eq!(
            parse_led_sequence("01-00-01-1"),
            Err(LedParseError::GroupLen {
                group: 3,
                len: 1,
                expected: 2
            })
        );
        assert_eq!(
            parse_led_sequence("01-00-21-11"),
            Err(LedParseError::IllegalChar {
                group: 2,
                character: '2'
            })
        );
        assert_eq!(
            parse_led_sequence("01--01-11"),
            Err(LedParseError::GroupLen {
                group: 1,
                len: 0,
                expected: 2
            })
        );
    }

    #[test]
    fn timings_override_timeouts_but_keep_the_same_defaults() {
        let defaults = ConsoleParams::default();
        let timings = Timings::default();
        let merged = defaults.with_timings(&timings);
        assert_eq!(merged.dwell_ms, 900);
        assert_eq!(merged.parse_timeout_ms, 3_000);
        assert_eq!(merged.column_timeout_ms, 12_000);
        assert_eq!(merged.global_timeout_ms, 90_000);
    }
}
