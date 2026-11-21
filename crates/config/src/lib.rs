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

pub use delta_capture::CaptureBackend;
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

/// 六套主题（`docs/design/ui-themes.md` §2.1~§2.6）。
///
/// 定义在 config 而不是 ui：配置要持久化它，而 `delta-ui` 依赖本 crate，反向不成立。
/// 色板数据放 `delta_ui::theme`（那边才有 egui 依赖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeId {
    /// 石墨（深，无彩色）。
    Graphite,
    /// 夜航（深，电蓝）。
    Nightwatch,
    /// 暗焰（深，暖橙）。
    Ember,
    /// 素纸（浅，墨蓝）。
    Paper,
    /// 霜白（浅，近黑）—— **用户拍板的默认主题**（spec D7）。
    #[default]
    Frost,
    /// 瓷青（浅，翠绿）。
    Celadon,
}

impl ThemeId {
    /// 设置页列表里的中文名。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Graphite => "石墨",
            Self::Nightwatch => "夜航",
            Self::Ember => "暗焰",
            Self::Paper => "素纸",
            Self::Frost => "霜白",
            Self::Celadon => "瓷青",
        }
    }

    /// 深色主题的判定（设置页的「浅色/深色」副标题用）。
    pub const fn is_dark(self) -> bool {
        matches!(self, Self::Graphite | Self::Nightwatch | Self::Ember)
    }

    /// 配置里的 token → 主题。**未知 token 回落 Frost 并留痕**（前向兼容：
    /// 一份写错主题名的配置不该让用户的校准坐标一起丢掉）。
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "graphite" => Self::Graphite,
            "nightwatch" => Self::Nightwatch,
            "ember" => Self::Ember,
            "paper" => Self::Paper,
            "frost" => Self::Frost,
            "celadon" => Self::Celadon,
            other => {
                eprintln!("[config] 未知主题 {other:?}，回落 frost");
                Self::Frost
            }
        }
    }

    /// 落盘 token（与 `parse` 一一对应）。
    pub const fn token(self) -> &'static str {
        match self {
            Self::Graphite => "graphite",
            Self::Nightwatch => "nightwatch",
            Self::Ember => "ember",
            Self::Paper => "paper",
            Self::Frost => "frost",
            Self::Celadon => "celadon",
        }
    }
}

impl<'de> Deserialize<'de> for ThemeId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(Self::parse(&raw))
    }
}

/// 主页上 7 个**可独立开关**的功能（用户拍板：每个功能一个开关）。
///
/// 持久化用的枚举放在本 crate：`delta-ui` 依赖本 crate，反向不成立，所以界面侧的
/// `delta_ui::FlowId` 不能拿来当配置类型。两张表的顺序与标签必须一致 ——
/// `delta-app` 有一条漂移守卫用例把它们钉在一起，任何一侧改字/挪位都会当场变红。
///
/// 下标即 [`FlowFlags::flags`] 的位序，所以**只允许在末尾追加**：插队会让旧配置位序错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flow {
    /// 摩斯码解码。
    Morse,
    /// 指纹识别。
    Fingerprint,
    /// 操作台：v5 旧旋钮流程（AZ3 四台都没命中时才轮到它）。
    Console,
    /// AZ3 · 大保险。
    Az3Timing,
    /// AZ3 · 任务保险。
    Az3Pins,
    /// AZ3 · 黄键台。
    Az3Keys,
    /// AZ3 · 信号线台。
    Az3Line,
}

impl Flow {
    /// 固定顺序 = 主页网格顺序 = [`FlowFlags::flags`] 下标序。
    pub const ALL: [Self; 7] = [
        Self::Morse,
        Self::Fingerprint,
        Self::Console,
        Self::Az3Timing,
        Self::Az3Pins,
        Self::Az3Keys,
        Self::Az3Line,
    ];

    /// 存储下标（声明顺序）。
    pub const fn index(self) -> usize {
        self as usize
    }

    /// 中文名：主页开关格与闸门文案 `<功能名>开关已关闭` 共用这一份字面量。
    pub const fn label(self) -> &'static str {
        match self {
            Self::Morse => "摩斯码",
            Self::Fingerprint => "指纹",
            Self::Console => "操作台",
            Self::Az3Timing => "大保险",
            Self::Az3Pins => "任务保险",
            Self::Az3Keys => "黄键台",
            Self::Az3Line => "信号线台",
        }
    }
}

/// 7 个功能开关（[`Config::flows`]）：关掉的功能在 `delta-app::worker::run_once` 里
/// 被闸门拦住，零注入、也不再多抓一帧。
///
/// 默认**全开**：升级不许把用户正在用的功能悄悄关掉（旧配置里没有这个字段时同样全开）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FlowFlags {
    pub morse: bool,
    pub fingerprint: bool,
    /// v5 旧旋钮操作台（AZ3 四台各有自己的开关，不受它支配）。
    pub console: bool,
    pub az3_timing: bool,
    pub az3_pins: bool,
    pub az3_keys: bool,
    pub az3_line: bool,
}

impl Default for FlowFlags {
    fn default() -> Self {
        Self::all_on()
    }
}

impl FlowFlags {
    /// 7 个全开。
    pub const fn all_on() -> Self {
        Self {
            morse: true,
            fingerprint: true,
            console: true,
            az3_timing: true,
            az3_pins: true,
            az3_keys: true,
            az3_line: true,
        }
    }

    /// 单个功能的开关值。
    pub const fn get(self, flow: Flow) -> bool {
        match flow {
            Flow::Morse => self.morse,
            Flow::Fingerprint => self.fingerprint,
            Flow::Console => self.console,
            Flow::Az3Timing => self.az3_timing,
            Flow::Az3Pins => self.az3_pins,
            Flow::Az3Keys => self.az3_keys,
            Flow::Az3Line => self.az3_line,
        }
    }

    /// 按 [`Flow::ALL`] 顺序取出 7 个布尔（与 `delta_ui::Toggles::flags` 同一口径）。
    pub const fn flags(self) -> [bool; Flow::ALL.len()] {
        [self.morse, self.fingerprint, self.console, self.az3_timing, self.az3_pins, self.az3_keys, self.az3_line]
    }

    /// 按 [`Flow::ALL`] 顺序装 7 个布尔（**7 个字段全写**，不做部分保存）。
    pub const fn from_flags(flags: [bool; Flow::ALL.len()]) -> Self {
        Self {
            morse: flags[Flow::Morse.index()],
            fingerprint: flags[Flow::Fingerprint.index()],
            console: flags[Flow::Console.index()],
            az3_timing: flags[Flow::Az3Timing.index()],
            az3_pins: flags[Flow::Az3Pins.index()],
            az3_keys: flags[Flow::Az3Keys.index()],
            az3_line: flags[Flow::Az3Line.index()],
        }
    }

    /// 是否至少有一个功能开着（全关 = 没有任何流程可跑）。
    pub fn any_on(self) -> bool {
        self.flags().iter().any(|on| *on)
    }
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
    /// AZ3 骇入电脑 4 类小游戏区域与预算配置。
    pub az3: Az3Config,
    /// 确认按钮点击点（spec §2.2 不变量 4：空数组 = 跳过确认点击）。
    pub confirm_clicks: Vec<PctPoint>,
    /// 热键绑定（spec D6：默认单键 F8 / `MOD_NOREPEAT`）。
    pub hotkeys: Hotkeys,
    /// 时序常量（spec §2.2 不变量 6 + spec §2.3 超时）。
    pub timings: Timings,
    /// OCR 兜底引擎开关（spec D2）。
    pub ocr_enabled: bool,
    /// 是否自动点「确认」按钮（默认开）。
    ///
    /// 开启时：`confirm_clicks` 为空会回退到内置坐标（上游 2K 版那两个点），做到开箱即用；
    /// 关闭时：无论配置里有没有坐标都不点 —— 这样 spec 不变量 4「空数组 = 跳过确认」
    /// 仍然可以被用户主动表达（把开关关掉即可），不会被内置值悄悄覆盖。
    pub auto_confirm: bool,
    /// 指纹是否真的自动点击（对齐上游 `fingerprint_auto_click`）。
    ///
    /// `false` → 只产出标注图、不点击，用于核对识别结果（上游手动模式）。
    pub fingerprint_auto_click: bool,
    /// 截图后端（spec D1：默认 WGC；未知值读入回落 WGC 并 `eprintln` 留痕）。
    #[serde(default, with = "capture_backend_serde")]
    pub capture_backend: CaptureBackend,
    /// 窗口是否置顶（默认 **关**）。
    ///
    /// 用户实测「只能挂后台不能置顶」的反面：常驻置顶会挡住游戏画面，所以默认不置顶，
    /// 需要时由标题条的图钉开关打开，并按开关状态立即落盘 —— 下次启动照旧生效。
    pub always_on_top: bool,
    /// F 键低级键盘钩子总开关（spec D3 边界 3，默认 **开**）。
    ///
    /// 关掉 = 卸载钩子，F 键完全不受本程序影响。**绝不**在关闭后回退成
    /// `RegisterHotKey("F")` —— 那是全局独占，会把游戏的 F 交互键吞掉（D3 边界 4）。
    pub f_key_hook_enabled: bool,
    /// 识别总开关（默认 **开**）。
    ///
    /// 关掉之后**任何**热键（F / F5 / F6 / F7 / F8）按下都不再触发识别，
    /// 连截图都不抓 —— 用于「现在不想让它动」的场合（对枪、跑图、看录像）。
    /// 与 `f_key_hook_enabled` 的区别：那个只管 F 键钩子，这个管全部入口。
    #[serde(default = "default_true")]
    pub master_enabled: bool,
    /// 主页 7 个功能各自的开关（用户拍板：独立启用/禁用）。
    ///
    /// **在总开关放行之后**才生效：总开关是「整体别动」，这 7 个是「哪几项别动」。
    /// 缺字段（旧配置）⇒ 全开，升级不许把用户正在用的功能关掉。
    #[serde(default)]
    pub flows: FlowFlags,
    /// 前台窗口标题命中这些关键词才响应 F 键（spec D3 边界 5）。
    ///
    /// 默认 `["三角洲", "Delta Force"]`；匹配规则是**大小写不敏感的子串包含**。
    /// 空列表 = 永不响应（不是「全都响应」）—— 用户清空后必须自己填回来。
    pub game_title_keywords: Vec<String>,
    /// 界面主题（spec D7：默认 **Frost 霜白**；旧配置缺该字段时回落到 Frost）。
    pub theme_id: ThemeId,
    /// 自定义主色（`#RRGGBB`；`None` = 用该主题自带的 accent）。
    ///
    /// 存字符串而不是自定义类型：配置是纯 JSON，且它只是「色相载体」——
    /// 明度/彩度由 `delta_ui::theme` 的 OKLCH 规则算，用户改不动对比度。
    pub accent_override: Option<String>,
    /// 当前实际屏幕分辨率（**不持久化**）。
    ///
    /// 百分比才是唯一持久真相：换算像素必须按「当前屏幕」，而不是「上次校准时的屏幕」，
    /// 否则换了分辨率还用旧尺寸换算，整张坐标图会整体偏移 —— 这正是「免校准」能成立的前提。
    /// `serde(skip)` 保证它永远不落盘，也不会覆盖校准基线。
    #[serde(skip)]
    pub runtime_monitor: MonitorInfo,
}

/// serde 默认值：字段缺失时按「开」处理（旧配置升级不该把功能关掉）。
fn default_true() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            monitor: MonitorInfo::default(),
            regions: default_regions(),
            fingerprint: FingerprintConfig::default(),
            console: ConsoleConfig::default(),
            az3: Az3Config::default(),
            confirm_clicks: Vec::new(),
            hotkeys: Hotkeys::default(),
            timings: Timings::default(),
            ocr_enabled: true,
            auto_confirm: true,
            fingerprint_auto_click: true,
            capture_backend: CaptureBackend::Wgc,
            always_on_top: false,
            f_key_hook_enabled: true,
            master_enabled: true,
            flows: FlowFlags::all_on(),
            game_title_keywords: default_game_title_keywords(),
            theme_id: ThemeId::default(),
            accent_override: None,
            runtime_monitor: MonitorInfo::default(),
        }
    }
}

impl Config {
    /// 把百分比配置换算成**独立的**像素视图。每次调用都从百分比重新计算，
    /// 不读取也不写回任何既有像素值 —— 这是「连续 load 不漂移」的结构性保证。
    pub fn try_px_view(&self) -> Result<PxView, ConfigError> {
        // 换算一律按「当前屏幕」：runtime_monitor 优先，未注入时才回退到持久化基线。
        let source = if self.runtime_monitor.is_calibrated() {
            self.runtime_monitor
        } else {
            self.monitor
        };
        let monitor = MonitorInfo::new(source.w, source.h)?;
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
            az3: PxAz3 {
                grid_region: px_rect(self.az3.grid_region, monitor)?,
                target_band: px_rect(self.az3.target_band, monitor)?,
                crack_button: px_rect(self.az3.crack_button, monitor)?,
                timing_window_px: ((self.az3.timing_window_pct * monitor.w as f32).round() as i32).max(2),
                track_band: px_rect(self.az3.track_band, monitor)?,
                indicator_band: px_rect(self.az3.indicator_band, monitor)?,
                pin_center_x: self.az3.pin_center_x.iter().map(|v| pct_axis_to_px(*v, monitor.w)).collect::<Result<Vec<_>, ConfigError>>()?,
                pin_tolerance_px: ((self.az3.pin_tolerance_pct * monitor.w as f32).round() as i32).max(2),
                button_grid_region: px_rect(self.az3.button_grid_region, monitor)?,
                progress_region: px_rect(self.az3.progress_region, monitor)?,
                line_led_region: px_rect(self.az3.line_led_region, monitor)?,
                line_panel_region: px_rect(self.az3.line_panel_region, monitor)?,
                line_col_x: self
                    .az3
                    .line_col_x
                    .iter()
                    .map(|value| pct_axis_to_px(*value, monitor.w))
                    .collect::<Result<Vec<_>, ConfigError>>()?,
                line_row_y: self
                    .az3
                    .line_row_y
                    .iter()
                    .map(|value| pct_axis_to_px(*value, monitor.h))
                    .collect::<Result<Vec<_>, ConfigError>>()?,
                line_knob_center: self.az3.line_knob_center.to_px(monitor)?,
                max_duration_ms: self.az3.max_duration_ms,
                frame_budget: self.az3.frame_budget,
                poll_interval_ms: self.az3.poll_interval_ms,
                keys_recognize_delay_ms: self.timings.keys_recognize_delay_ms,
                first_key_extra_delay_ms: self.timings.first_key_extra_delay_ms,
                click_interval_ms: self.timings.click_interval_ms,
            },
            // 自动确认开 + 配置里没写坐标 → 用内置坐标（开箱即用）；
            // 自动确认关 → 一律空数组（spec 不变量 4 的「空 = 跳过」）。
            confirm_clicks: if self.auto_confirm && self.confirm_clicks.is_empty() {
                builtin::CONFIRM_CLICKS
                    .iter()
                    .map(|point| Ok(point.to_px(monitor)?))
                    .collect::<Result<Vec<_>, ConfigError>>()?
            } else if self.auto_confirm {
                self.confirm_clicks
                    .iter()
                    .map(|point| Ok(point.to_px(monitor)?))
                    .collect::<Result<Vec<_>, ConfigError>>()?
            } else {
                Vec::new()
            },
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
    pub fn validate(&self) -> Vec<MissingArea> {
        let mut missing = Vec::new();
        if !self.monitor.is_calibrated() && !self.runtime_monitor.is_calibrated() {
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

/// **正常路径**（进度格读得到）的输入事件硬上限（安全网，不是可调参数）。
///
/// 合法用量：单盘最坏 = 16 颗键全亮 × `DEAD_STRIKES`(flow-az3，现 4) 轮判死重试 × 2 事件
/// （点击 = 按下 + 抬起）= `16 × 4 × 2` = **128 个事件**
/// （`module_keys_aborts_once_all_sixteen_keys_are_spent` 实测就是走到这个量）。
///
/// 上限必须**高于**它，否则「16 格全亮、进度不动」这种**合法**收尾会被误报成「疑似误判」——
/// 把「这局没解开」说成「这不是键台」，归因全错（LESSON-007：两个各自合理的数字相乘）。
///
/// 160 = 合法最坏 128 + 32 余量（允许几帧重复确认与末轮补点），
/// 同时仍远低于实测误判乱点（344 / 330 / 370）—— 真跑飞时 `16 键全点一遍只需 32 事件`，
/// 撞不到这条线，所以 160 挡不住「正常一轮」，只挡得住「无穷重试」。
///
/// 历史：曾按「一屏 5 颗亮键 × 1 事件」估成 60（漏了「一次点击两个事件」与「16 键全亮」）；
/// 后按 2 轮判死估成 120，`DEAD_STRIKES` 从 2 提到 4 之后 128 > 120，必须跟着抬到 160。
pub const AZ3_MAX_INPUT_EVENTS: u32 = 160;

/// **退化路径**（进度格读不到、靠「16 格点完」收尾）的输入事件硬上限。
///
/// 合法上限 = 16 键 × 2 轮 × 2 事件 × (1 主盘 + 退化模式允许的 3 次重扫) = **256 个事件**。
/// 300 给它余量，同时仍**低于实测乱点 344** —— 分层就是为了同时不误杀
/// 「真面板但进度格读不清」这个已知退化场景，也不放过真正的跑飞。
///
/// 误判路径（进度格是假的、矩阵也不是面板）通常「什么都读不全」，会落进这一档。
pub const AZ3_MAX_INPUT_EVENTS_DEGRADED: u32 = 300;

/// 单次任务的**拖动次数硬上限**（安全网）。
///
/// **单位是「次拖动」，不是「个事件」**：一次拖动 = 按下 + 8 步位移 + 抬起 = 10 个输入事件。
/// 信号线台正常一轮是 4 列各 1~2 拖（含补拖、归零兜底），最多约 28 次 ⇒ 用 60 个事件
/// 当上限会在正常一轮里就误报「疑似误判」。60 次拖动 ≈ 600 个事件，仍只是帧预算
/// （300 帧 × 每帧最多 2 拖）的一小部分，足以在跑飞时收住。
pub const AZ3_MAX_DRAG_ACTIONS: u32 = 60;

/// AZ3 骇入电脑 4 类小游戏配置（百分比坐标，1280×720 实测默认值）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Az3Config {
    /// 大保险：5×5 字符矩阵区域。
    pub grid_region: PctRect,
    /// 大保险：高亮目标行带。
    pub target_band: PctRect,
    /// 大保险：右侧绿色「破解成功」圆钮区域。
    pub crack_button: PctRect,
    /// 大保险：光标命中中心容差（相对屏幕宽百分比）。
    pub timing_window_pct: f32,
    /// 任务保险：5 个竖向游标滑槽带。
    pub track_band: PctRect,
    /// 任务保险：底部 5 个绿色锁定指示条带。
    pub indicator_band: PctRect,
    /// 任务保险：5 槽中心 x 百分比。
    pub pin_center_x: Vec<f32>,
    /// 任务保险：游标命中中心容差（相对屏幕宽百分比）。
    pub pin_tolerance_pct: f32,
    /// 黄键台：右侧 4×4 按键网格区域。
    ///
    /// **保留仅为向后兼容**：黄键台已改为每帧自适应定位（`delta_scene::locate::locate_keys`），
    /// 本字段不再参与判别与执行。实测换一版界面布局，这个固定矩形就整体偏出去，
    /// 四等分切出来的格子会落在键缝里（点空）。
    pub button_grid_region: PctRect,
    /// 黄键台：左侧屏幕底部 5 个进度勾选框区域。
    /// **保留仅为向后兼容**（已改自适应定位，不再被读取）。
    pub progress_region: PctRect,
    /// 信号线台：右上 LED 序列区（`NN-NN-NN-NN`）。
    ///
    /// **保留仅为向后兼容**：信号线台已改为每帧自适应定位（`delta_scene::locate`），
    /// 本字段不再参与执行。实测旋钮/面板/LED 会随视角漂动，固定百分比必然时而全落空。
    pub line_led_region: PctRect,
    /// 信号线台：深蓝面板区。**保留仅为向后兼容**（已改自适应定位，不再被读取）。
    pub line_panel_region: PctRect,
    /// 信号线台：4 列的 x 百分比。**保留仅为向后兼容**（已改自适应定位，不再被读取）。
    pub line_col_x: Vec<f32>,
    /// 信号线台：4 行标签的 y 百分比。**保留仅为向后兼容**（已改自适应定位，不再被读取）。
    pub line_row_y: Vec<f32>,
    /// 信号线台：红色旋钮中心。**保留仅为向后兼容**（已改自适应定位，不再被读取）。
    pub line_knob_center: PctPoint,
    /// 单次任务最大实时抓帧时长上限（默认 15_000ms）。
    pub max_duration_ms: u64,
    /// 单次任务实时抓帧配额上限（默认 300 帧 ≈ 15s @ 20fps）。
    pub frame_budget: u32,
    /// 实时轮询间隔（默认 50ms ≈ 20fps）。
    pub poll_interval_ms: u64,
}

impl Default for Az3Config {
    fn default() -> Self {
        Self {
            grid_region: builtin::AZ3_GRID_REGION,
            target_band: builtin::AZ3_TARGET_BAND,
            crack_button: builtin::AZ3_CRACK_BUTTON,
            timing_window_pct: 0.018,
            track_band: builtin::AZ3_TRACK_BAND,
            indicator_band: builtin::AZ3_INDICATOR_BAND,
            pin_center_x: builtin::AZ3_PIN_CENTER_X.to_vec(),
            pin_tolerance_pct: 0.012,
            button_grid_region: builtin::AZ3_BUTTON_GRID_REGION,
            progress_region: builtin::AZ3_PROGRESS_REGION,
            line_led_region: builtin::AZ3_LINE_LED_REGION,
            line_panel_region: builtin::AZ3_LINE_PANEL_REGION,
            line_col_x: builtin::AZ3_LINE_COL_X.to_vec(),
            line_row_y: builtin::AZ3_LINE_ROW_Y.to_vec(),
            line_knob_center: builtin::AZ3_LINE_KNOB_CENTER,
            max_duration_ms: 15_000,
            frame_budget: 300,
            poll_interval_ms: 50,
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
    /// 一键识别（自动判别场景）。
    pub scene: String,
    /// 强制跑摩斯（对齐上游 `F5`）。
    pub morse: String,
    /// 强制跑指纹（对齐上游 `F6`）。
    pub fingerprint: String,
    /// 强制跑操作台。
    pub console: String,
    /// 强制跑黄键台（AZ3 `ModuleKeys`，默认 `F10`）。
    ///
    /// 与 `console` 的分工：`console` 进去后还要靠 UI 模板闸再判是哪个台子；
    /// 这个键是**用户已明确站在黄键台前**，直接进黄键台流程，不再判别。
    pub keys: String,
    /// 强制跑横线台（AZ3 `SignalLine`，默认 `F11`）。语义同上。
    pub line: String,
    /// 识别总开关的翻转键（默认 `F9`）。
    ///
    /// 与上面四个的分工：那四个**发起**识别，这个只**开关**识别 ——
    /// 而且关着的时候它照样有效，否则关掉就再也按不回来了。
    pub master: String,
}

/// 七个可独立改键的槽位（设置页逐个可点）。
///
/// 与 [`Hotkeys`] 的字段一一对应；`Scene` 是「一键识别」，中间四个是强制入口
/// （`Keys` / `Line` 进一步细到 AZ3 变体，不再判别），
/// `Master` 是识别总开关的翻转键（唯一一个**不发起**识别的槽位）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HotkeySlot {
    /// 一键识别（自动判别场景）。
    Scene,
    /// 强制摩斯。
    Morse,
    /// 强制指纹。
    Fingerprint,
    /// 强制操作台。
    Console,
    /// 强制黄键台（AZ3 `ModuleKeys`）。
    Keys,
    /// 强制横线台（AZ3 `SignalLine`）。
    Line,
    /// 翻转识别总开关。
    Master,
}

impl HotkeySlot {
    /// 按设置页显示顺序排列（`Master` 紧跟总开关行、排在识别键之前）。
    pub const ALL: [Self; 7] = [
        Self::Master,
        Self::Scene,
        Self::Morse,
        Self::Fingerprint,
        Self::Console,
        Self::Keys,
        Self::Line,
    ];

    /// 数组下标（`ALL` 的位置）。
    pub fn index(self) -> usize {
        match self {
            Self::Master => 0,
            Self::Scene => 1,
            Self::Morse => 2,
            Self::Fingerprint => 3,
            Self::Console => 4,
            Self::Keys => 5,
            Self::Line => 6,
        }
    }

    /// 设置页左侧标题。
    pub fn label(self) -> &'static str {
        match self {
            Self::Master => "总开关热键",
            Self::Scene => "全局热键",
            Self::Morse => "摩斯热键",
            Self::Fingerprint => "指纹热键",
            Self::Console => "操作台热键",
            Self::Keys => "黄键台热键",
            Self::Line => "横线台热键",
        }
    }

    /// 读当前键名。
    pub fn read(self, keys: &Hotkeys) -> &str {
        match self {
            Self::Master => &keys.master,
            Self::Scene => &keys.scene,
            Self::Morse => &keys.morse,
            Self::Fingerprint => &keys.fingerprint,
            Self::Console => &keys.console,
            Self::Keys => &keys.keys,
            Self::Line => &keys.line,
        }
    }

    /// 写回键名。
    pub fn write(self, keys: &mut Hotkeys, value: String) {
        match self {
            Self::Master => keys.master = value,
            Self::Scene => keys.scene = value,
            Self::Morse => keys.morse = value,
            Self::Fingerprint => keys.fingerprint = value,
            Self::Console => keys.console = value,
            Self::Keys => keys.keys = value,
            Self::Line => keys.line = value,
        }
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        Self {
            scene: "F8".to_owned(),
            // 与上游一致：F5 摩斯、F6 指纹；操作台是本版新增，占用 F7。
            morse: "F5".to_owned(),
            fingerprint: "F6".to_owned(),
            console: "F7".to_owned(),
            // F10/F11：把「操作台里的两个 AZ3 台子」拆成独立强制键，接在 F7 之后。
            keys: "F10".to_owned(),
            line: "F11".to_owned(),
            // 总开关翻转键：紧挨 F5~F8，游戏里通常空着。
            master: "F9".to_owned(),
        }
    }
}

// ───────────────────── 内置默认区域（开箱即用，免校准） ─────────────────────

/// 内置区域坐标（百分比，与分辨率无关）。
///
/// 来源：
/// - **摩斯 / 指纹**：上游 Python 版 `config/settings.json` 的 2K(2560×1440) 基准坐标，
///   逐个折算成屏幕百分比。上游是靠「按当前分辨率等比缩放」实现自适应的，这里改成
///   直接用百分比表达 —— 效果等价，且不会重现上游「重复 load 二次缩放漂移」的缺陷。
/// - **操作台**：实测游戏画面（1280×720 录屏帧）逐元素量取后折算。
///
/// 意义：**不校准也能直接用**。校准向导退化成微调手段，只有换分辨率、改游戏 UI 缩放、
/// 或者游戏大版本改版时才需要走一遍。
pub mod builtin {
    use super::{PctPoint, PctRect, RegionEntry, CANDIDATE_BOX_COUNT, CONSOLE_COLUMNS, CONSOLE_ROWS};

    const fn rect(x: f32, y: f32, w: f32, h: f32) -> PctRect {
        PctRect { x, y, w, h }
    }
    const fn point(x: f32, y: f32) -> PctPoint {
        PctPoint { x, y }
    }

    /// 摩斯三个点划区：上游 2K 坐标 (699/967/1234, 514, 200×50)。
    pub const MORSE: [PctRect; 3] = [
        rect(0.273047, 0.356944, 0.078125, 0.034722),
        rect(0.377734, 0.356944, 0.078125, 0.034722),
        rect(0.482031, 0.356944, 0.078125, 0.034722),
    ];

    /// 摩斯输入完成后依次点击的两个确认点：上游 2K 坐标 (1241/1359, 1060)。
    pub const CONFIRM_CLICKS: [PctPoint; 2] =
        [point(0.484766, 0.736111), point(0.530859, 0.736111)];

    /// 指纹人名区：上游 2K (706,657)-(866,689)。
    pub const NAME_REGION: PctRect = rect(0.275781, 0.456250, 0.062500, 0.022222);
    /// 指纹模式数字区：上游 2K (1030,530)-(1365,930)。
    pub const NUMBER_REGION: PctRect = rect(0.402344, 0.368056, 0.130859, 0.277778);
    /// 九宫格候选指纹：3 列 × 3 行，上游 2K 列 x=1522/1650/1778，行 y=560/687/815，格 105×105。
    pub const CANDIDATE_COL_X: [f32; 3] = [0.594531, 0.644531, 0.694531];
    pub const CANDIDATE_ROW_Y: [f32; 3] = [0.388889, 0.477083, 0.565972];
    pub const CANDIDATE_W: f32 = 0.041016;
    pub const CANDIDATE_H: f32 = 0.072917;

    /// 操作台 LED 目标序列：录屏帧 (869,113)，378×47。
    pub const LED_REGION: PctRect = rect(0.678906, 0.156944, 0.295313, 0.065278);
    /// 操作台面板：录屏帧 (66,86)，614×460。
    pub const PANEL_REGION: PctRect = rect(0.051563, 0.119444, 0.479688, 0.638889);
    /// 面板内 4 列的 x（面板宽度四等分中心）。
    pub const COL_X: [f32; CONSOLE_COLUMNS] = [0.111524, 0.231446, 0.351368, 0.471290];
    /// 面板内 4 行的 y（录屏帧实测 204 / 277 / 350 / 424，帧高 720）。
    pub const ROW_Y: [f32; CONSOLE_ROWS] = [0.283333, 0.384722, 0.486111, 0.588889];
    /// 旋钮中心：录屏帧实测约 (719, 331)/(1280×720)。
    pub const KNOB_CENTER: PctPoint = point(0.561719, 0.459722);

    /// AZ3 大保险：5×5 字符矩阵区域（1280×720 实测）。
    pub const AZ3_GRID_REGION: PctRect = rect(0.215, 0.330, 0.310, 0.300);
    /// AZ3 大保险：高亮目标行带。
    ///
    /// 与 [`AZ3_GRID_REGION`] **同 x 同 w**（`0.215 / 0.310`，右边界 0.525）：目标行带只在
    /// 网格的竖直方向上开一个横条，水平范围必须盖住整张 5×5 字符矩阵。
    /// 旧值 `x = 0.225 / w = 0.290`（右边界 0.515）比网格窄了 10 个千分点，
    /// 用户截图里网格区实测占 `x ≈ 0.27..0.55` —— 右边那一小截第 5 列会被切在带外，
    /// 高亮绿像素与等距字符列的计数都会偏低（大保险时判时不中）。
    pub const AZ3_TARGET_BAND: PctRect = rect(0.215, 0.440, 0.310, 0.120);
    /// AZ3 大保险：右侧「破解成功」圆钮（1280×720 实测）。
    pub const AZ3_CRACK_BUTTON: PctRect = rect(0.600, 0.330, 0.140, 0.240);
    /// AZ3 任务保险：5 槽横向轨道（1280×720 实测：y 0.225–0.315）。
    pub const AZ3_TRACK_BAND: PctRect = rect(0.285, 0.215, 0.415, 0.105);
    /// AZ3 任务保险：底部 5 个绿灯锁定条带（1280×720 实测）。
    pub const AZ3_INDICATOR_BAND: PctRect = rect(0.285, 0.300, 0.415, 0.120);
    /// AZ3 任务保险：5 槽中心 x 百分比（1280×720 实测）。
    pub const AZ3_PIN_CENTER_X: [f32; 5] = [0.325, 0.412, 0.495, 0.577, 0.660];
    /// AZ3 黄键台：右侧 4×4 按键网格（1280×720 实测：x 0.505–0.705、y 0.275–0.590）。
    pub const AZ3_BUTTON_GRID_REGION: PctRect = rect(0.500, 0.270, 0.210, 0.325);
    /// AZ3 黄键台：左侧屏幕底部 5 个进度勾选框（**2026-11-16 1080p 实机实测**）。
    ///
    /// 实机 1920×1080 逐像素量出的 5 格并集（720p 归一化口径）：`x 0.275..0.494`、
    /// `y 0.453..0.528`，即 `x=0.275, y=0.453, w=0.219, h=0.075`。
    ///
    /// 旧值 `x = 0.262 / w = 0.155`（右边界 **0.417**）窄到**右边只到第 3 格**：第 4、5 格
    /// 落在区域外。实机同一张 `task_1794777686_ok.png` 上 5 格是 `✓ ✓ ✓ ✓ ⬜`
    /// （逐格绿像素 174/164/185/239/**0**），日志却报「5/5 已勾选」—— 根因就是这个窄区域。
    pub const AZ3_PROGRESS_REGION: PctRect = rect(0.275, 0.453, 0.219, 0.075);
    /// AZ3 信号线台：右上 LED 序列区（1280×720 实测）。
    pub const AZ3_LINE_LED_REGION: PctRect = rect(0.632, 0.243, 0.225, 0.075);
    /// AZ3 信号线台：深蓝面板区（1280×720 实测）。
    pub const AZ3_LINE_PANEL_REGION: PctRect = rect(0.163, 0.255, 0.440, 0.440);
    /// AZ3 信号线台：4 列旋钮的 x 百分比（1280×720 实测）。
    pub const AZ3_LINE_COL_X: [f32; CONSOLE_COLUMNS] = [0.265, 0.363, 0.468, 0.545];
    /// AZ3 信号线台：4 行标签的 y 百分比（1280×720 实测）。
    pub const AZ3_LINE_ROW_Y: [f32; CONSOLE_ROWS] = [0.345, 0.405, 0.472, 0.540];
    /// AZ3 信号线台：红色旋钮中心（1280×720 实测）。
    pub const AZ3_LINE_KNOB_CENTER: PctPoint = point(0.735, 0.565);

    /// 摩斯三区条目（名字与校准向导一致，便于逐项覆盖）。
    pub fn regions() -> Vec<RegionEntry> {
        MORSE
            .iter()
            .enumerate()
            .map(|(index, rect)| RegionEntry {
                name: format!("morse_cell_{}", index + 1),
                rect: *rect,
            })
            .collect()
    }

    /// 九宫格候选（行主序，与上游 `candidate_boxes` 顺序一致）。
    pub fn candidate_boxes() -> Vec<PctRect> {
        let mut boxes = Vec::with_capacity(CANDIDATE_BOX_COUNT);
        for row in CANDIDATE_ROW_Y {
            for col in CANDIDATE_COL_X {
                boxes.push(rect(col, row, CANDIDATE_W, CANDIDATE_H));
            }
        }
        boxes
    }

    pub fn confirm_clicks() -> Vec<PctPoint> {
        CONFIRM_CLICKS.to_vec()
    }
}

/// 百分比矩形是否已设置（宽高都大于 0）。
///
/// 注意：`PctRect` 本身不带 `is_calibrated`（那是 `MonitorInfo` / `PxRect` 的方法），
/// 且**不能**用 `x/y != 0` 判断 —— 贴左边缘的合法区域 x 就是 0。
fn is_set(rect: PctRect) -> bool {
    rect.w > 0.0 && rect.h > 0.0
}

impl Config {
    /// 用内置坐标补齐「从未校准过」的区域。
    ///
    /// 只在对应区域**全空**时覆盖，已校准的值一律保留 —— 用户的微调结果不会被抹掉。
    /// 这是「开箱即用」的核心：新装、旧空配置、换机器都能直接跑，不必先进游戏框选。
    pub fn with_builtin_regions(mut self) -> Self {
        if self.regions.len() != MORSE_REGION_COUNT
            || self.regions.iter().all(|entry| !is_set(entry.rect))
        {
            self.regions = builtin::regions();
        }
        if !is_set(self.fingerprint.name_region) && !is_set(self.fingerprint.number_region) {
            self.fingerprint = FingerprintConfig {
                name_region: builtin::NAME_REGION,
                number_region: builtin::NUMBER_REGION,
                candidate_boxes: builtin::candidate_boxes(),
            };
        }
        if !is_set(self.console.panel_region) {
            self.console = ConsoleConfig {
                led_region: builtin::LED_REGION,
                panel_region: builtin::PANEL_REGION,
                col_x: builtin::COL_X.to_vec(),
                row_y: builtin::ROW_Y.to_vec(),
                knob_center: builtin::KNOB_CENTER,
            };
        }
        // 注意：**不回填 confirm_clicks**。它由 `auto_confirm` 开关在 `try_px_view` 里
        // 决定是否回退到内置坐标 —— 配置层保持「空就是空」，spec 不变量 4 才不被破坏。
        self
    }

    /// 注入「当前实际屏幕分辨率」（不落盘），供百分比→像素换算使用。
    ///
    /// 与持久化的 [`Config::monitor`]（上次校准时的分辨率基线）刻意分离：
    /// 前者用于换算与提示，后者只用于判断「屏幕是否变过」。
    pub fn with_runtime_monitor(mut self, monitor: MonitorInfo) -> Self {
        self.runtime_monitor = monitor;
        self
    }
}

// ───────────────────── 热键校验（拒绝会吞游戏键的注册） ─────────────────────

/// 不允许**单独**用作全局热键的键。
///
/// `F` 是游戏交互键（开门/拾取）：`RegisterHotKey` 注册无修饰的 F 会全局独占，
/// 游戏里按 F 全部失效 —— 这正是 spec D3 宁愿写低级键盘钩子也不用 `RegisterHotKey("F")`
/// 的唯一原因。所以配置层直接拒绝，带修饰键（`Ctrl+F`）则允许：组合键不占用单键。
pub const RESERVED_SINGLE_HOTKEYS: [&str; 1] = ["F"];

/// 被拒的热键：字段名 + 原文 + 人话原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HotkeyRejection {
    /// 配置字段（如 `hotkeys.scene`）。
    pub field: &'static str,
    /// 配置里的原文。
    pub hotkey: String,
    /// 为什么不行。
    pub reason: &'static str,
}

impl std::fmt::Display for HotkeyRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} = {:?} 不可用：{}",
            self.field, self.hotkey, self.reason
        )
    }
}

/// 无修饰的单键 `F` → `Some(原因)`；其它写法 → `None`。
///
/// 纯字符串判定：平台层的 `parse_hotkey` 必须**继续**能解析 `F`（F 键监听要用它），
/// 「不许拿它做全局热键」是配置策略，不是解析能力。
pub fn single_key_rejection(hotkey: &str) -> Option<&'static str> {
    let parts: Vec<&str> = hotkey
        .split('+')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .collect();
    let [main] = parts[..] else {
        return None;
    };
    RESERVED_SINGLE_HOTKEYS
        .iter()
        .any(|reserved| main.eq_ignore_ascii_case(reserved))
        .then_some("F 是游戏交互键，会被全局独占；请用 F5~F8、符号键，或直接靠 F 键监听")
}

impl Hotkeys {
    /// 七个字段（按配置顺序），供逐一校验。
    pub fn fields(&self) -> [(&'static str, &str); 7] {
        [
            ("hotkeys.scene", &self.scene),
            ("hotkeys.morse", &self.morse),
            ("hotkeys.fingerprint", &self.fingerprint),
            ("hotkeys.console", &self.console),
            ("hotkeys.keys", &self.keys),
            ("hotkeys.line", &self.line),
            ("hotkeys.master", &self.master),
        ]
    }
}

impl Config {
    /// 列出所有被拒的热键（空 = 全部可用）。
    ///
    /// 与 [`Config::validate`] 分开：那个列表只装「未校准区域」，UI 会把它们显示成
    /// 「请先校准」；热键写法问题混进去会让主按钮的修复动作指错方向。
    pub fn validate_hotkeys(&self) -> Vec<HotkeyRejection> {
        self.hotkeys
            .fields()
            .into_iter()
            .filter_map(|(field, value)| {
                single_key_rejection(value).map(|reason| HotkeyRejection {
                    field,
                    hotkey: value.to_owned(),
                    reason,
                })
            })
            .collect()
    }

    /// 首个被拒的热键（启动时据此回落默认键 + 提示）。
    pub fn hotkey_rejection(&self) -> Option<HotkeyRejection> {
        self.validate_hotkeys().into_iter().next()
    }
}

/// 时序常量。默认值与上游逐一一致（spec §2.2 不变量 6），全部可配置。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Timings {
    /// 摩斯识别成功后开始输入数字前的延迟（默认 20ms，防输入过快卡死）。
    pub morse_recognize_delay_ms: u64,
    /// 摩斯输入后等待（点击下载/确认按钮前延时，默认 600ms）。
    pub morse_after_input_ms: u64,
    /// 黄键台识别成功后开始点击前的延迟（默认 100ms）。
    pub keys_recognize_delay_ms: u64,
    /// 黄键台首个按钮额外延迟（默认 50ms，防开场动画吞掉首击）。
    pub first_key_extra_delay_ms: u64,
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
            morse_recognize_delay_ms: 20,
            morse_after_input_ms: 600,
            keys_recognize_delay_ms: 100,
            first_key_extra_delay_ms: 50,
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

/// 设置面板里可模块化调节的延时项。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TimingField {
    /// 摩斯识别后输入延迟（默认 20ms）。
    MorseRecognizeDelay,
    /// 摩斯输入后点击下载/确认延迟（默认 600ms）。
    MorseConfirmDelay,
    /// 黄键台识别后点击延迟（默认 100ms）。
    KeysRecognizeDelay,
    /// 黄键台首个按钮额外延迟（默认 50ms）。
    FirstKeyExtraDelay,
    /// 连续按键/点击间隔（默认 50ms）。
    ActionInterval,
}

impl TimingField {
    pub const ALL: [Self; 5] = [
        Self::MorseRecognizeDelay,
        Self::MorseConfirmDelay,
        Self::KeysRecognizeDelay,
        Self::FirstKeyExtraDelay,
        Self::ActionInterval,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::MorseRecognizeDelay => "摩斯识别后延时",
            Self::MorseConfirmDelay => "点击下载前延时",
            Self::KeysRecognizeDelay => "黄键识别后延时",
            Self::FirstKeyExtraDelay => "黄键首键额外延时",
            Self::ActionInterval => "连续按键/点击间隔",
        }
    }

    pub const fn step_ms(self) -> u64 {
        match self {
            Self::MorseConfirmDelay => 50,
            _ => 10,
        }
    }

    pub const fn max_ms(self) -> u64 {
        match self {
            Self::MorseConfirmDelay => 3000,
            Self::KeysRecognizeDelay | Self::MorseRecognizeDelay => 2000,
            Self::FirstKeyExtraDelay | Self::ActionInterval => 1000,
        }
    }

    pub const fn min_ms(self) -> u64 {
        match self {
            Self::ActionInterval => 10,
            _ => 0,
        }
    }

    pub const fn default_ms(self) -> u64 {
        match self {
            Self::MorseRecognizeDelay => 20,
            Self::MorseConfirmDelay => 600,
            Self::KeysRecognizeDelay => 100,
            Self::FirstKeyExtraDelay => 50,
            Self::ActionInterval => 50,
        }
    }

    pub const fn read(self, timings: &Timings) -> u64 {
        match self {
            Self::MorseRecognizeDelay => timings.morse_recognize_delay_ms,
            Self::MorseConfirmDelay => timings.morse_after_input_ms,
            Self::KeysRecognizeDelay => timings.keys_recognize_delay_ms,
            Self::FirstKeyExtraDelay => timings.first_key_extra_delay_ms,
            Self::ActionInterval => timings.key_interval_ms,
        }
    }

    pub fn write(self, timings: &mut Timings, value: u64) {
        let clamped = value.clamp(self.min_ms(), self.max_ms());
        match self {
            Self::MorseRecognizeDelay => timings.morse_recognize_delay_ms = clamped,
            Self::MorseConfirmDelay => timings.morse_after_input_ms = clamped,
            Self::KeysRecognizeDelay => timings.keys_recognize_delay_ms = clamped,
            Self::FirstKeyExtraDelay => timings.first_key_extra_delay_ms = clamped,
            Self::ActionInterval => {
                timings.key_interval_ms = clamped;
                timings.click_interval_ms = clamped;
            }
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
    pub az3: PxAz3,
    pub confirm_clicks: Vec<PxPoint>,
}

/// AZ3 4 类小游戏的像素视图（只读，不落盘）。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PxAz3 {
    pub grid_region: PxRect,
    pub target_band: PxRect,
    pub crack_button: PxRect,
    pub timing_window_px: i32,
    pub track_band: PxRect,
    pub indicator_band: PxRect,
    pub pin_center_x: Vec<i32>,
    pub pin_tolerance_px: i32,
    /// 黄键台 4×4 键阵。**保留仅为向后兼容**（已改自适应定位，不再被读取）。
    pub button_grid_region: PxRect,
    /// 黄键台进度勾选格。**保留仅为向后兼容**（已改自适应定位，不再被读取）。
    pub progress_region: PxRect,
    pub line_led_region: PxRect,
    pub line_panel_region: PxRect,
    pub line_col_x: Vec<i32>,
    pub line_row_y: Vec<i32>,
    pub line_knob_center: PxPoint,
    pub max_duration_ms: u64,
    pub frame_budget: u32,
    pub poll_interval_ms: u64,
    pub keys_recognize_delay_ms: u64,
    pub first_key_extra_delay_ms: u64,
    pub click_interval_ms: u64,
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

/// F 钩子的默认前台关键词：两个名字都是这款游戏自己标题里的词。
fn default_game_title_keywords() -> Vec<String> {
    vec!["三角洲".to_owned(), "Delta Force".to_owned()]
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

/// `CaptureBackend` 的 serde 桥接：配置里只出现稳定 token（`wgc` / `bitblt`）。
///
/// 枚举定义在 `delta-capture`（那边不依赖本 crate），因此这里用 `with` 提供读写，
/// 而不是把枚举复制一份 —— 单一来源，UI 与流程拿到的永远是同一个类型。
mod capture_backend_serde {
    use delta_capture::CaptureBackend;
    use serde::{Deserialize, Deserializer, Serializer};

    use super::parse_backend_token;

    pub fn serialize<S: Serializer>(
        backend: &CaptureBackend,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(backend.token())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<CaptureBackend, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(parse_backend_token(&raw))
    }
}

/// 配置里的后端字符串 → 枚举。
///
/// 显式边界（spec D1）：**未知值回落 `Wgc` 并留痕**，绝不 panic、也不把整份配置读废 ——
/// 一份写错后端名的配置不该让用户的校准坐标一起丢掉。
fn parse_backend_token(raw: &str) -> CaptureBackend {
    match raw.trim().to_ascii_lowercase().as_str() {
        "wgc" => CaptureBackend::Wgc,
        "bitblt" => CaptureBackend::BitBlt,
        _ => {
            eprintln!("[config] 未知截图后端 {raw:?}，回落 wgc");
            CaptureBackend::Wgc
        }
    }
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
    fn game_title_keywords_default_to_the_game_and_survive_old_configs() {
        let config = Config::default();
        assert_eq!(
            config.game_title_keywords,
            vec!["三角洲".to_owned(), "Delta Force".to_owned()],
            "spec D3 边界 5 的默认关键词"
        );

        // 老配置（没有这个字段）→ 落到默认关键词，而不是空列表（空列表 = 永不响应）。
        let legacy: Config = serde_json::from_str(r#"{"ocr_enabled":true}"#).expect("老配置合法");
        assert_eq!(legacy.game_title_keywords, config.game_title_keywords);

        // 用户增删过的关键词必须原样往返（设置页改完要跨启动保持）。
        let custom: Config =
            serde_json::from_str(r#"{"game_title_keywords":["Arena"]}"#).expect("可读");
        assert_eq!(custom.game_title_keywords, vec!["Arena".to_owned()]);
        let dumped = serde_json::to_string(&custom).expect("序列化");
        assert!(dumped.contains("Arena"), "{dumped}");
        assert_eq!(
            serde_json::from_str::<Config>(&dumped)
                .expect("反序列化")
                .game_title_keywords,
            vec!["Arena".to_owned()]
        );
    }

    #[test]
    fn f_key_hook_defaults_to_on_and_survives_old_configs() {
        assert!(Config::default().f_key_hook_enabled, "默认开");

        let legacy: Config = serde_json::from_str(r#"{"ocr_enabled":true}"#).expect("老配置合法");
        assert!(legacy.f_key_hook_enabled, "老配置缺字段必须回落 true");

        let off: Config =
            serde_json::from_str(r#"{"f_key_hook_enabled":false}"#).expect("可读");
        assert!(!off.f_key_hook_enabled);
        let dumped = serde_json::to_string(&off).expect("序列化");
        assert!(dumped.contains("\"f_key_hook_enabled\":false"), "{dumped}");
        assert!(
            !serde_json::from_str::<Config>(&dumped)
                .expect("反序列化")
                .f_key_hook_enabled,
            "关闭状态必须跨启动保持"
        );
    }

    #[test]
    fn master_hotkey_defaults_to_f9_and_survives_old_configs() {
        // 老配置没有这个字段时必须是 F9，不能是空串（空串注册不上 = 功能静默消失）。
        assert_eq!(Config::default().hotkeys.master, "F9");
        let legacy: Config = serde_json::from_str(r#"{"hotkeys":{"scene":"F8"}}"#).expect("老配置合法");
        assert_eq!(legacy.hotkeys.master, "F9", "缺字段必须回落 F9");

        // 改过之后必须原样回得来（跨启动保持）。
        let custom: Config =
            serde_json::from_str(r#"{"hotkeys":{"master":"Ctrl+Alt+M"}}"#).expect("可读");
        assert_eq!(custom.hotkeys.master, "Ctrl+Alt+M");
        let dumped = serde_json::to_string(&custom).expect("序列化");
        assert!(dumped.contains("\"master\":\"Ctrl+Alt+M\""), "{dumped}");
        assert_eq!(
            serde_json::from_str::<Config>(&dumped)
                .expect("反序列化")
                .hotkeys
                .master,
            "Ctrl+Alt+M"
        );
    }

    #[test]
    fn every_hotkey_slot_owns_one_index_and_one_field() {
        // 槽位下标直接当数组下标用（热键泵的 `bound[]`）：重复就是串键，越界就是崩。
        let slots = HotkeySlot::ALL.len();
        let mut seen = vec![false; slots];
        let mut names = Vec::new();
        for slot in HotkeySlot::ALL {
            let index = slot.index();
            assert!(index < slots, "{slot:?} 下标越界");
            assert!(!seen[index], "{slot:?} 的下标与别的槽位撞了");
            seen[index] = true;
            // 写进去的必须从这个槽读得回来（读写指向同一字段）。
            let mut keys = Hotkeys::default();
            let marker = format!("{slot:?}-测试键");
            HotkeySlot::write(slot, &mut keys, marker.clone());
            assert_eq!(HotkeySlot::read(slot, &keys), marker, "{slot:?} 读写不是同一个字段");
            names.push(slot.read(&Hotkeys::default()).to_owned());
        }
        assert!(seen.iter().all(|hit| *hit), "ALL 必须覆盖全部槽位");
        assert_eq!(
            Hotkeys::default().fields().len(),
            HotkeySlot::ALL.len(),
            "校验列表漏一项 = 某个键永远不被校验"
        );
        // 默认键互不相同：同一个键注册两次只会成功一个。
        let mut unique = names.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), names.len(), "默认键位重复：{names:?}");
    }

    #[test]
    fn the_two_force_az3_hotkeys_default_to_f10_and_f11_and_survive_old_configs() {
        // 用户拍板：黄键台 / 横线台各有独立强制键，默认 F10 / F11。
        let default = Config::default().hotkeys;
        assert_eq!(default.keys, "F10");
        assert_eq!(default.line, "F11");

        // 老配置里没有这两个字段 ⇒ 必须回落默认值，不能是空串
        // （空串注册不上 = 功能静默消失，用户按 F10 毫无反应且没有任何提示）。
        let legacy: Config = serde_json::from_str(r#"{"hotkeys":{"scene":"F8"}}"#).expect("老配置合法");
        assert_eq!(legacy.hotkeys.keys, "F10", "缺字段必须回落 F10");
        assert_eq!(legacy.hotkeys.line, "F11", "缺字段必须回落 F11");

        // 改过之后必须原样回得来（跨启动保持）。
        let custom: Config = serde_json::from_str(r#"{"hotkeys":{"keys":"Ctrl+Alt+K","line":"Ctrl+Alt+L"}}"#)
            .expect("可读");
        assert_eq!(custom.hotkeys.keys, "Ctrl+Alt+K");
        assert_eq!(custom.hotkeys.line, "Ctrl+Alt+L");
        let dumped = serde_json::to_string(&custom).expect("序列化");
        let round_trip: Config = serde_json::from_str(&dumped).expect("反序列化");
        assert_eq!(round_trip.hotkeys.keys, "Ctrl+Alt+K");
        assert_eq!(round_trip.hotkeys.line, "Ctrl+Alt+L");
    }

    #[test]
    fn legacy_hotkey_fields_are_ignored_not_fatal() {
        // 五个键都能配（对齐上游 F5/F6，另加操作台与总开关）；未知字段静默忽略（前向兼容）。
        let json = r#"{"hotkeys":{"scene":"F7","morse":"F9","fingerprint":"F10","console":"F11","master":"F12","future":"X1"}}"#;
        let config: Config = serde_json::from_str(json).expect("配置必须仍能解析");
        assert_eq!(config.hotkeys.scene, "F7");
        assert_eq!(config.hotkeys.morse, "F9");
        assert_eq!(config.hotkeys.fingerprint, "F10");
        assert_eq!(config.hotkeys.console, "F11");
        assert_eq!(config.hotkeys.master, "F12");
    }

    #[test]
    fn capture_backend_defaults_to_wgc_and_round_trips() {
        let config = Config::default();
        assert_eq!(config.capture_backend, CaptureBackend::Wgc, "spec D1：默认 WGC");

        let json = serde_json::to_string(&config).expect("序列化");
        assert!(json.contains("\"capture_backend\":\"wgc\""), "{json}");
        let parsed: Config = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(parsed.capture_backend, CaptureBackend::Wgc);

        let bitblt: Config =
            serde_json::from_str(r#"{"capture_backend":"bitblt"}"#).expect("兼容档必须可读");
        assert_eq!(bitblt.capture_backend, CaptureBackend::BitBlt);
        assert!(serde_json::to_string(&bitblt)
            .expect("序列化")
            .contains("\"capture_backend\":\"bitblt\""));
    }

    #[test]
    fn always_on_top_defaults_to_off_and_round_trips() {
        // 默认**不置顶**：窗口要被游戏盖住、挂在后台，需要时才由标题条图钉浮起来。
        assert!(!Config::default().always_on_top, "默认必须不置顶");
        let json = serde_json::to_string(&Config::default()).expect("序列化");
        assert!(json.contains("\"always_on_top\":false"), "{json}");

        // 老配置（没有这个字段）读进来也是不置顶，不会被默认值改成置顶。
        let legacy: Config = serde_json::from_str(r#"{"ocr_enabled":true}"#).expect("老配置合法");
        assert!(!legacy.always_on_top);

        // 打开后必须原样回得来（开关状态要跨启动保持）。
        let pinned: Config =
            serde_json::from_str(r#"{"always_on_top":true}"#).expect("可读");
        assert!(pinned.always_on_top);
        let dumped = serde_json::to_string(&pinned).expect("序列化");
        assert!(dumped.contains("\"always_on_top\":true"), "{dumped}");
        assert!(
            serde_json::from_str::<Config>(&dumped)
                .expect("反序列化")
                .always_on_top
        );
    }

    #[test]
    fn theme_id_defaults_to_frost_and_survives_old_configs() {
        assert_eq!(Config::default().theme_id, ThemeId::Frost, "spec D7：默认霜白");
        // 老配置没有这个字段 → 回落 Frost，不是深色主题。
        let legacy: Config = serde_json::from_str(r#"{"ocr_enabled":true}"#).expect("老配置合法");
        assert_eq!(legacy.theme_id, ThemeId::Frost);

        // 六套都能写能读（token 是小写下划线风格）。
        for (json, expected) in [
            ("graphite", ThemeId::Graphite),
            ("nightwatch", ThemeId::Nightwatch),
            ("ember", ThemeId::Ember),
            ("paper", ThemeId::Paper),
            ("frost", ThemeId::Frost),
            ("celadon", ThemeId::Celadon),
        ] {
            let config: Config = serde_json::from_str(&format!(r#"{{"theme_id":"{json}"}}"#))
                .expect("主题 token 必须可读");
            assert_eq!(config.theme_id, expected);
            let dumped = serde_json::to_string(&config).expect("序列化");
            assert!(dumped.contains(&format!("\"theme_id\":\"{json}\"")), "{dumped}");
            assert_eq!(
                serde_json::from_str::<Config>(&dumped).expect("反序列化").theme_id,
                expected,
                "往返必须不变"
            );
        }
        // 未知主题名回落默认（前向兼容，不把整份配置读废）。
        let unknown: Config = serde_json::from_str(r#"{"theme_id":"neon"}"#)
            .expect("未知主题名不得让配置读不出来");
        assert_eq!(unknown.theme_id, ThemeId::Frost);
    }

    #[test]
    fn accent_override_round_trips_and_accepts_null() {
        assert_eq!(Config::default().accent_override, None, "默认用主题自带主色");
        let legacy: Config = serde_json::from_str("{}").expect("空配置合法");
        assert_eq!(legacy.accent_override, None);

        let custom: Config =
            serde_json::from_str(r##"{"accent_override":"#B23A2A"}"##).expect("可读");
        assert_eq!(custom.accent_override.as_deref(), Some("#B23A2A"));
        let dumped = serde_json::to_string(&custom).expect("序列化");
        assert_eq!(
            serde_json::from_str::<Config>(&dumped)
                .expect("反序列化")
                .accent_override
                .as_deref(),
            Some("#B23A2A")
        );
    }

    /// W-T1：旧配置没有 `flows` 键 ⇒ 7 个全开（升级不许把用户在用的功能悄悄关掉）。
    ///
    /// 反例：`flows` 字段上的 `#[serde(default)]` 漏写 → 缺字段按 `false` 填 → 全关，
    /// 主按钮当场变「请开启至少一项」，用户的功能看着像被升级弄没了。
    #[test]
    fn legacy_config_without_flows_switches_everything_on() {
        let legacy: Config = serde_json::from_str(r#"{"ocr_enabled":true}"#).expect("老配置合法");
        assert_eq!(legacy.flows, FlowFlags::all_on(), "缺字段必须全开");
        assert!(legacy.flows.any_on(), "全开必须满足 any_on");
        for flow in Flow::ALL {
            assert!(legacy.flows.get(flow), "「{}」不该被升级关掉", flow.label());
        }
    }

    /// 关闭状态必须跨启动保持，且写回时 **7 个字段全写**（不做部分保存）。
    #[test]
    fn flow_switches_survive_a_round_trip_with_all_seven_fields_written() {
        let config: Config = serde_json::from_str(r#"{"flows":{"morse":false,"az3_keys":false}}"#)
            .expect("可读");
        assert!(!config.flows.get(Flow::Morse), "配置里写的必须生效");
        assert!(!config.flows.get(Flow::Az3Keys));
        assert!(config.flows.get(Flow::Fingerprint), "没写的字段按默认（开）补齐");

        let dumped = serde_json::to_string(&config).expect("序列化");
        for key in [
            "morse",
            "fingerprint",
            "console",
            "az3_timing",
            "az3_pins",
            "az3_keys",
            "az3_line",
        ] {
            assert!(
                dumped.contains(&format!("\"{key}\":")),
                "写回必须包含 {key}，实际：{dumped}"
            );
        }
        let round_trip: Config = serde_json::from_str(&dumped).expect("反序列化");
        assert_eq!(round_trip.flows, config.flows, "往返必须不变");
        assert!(!round_trip.flows.get(Flow::Morse), "关闭状态必须跨启动保持");
    }

    /// W-T6：`any_on()` 必须覆盖全部 7 个开关；全关为 `false`。
    ///
    /// 反例：漏算 4 个 AZ3 开关 → 只开「信号线台」时仍判「没有任何流程可跑」。
    #[test]
    fn any_on_counts_all_seven_switches() {
        assert!(
            !FlowFlags::from_flags([false; Flow::ALL.len()]).any_on(),
            "7 个全关 = 没有流程"
        );
        assert!(FlowFlags::all_on().any_on());
        for (index, flow) in Flow::ALL.into_iter().enumerate() {
            let mut flags = [false; Flow::ALL.len()];
            flags[index] = true;
            assert!(
                FlowFlags::from_flags(flags).any_on(),
                "只开「{}」也算有流程可用",
                flow.label()
            );
        }
    }

    /// `flags()` / `from_flags()` / `get()` 必须指向同一批字段。
    ///
    /// 反例：`from_flags` 里两个下标抄重 → 关一个开关其实关掉了另一个（旧配置位序串台）。
    #[test]
    fn flag_order_and_getters_stay_in_sync() {
        for (index, flow) in Flow::ALL.into_iter().enumerate() {
            assert_eq!(flow.index(), index, "下标必须等于声明位置");
            let mut flags = [false; Flow::ALL.len()];
            flags[index] = true;
            let built = FlowFlags::from_flags(flags);
            assert!(built.get(flow), "「{}」的位序与 get() 不一致", flow.label());
            assert_eq!(built.flags(), flags, "flags() 必须原样还原位序");
        }
        // 一次只关一个：其余 6 个不许跟着变。
        for flow in Flow::ALL {
            let mut flags = [true; Flow::ALL.len()];
            flags[flow.index()] = false;
            let off = FlowFlags::from_flags(flags);
            for other in Flow::ALL.into_iter().filter(|other| *other != flow) {
                assert!(
                    off.get(other),
                    "关「{}」时「{}」跟着变了",
                    flow.label(),
                    other.label()
                );
            }
        }
    }

    #[test]
    fn plain_f_is_rejected_as_a_global_hotkey_but_ctrl_f_is_allowed() {
        // 复审 S2 遗留③：无修饰的 F 会被 RegisterHotKey 全局独占，吞掉游戏的交互键。
        let config: Config =
            serde_json::from_str(r#"{"hotkeys":{"scene":"F"}}"#).expect("配置仍要能解析");
        let rejected = config.validate_hotkeys();
        assert_eq!(rejected.len(), 1, "F 必须被拒：{rejected:?}");
        assert_eq!(rejected[0].field, "hotkeys.scene");
        assert!(rejected[0].reason.contains("游戏交互键"), "{}", rejected[0]);
        assert!(config.hotkey_rejection().is_some());

        // 小写、带空格的单键 F 同样拒绝（写法等价）。
        for spelling in ["f", " F ", "F "] {
            let config: Config =
                serde_json::from_str(&format!(r#"{{"hotkeys":{{"morse":"{spelling}"}}}}"#))
                    .expect("可读");
            assert_eq!(config.validate_hotkeys().len(), 1, "{spelling:?} 必须被拒");
        }

        // 带修饰键不独占单键 → 允许；F5~F8 与符号键当然也允许。
        for allowed in ["Ctrl+F", "Ctrl+Alt+F", "F5", "F8", "/", "`"] {
            assert!(
                single_key_rejection(allowed).is_none(),
                "{allowed:?} 不该被拒"
            );
        }
        // 四个字段一起校验，默认全部可用。
        assert!(Config::default().validate_hotkeys().is_empty());
    }

    #[test]
    fn unknown_capture_backend_falls_back_to_wgc_instead_of_losing_the_config() {
        // 写错后端名不该让用户的校准坐标一起丢掉（spec D1 显式边界）。
        let config: Config =
            serde_json::from_str(r#"{"capture_backend":"dwm","ocr_enabled":false}"#)
                .expect("未知后端不得让整份配置读不出来");
        assert_eq!(config.capture_backend, CaptureBackend::Wgc);
        assert!(!config.ocr_enabled, "同份配置的其它字段必须照常生效");

        // 大小写不敏感（与 `token()` 的规范写法等价）。
        let config: Config =
            serde_json::from_str(r#"{"capture_backend":" BitBlt "}"#).expect("大小写不敏感");
        assert_eq!(config.capture_backend, CaptureBackend::BitBlt);

        // 缺字段（老配置）同样回落 WGC。
        let config: Config = serde_json::from_str("{}").expect("空配置合法");
        assert_eq!(config.capture_backend, CaptureBackend::Wgc);
    }
}
