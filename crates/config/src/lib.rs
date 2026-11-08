//! 配置模型 + 原子持久化（Deep Module）。
//!
//! 规格条款：
//! - **spec D9 / t1 任务 4**：只存百分比；`load()` 产出**独立**的 [`PxView`] 像素视图。
//! - **spec §2.1 `utils/config.py:44-123`**：上游在原地改写 cfg 造成重复 load 二次缩放漂移；
//!   本模块把「百分比是唯一持久真相、像素是每次换算出来的只读产物」变成类型约束，
//!   因此 `save()` 拿不到任何像素字段，物理上无法把像素值回写。
//! - **spec §2.2 不变量 6**：[`Timings`] 默认值与上游时序常量逐一对应。
//! - **spec D9**：校准缺失的必填区域由 [`Config::validate`] 列出，UI 据此灰置流程入口。
//! - 缺失字段走默认值、未知字段忽略（前向兼容，见 t1 任务 4）。

use delta_geom::{GeomError, MonitorInfo, PctPoint, PctRect, PxPoint, PxRect};
use serde::{Deserialize, Serialize};
use thiserror::Error;

mod store;

pub use store::{load, save, Loaded, CONFIG_FILE_NAME};

/// 摩斯数字区数量（spec §2.1 `core/morse.py:24-55`：三块点划区）。
pub const MORSE_REGION_COUNT: usize = 3;
/// 指纹候选格数量（spec §2.2 不变量 9 / 上游 `settings.json` 的 9 格）。
pub const CANDIDATE_BOX_COUNT: usize = 9;
/// 操作台列数（spec D7：4 列 × 4 行）。
pub const CONSOLE_COLUMNS: usize = 4;
/// 操作台行数（spec D7）。
pub const CONSOLE_ROWS: usize = 4;
/// 百分比「未校准」哨兵：0.0 表示该列/行/旋钮中心尚未校准。
pub const UNCALIBRATED_PCT: f32 = 0.0;

/// 配置读写错误。所有失败都带明确的动作与路径，便于「失败必留证」（spec D10）。
#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{action}失败：{path}")]
    Io {
        action: &'static str,
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("配置 JSON 解析失败：{path}")]
    Parse {
        path: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("配置序列化失败")]
    Serialize(#[source] serde_json::Error),
    #[error("坐标换算失败")]
    Geom(#[from] GeomError),
    #[error("主显示器尺寸未校准，无法生成像素视图：需先运行校准向导")]
    Uncalibrated,
}

/// 主显示器尺寸 + 全部校准区域（**只存百分比**）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 主显示器物理分辨率；未校准时为 `MonitorInfo::default()`（0x0）。
    pub monitor: MonitorInfo,
    /// 摩斯三块数字区。
    pub regions: Vec<RegionEntry>,
    /// 指纹界面区域。
    pub fingerprint: FingerprintConfig,
    /// 操作台面板区域。
    pub console: ConsoleConfig,
    /// 确认按钮点击点（spec §2.2 不变量 4：空数组 = 跳过确认点击）。
    pub confirm_clicks: Vec<PctPoint>,
    /// 热键绑定（spec D6：默认单键 F8 / `MOD_NOREPEAT`）。
    pub hotkeys: Hotkeys,
    /// 时序常量（spec §2.2 不变量 6 + spec §2.3 超时）。
    pub timings: Timings,
    /// 免责声明已接受（spec D12）。
    pub accepted_disclaimer: bool,
    /// OCR 兜底引擎开关（spec D2）。
    pub ocr_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            monitor: MonitorInfo::default(),
            regions: default_regions(),
            fingerprint: FingerprintConfig::default(),
            console: ConsoleConfig::default(),
            confirm_clicks: Vec::new(),
            hotkeys: Hotkeys::default(),
            timings: Timings::default(),
            accepted_disclaimer: false,
            ocr_enabled: true,
        }
    }
}

impl Config {
    /// 把百分比配置换算成**独立的**像素视图。每次调用都从百分比重新计算，
    /// 不读取也不写回任何既有像素值 —— 这是「连续 load 不漂移」的结构性保证。
    pub fn try_px_view(&self) -> Result<PxView, ConfigError> {
        let monitor = MonitorInfo::new(self.monitor.w, self.monitor.h)?;
        let regions = self
            .regions
            .iter()
            .map(|entry| {
                Ok(PxRegion {
                    name: entry.name.clone(),
                    rect: px_rect(entry.rect, monitor)?,
                })
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;
        let candidate_boxes = self
            .fingerprint
            .candidate_boxes
            .iter()
            .map(|rect| px_rect(*rect, monitor))
            .collect::<Result<Vec<_>, ConfigError>>()?;
        let col_x = self
            .console
            .col_x
            .iter()
            .map(|value| pct_axis_to_px(*value, monitor.w))
            .collect::<Result<Vec<_>, ConfigError>>()?;
        let row_y = self
            .console
            .row_y
            .iter()
            .map(|value| pct_axis_to_px(*value, monitor.h))
            .collect::<Result<Vec<_>, ConfigError>>()?;
        Ok(PxView {
            monitor,
            regions,
            fingerprint: PxFingerprint {
                name_region: px_rect(self.fingerprint.name_region, monitor)?,
                number_region: px_rect(self.fingerprint.number_region, monitor)?,
                candidate_boxes,
            },
            console: PxConsole {
                led_region: px_rect(self.console.led_region, monitor)?,
                panel_region: px_rect(self.console.panel_region, monitor)?,
                col_x,
                row_y,
                knob_center: self.console.knob_center.to_px(monitor)?,
            },
            confirm_clicks: self
                .confirm_clicks
                .iter()
                .map(|point| Ok(point.to_px(monitor)?))
                .collect::<Result<Vec<_>, ConfigError>>()?,
        })
    }

    /// 当前主显示器分辨率是否已不同于校准记录的分辨率。
    ///
    /// 只提示、不阻断：坐标以百分比存储，换分辨率后**相对位置仍然有效**，
    /// 但绝对像素会有偏移，UI 据此提示「建议重新校准」（spec D9）。
    pub fn resolution_changed(&self, current: MonitorInfo) -> bool {
        self.monitor.is_calibrated()
            && current.is_calibrated()
            && (self.monitor.w, self.monitor.h) != (current.w, current.h)
    }

    /// 列出所有未校准的必填区域。空列表 == 全部就绪（spec D9 的 UI 灰置依据）。
    pub fn validate(&self) -> Vec<MissingArea> {        let mut missing = Vec::new();
        if !self.monitor.is_calibrated() {
            missing.push(MissingArea::Monitor);
        }
        for index in 0..MORSE_REGION_COUNT {
            let name = self
                .regions
                .get(index)
                .map(|entry| entry.name.clone())
                .unwrap_or_else(|| default_region_name(index));
            if self.regions.get(index).is_none_or(|e| e.rect.is_empty()) {
                missing.push(MissingArea::MorseRegion { index, name });
            }
        }
        if self.fingerprint.name_region.is_empty() {
            missing.push(MissingArea::FingerprintName);
        }
        if self.fingerprint.number_region.is_empty() {
            missing.push(MissingArea::FingerprintNumber);
        }
        for index in 0..CANDIDATE_BOX_COUNT {
            if self
                .fingerprint
                .candidate_boxes
                .get(index)
                .is_none_or(|rect| rect.is_empty())
            {
                missing.push(MissingArea::FingerprintCandidateBox { index });
            }
        }
        if self.console.led_region.is_empty() {
            missing.push(MissingArea::ConsoleLed);
        }
        if self.console.panel_region.is_empty() {
            missing.push(MissingArea::ConsolePanel);
        }
        for index in 0..CONSOLE_COLUMNS {
            if !is_calibrated_axis(self.console.col_x.get(index).copied()) {
                missing.push(MissingArea::ConsoleColumn { index });
            }
        }
        for index in 0..CONSOLE_ROWS {
            if !is_calibrated_axis(self.console.row_y.get(index).copied()) {
                missing.push(MissingArea::ConsoleRow { index });
            }
        }
        let knob = self.console.knob_center;
        if knob.x == UNCALIBRATED_PCT && knob.y == UNCALIBRATED_PCT {
            missing.push(MissingArea::ConsoleKnob);
        }
        missing
    }

    /// 全部必填区域是否已校准。
    pub fn is_fully_calibrated(&self) -> bool {
        self.validate().is_empty()
    }
}

/// 一项未校准的必填区域（UI 灰置 + 提示用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissingArea {
    Monitor,
    MorseRegion { index: usize, name: String },
    FingerprintName,
    FingerprintNumber,
    FingerprintCandidateBox { index: usize },
    ConsoleLed,
    ConsolePanel,
    ConsoleColumn { index: usize },
    ConsoleRow { index: usize },
    ConsoleKnob,
}

impl std::fmt::Display for MissingArea {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Monitor => write!(f, "主显示器分辨率"),
            Self::MorseRegion { index, name } => write!(f, "摩斯区域 #{}（{name}）", index + 1),
            Self::FingerprintName => write!(f, "指纹角色名区域"),
            Self::FingerprintNumber => write!(f, "指纹模式 N 区域"),
            Self::FingerprintCandidateBox { index } => write!(f, "指纹候选格 #{}", index + 1),
            Self::ConsoleLed => write!(f, "操作台 LED 目标序列区域"),
            Self::ConsolePanel => write!(f, "操作台面板区域"),
            Self::ConsoleColumn { index } => write!(f, "操作台列 #{}", index + 1),
            Self::ConsoleRow { index } => write!(f, "操作台行 #{}", index + 1),
            Self::ConsoleKnob => write!(f, "操作台旋钮中心"),
        }
    }
}

/// 一块具名区域。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RegionEntry {
    pub name: String,
    pub rect: PctRect,
}

/// 指纹界面区域（spec §2.1 `core/fingerprint.py`）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FingerprintConfig {
    pub name_region: PctRect,
    pub number_region: PctRect,
    pub candidate_boxes: Vec<PctRect>,
}

impl Default for FingerprintConfig {
    fn default() -> Self {
        Self {
            name_region: PctRect::default(),
            number_region: PctRect::default(),
            candidate_boxes: vec![PctRect::default(); CANDIDATE_BOX_COUNT],
        }
    }
}

/// 操作台面板区域（spec D7 / §2.3）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsoleConfig {
    pub led_region: PctRect,
    pub panel_region: PctRect,
    /// 4 列的 x 百分比（相对显示器宽）；0.0 = 未校准。
    pub col_x: Vec<f32>,
    /// 4 行的 y 百分比（相对显示器高）；0.0 = 未校准。
    pub row_y: Vec<f32>,
    pub knob_center: PctPoint,
}

impl Default for ConsoleConfig {
    fn default() -> Self {
        Self {
            led_region: PctRect::default(),
            panel_region: PctRect::default(),
            col_x: vec![UNCALIBRATED_PCT; CONSOLE_COLUMNS],
            row_y: vec![UNCALIBRATED_PCT; CONSOLE_ROWS],
            knob_center: PctPoint::default(),
        }
    }
}

/// 热键绑定（spec D6：单键 + `MOD_NOREPEAT`，默认 F8）。
///
/// 只保留**真正被读取**的绑定：`scene` 由 `delta-app` 注册并分发。摩斯 / 指纹流程共用同一个
/// 场景热键，因此不存在独立的 `morse` / `fingerprint` 键位（spec §2.2 第 10 条：不得复活死配置）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    pub scene: String,
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            scene: "F8".to_owned(),
        }
    }
}

/// 时序常量。默认值与上游逐一一致（spec §2.2 不变量 6），全部可配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timings {
    /// 摩斯输入后等待（上游 0.6s）。
    pub morse_after_input_ms: u64,
    /// 指纹点击后等待（上游 1.7s）。
    pub fingerprint_after_click_ms: u64,
    /// 键间隔（上游 0.05s）。
    pub key_interval_ms: u64,
    /// 点击内含间隔（上游 0.05s）。
    pub click_interval_ms: u64,
    /// 操作台停留确认时长（spec §2.3 Dwell，默认 900ms）。
    pub console_dwell_ms: u64,
    /// 操作台单帧解析超时（spec §2.3：3s）。
    pub console_parse_timeout_ms: u64,
    /// 操作台单列超时（spec §2.3：12s）。
    pub console_column_timeout_ms: u64,
    /// 操作台全局超时（spec §2.3：90s）。
    pub console_global_timeout_ms: u64,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            morse_after_input_ms: 600,
            fingerprint_after_click_ms: 1700,
            key_interval_ms: 50,
            click_interval_ms: 50,
            console_dwell_ms: 900,
            console_parse_timeout_ms: 3_000,
            console_column_timeout_ms: 12_000,
            console_global_timeout_ms: 90_000,
        }
    }
}

/// 由百分比配置换算出的**只读**像素视图；不参与序列化，也不可能被写回配置。
#[derive(Debug, Clone, PartialEq)]
pub struct PxView {
    pub monitor: MonitorInfo,
    pub regions: Vec<PxRegion>,
    pub fingerprint: PxFingerprint,
    pub console: PxConsole,
    pub confirm_clicks: Vec<PxPoint>,
}

/// 具名像素区域。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PxRegion {
    pub name: String,
    pub rect: PxRect,
}

/// 指纹区域的像素视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PxFingerprint {
    pub name_region: PxRect,
    pub number_region: PxRect,
    pub candidate_boxes: Vec<PxRect>,
}

/// 操作台区域的像素视图。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PxConsole {
    pub led_region: PxRect,
    pub panel_region: PxRect,
    pub col_x: Vec<i32>,
    pub row_y: Vec<i32>,
    pub knob_center: PxPoint,
}

fn default_regions() -> Vec<RegionEntry> {
    (0..MORSE_REGION_COUNT)
        .map(|index| RegionEntry {
            name: default_region_name(index),
            rect: PctRect::default(),
        })
        .collect()
}

fn default_region_name(index: usize) -> String {
    format!("morse_cell_{}", index + 1)
}

/// 百分比矩形 → 屏幕内像素矩形；越界即报错，不裁剪。
fn px_rect(rect: PctRect, monitor: MonitorInfo) -> Result<PxRect, ConfigError> {
    let px = rect.to_px(monitor)?;
    px.ensure_on_screen(monitor)?;
    Ok(px)
}

/// 单轴百分比 → 像素坐标（复用 `PctPoint` 的四舍五入规则，保证与矩形换算一致）。
fn pct_axis_to_px(value: f32, extent: i32) -> Result<i32, ConfigError> {
    let monitor = MonitorInfo::new(extent, 1)?;
    Ok(PctPoint { x: value, y: 0.0 }.to_px(monitor)?.x)
}

/// 列/行坐标是否已校准（[`UNCALIBRATED_PCT`] 为哨兵，且必须落在 (0,1]）。
fn is_calibrated_axis(value: Option<f32>) -> bool {
    value.is_some_and(|v| v.is_finite() && v > UNCALIBRATED_PCT && v <= 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calibrated_config() -> Config {
        let monitor = MonitorInfo::new(1920, 1080).expect("固定分辨率");
        Config {
            monitor,
            regions: (0..MORSE_REGION_COUNT)
                .map(|index| RegionEntry {
                    name: default_region_name(index),
                    rect: PctRect {
                        x: 0.1,
                        y: 0.1 + 0.1 * index as f32,
                        w: 0.2,
                        h: 0.05,
                    },
                })
                .collect(),
            fingerprint: FingerprintConfig {
                name_region: PctRect {
                    x: 0.1,
                    y: 0.1,
                    w: 0.2,
                    h: 0.05,
                },
                number_region: PctRect {
                    x: 0.1,
                    y: 0.2,
                    w: 0.2,
                    h: 0.05,
                },
                candidate_boxes: (0..CANDIDATE_BOX_COUNT)
                    .map(|index| PctRect {
                        x: 0.1 * (index % 3) as f32,
                        y: 0.4 + 0.05 * (index / 3) as f32,
                        w: 0.08,
                        h: 0.04,
                    })
                    .collect(),
            },
            console: ConsoleConfig {
                led_region: PctRect {
                    x: 0.6,
                    y: 0.1,
                    w: 0.2,
                    h: 0.05,
                },
                panel_region: PctRect {
                    x: 0.3,
                    y: 0.3,
                    w: 0.3,
                    h: 0.3,
                },
                col_x: vec![0.35, 0.42, 0.49, 0.56],
                row_y: vec![0.35, 0.42, 0.49, 0.56],
                knob_center: PctPoint { x: 0.5, y: 0.8 },
            },
            confirm_clicks: vec![PctPoint { x: 0.5, y: 0.9 }],
            ..Config::default()
        }
    }

    #[test]
    fn default_config_reports_every_required_area_missing() {
        let missing = Config::default().validate();
        // 1 monitor + 3 morse + 2 fingerprint + 9 boxes + 2 console + 4 + 4 + 1 knob = 26
        assert_eq!(missing.len(), 26, "缺失清单：{missing:?}");
        assert_eq!(missing[0], MissingArea::Monitor);
        assert!(missing.contains(&MissingArea::ConsoleKnob));
        assert!(missing.contains(&MissingArea::FingerprintCandidateBox { index: 8 }));
        assert!(!Config::default().is_fully_calibrated());
    }

    #[test]
    fn fully_calibrated_config_has_no_missing_area_and_yields_view() {
        let config = calibrated_config();
        assert_eq!(config.validate(), Vec::new());
        let view = config.try_px_view().expect("已校准配置必须可换算");
        assert_eq!(view.monitor, MonitorInfo::new(1920, 1080).expect("固定分辨率"));
        assert_eq!(
            view.regions[0].rect,
            PxRect {
                x: 192,
                y: 108,
                w: 384,
                h: 54
            }
        );
        assert_eq!(view.fingerprint.candidate_boxes.len(), CANDIDATE_BOX_COUNT);
        assert_eq!(view.console.col_x, vec![672, 806, 941, 1075]);
        assert_eq!(view.console.row_y, vec![378, 454, 529, 605]);
        assert_eq!(view.confirm_clicks, vec![PxPoint { x: 960, y: 972 }]);
    }

    #[test]
    fn zero_percent_column_is_treated_as_uncalibrated() {
        let mut config = calibrated_config();
        config.console.col_x[2] = UNCALIBRATED_PCT;
        assert_eq!(
            config.validate(),
            vec![MissingArea::ConsoleColumn { index: 2 }]
        );
    }

    #[test]
    fn short_vector_fields_report_missing_indices() {
        let mut config = calibrated_config();
        config.regions.truncate(1);
        config.console.row_y.truncate(2);
        let missing = config.validate();
        assert!(missing.contains(&MissingArea::MorseRegion {
            index: 1,
            name: "morse_cell_2".to_owned()
        }));
        assert!(missing.contains(&MissingArea::MorseRegion {
            index: 2,
            name: "morse_cell_3".to_owned()
        }));
        assert!(missing.contains(&MissingArea::ConsoleRow { index: 2 }));
        assert!(missing.contains(&MissingArea::ConsoleRow { index: 3 }));
    }

    #[test]
    fn out_of_screen_percentage_is_an_error_not_a_crop() {
        let mut config = calibrated_config();
        config.regions[0].rect = PctRect {
            x: 0.95,
            y: 0.1,
            w: 0.2,
            h: 0.05,
        };
        assert!(matches!(
            config.try_px_view(),
            Err(ConfigError::Geom(GeomError::OutOfScreen { .. }))
        ));
    }

    #[test]
    fn uncalibrated_monitor_cannot_produce_pixel_view() {
        let config = Config::default();
        assert!(matches!(
            config.try_px_view(),
            Err(ConfigError::Geom(GeomError::InvalidMonitor { w: 0, h: 0 }))
        ));
    }

    #[test]
    fn resolution_change_is_detected_but_keeps_relative_positions() {
        let config = calibrated_config();
        let same = MonitorInfo::new(1920, 1080).expect("固定分辨率");
        let larger = MonitorInfo::new(2560, 1440).expect("固定分辨率");
        assert!(!config.resolution_changed(same), "同一分辨率不得误报");
        assert!(config.resolution_changed(larger), "换分辨率必须被检出");

        // 百分比是唯一持久真相：换分辨率后相对位置不变，绝对像素按比例放大。
        let knob_pct = config.console.knob_center;
        let on_1080 = knob_pct.to_px(same).expect("换算");
        let on_1440 = knob_pct.to_px(larger).expect("换算");
        assert_eq!(on_1080.x, 960);
        assert_eq!(on_1440.x, 1280);
        let ratio = |value: i32, extent: i32| value as f32 / extent as f32;
        assert!((ratio(on_1080.x, same.w) - ratio(on_1440.x, larger.w)).abs() < 1e-6);
        assert!((ratio(on_1080.y, same.h) - ratio(on_1440.y, larger.h)).abs() < 1e-6);

        // 未校准（0x0）时不算「变更」，避免首次启动就报警。
        assert!(!Config::default().resolution_changed(larger));
    }

    #[test]
    fn legacy_hotkey_fields_are_ignored_not_fatal() {
        // 旧配置文件里的 `morse` / `fingerprint` 已删除：必须被静默忽略（前向兼容），
        // 且剩下的 `scene` 仍正常读出。
        let json = r#"{"hotkeys":{"scene":"F7","morse":"F9","fingerprint":"F10"}}"#;
        let config: Config = serde_json::from_str(json).expect("旧配置必须仍能解析");
        assert_eq!(config.hotkeys.scene, "F7");
        assert_eq!(config.hotkeys, Hotkeys { scene: "F7".to_owned() });
    }
}
