//! 校准向导的状态机与坐标落库（spec D9）。
//!
//! 五步闭环：`选场景 → 框选区域 → 排错复看 → 试跑校验 → 保存`。
//! 每一步只让用户做一个动作、可回退（Esc）、可跳过；坐标一律存**百分比**，
//! 换算用主显示器物理分辨率（`delta-config::Config::try_px_view`）。
//!
//! 区域清单是**表驱动**的：新增区域只加一行数据，不动步骤逻辑。

use delta_config::Config;
use delta_geom::{PctPoint, PctRect};
/// 向导可校准的对象（第四个是 spec 风险评估里的「补录指纹模板」）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WizardScene {
    Morse,
    Fingerprint,
    Console,
    Recapture,
}

impl WizardScene {
    /// 三张场景卡片 + 补录入口，按显示顺序。
    pub const ALL: [Self; 4] = [
        Self::Morse,
        Self::Fingerprint,
        Self::Console,
        Self::Recapture,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Morse => "摩斯解码",
            Self::Fingerprint => "指纹识别",
            Self::Console => "操作台",
            Self::Recapture => "补录模板",
        }
    }

    /// 一句话说明（≤10 字，告诉用户这一步要框什么）。
    pub fn hint(self) -> &'static str {
        match self {
            Self::Morse => "框三个摩斯格",
            Self::Fingerprint => "框人名与九格",
            Self::Console => "框面板与旋钮",
            Self::Recapture => "截九格填角色",
        }
    }
}

/// 一次框选产物的语义（决定落在配置的哪个字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// 普通矩形区域。
    Rect,
    /// 列线：取框的中心 x。
    Column(usize),
    /// 行线：取框的中心 y。
    Row(usize),
    /// 点：取框的中心。
    Point,
}

/// 一个待校准项。
#[derive(Debug, Clone, Copy)]
pub struct TargetSpec {
    pub label: &'static str,
    pub kind: TargetKind,
}

/// 摩斯 3 格（spec §2.2：可只框 1 个，缺的流程灰置）。
pub const MORSE_TARGETS: [TargetSpec; 3] = [
    TargetSpec {
        label: "第一格",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "第二格",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "第三格",
        kind: TargetKind::Rect,
    },
];

/// 指纹：人名区 + 数字区 + 9 个候选格。
pub const FINGERPRINT_TARGETS: [TargetSpec; 11] = [
    TargetSpec {
        label: "角色名",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "数字区",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 1",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 2",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 3",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 4",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 5",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 6",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 7",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 8",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "候选 9",
        kind: TargetKind::Rect,
    },
];

/// 操作台：LED / 面板 / 4 列线 / 4 行线 / 旋钮中心。
pub const CONSOLE_TARGETS: [TargetSpec; 11] = [
    TargetSpec {
        label: "目标序列",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "面板",
        kind: TargetKind::Rect,
    },
    TargetSpec {
        label: "列 1",
        kind: TargetKind::Column(0),
    },
    TargetSpec {
        label: "列 2",
        kind: TargetKind::Column(1),
    },
    TargetSpec {
        label: "列 3",
        kind: TargetKind::Column(2),
    },
    TargetSpec {
        label: "列 4",
        kind: TargetKind::Column(3),
    },
    TargetSpec {
        label: "行 1",
        kind: TargetKind::Row(0),
    },
    TargetSpec {
        label: "行 2",
        kind: TargetKind::Row(1),
    },
    TargetSpec {
        label: "行 3",
        kind: TargetKind::Row(2),
    },
    TargetSpec {
        label: "行 4",
        kind: TargetKind::Row(3),
    },
    TargetSpec {
        label: "旋钮中心",
        kind: TargetKind::Point,
    },
];

/// 某场景要框的区域清单。
pub fn targets(scene: WizardScene) -> &'static [TargetSpec] {
    match scene {
        WizardScene::Morse => &MORSE_TARGETS,
        WizardScene::Fingerprint => &FINGERPRINT_TARGETS,
        WizardScene::Console => &CONSOLE_TARGETS,
        // 补录模板不是框选，走「截 9 格 + 填角色名」的另一条路径。
        WizardScene::Recapture => &FINGERPRINT_TARGETS[2..],
    }
}

/// 向导五步。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    PickScene,
    SelectRegion,
    Review,
    TestRun,
    Save,
}

impl Step {
    /// 显示用步序（1..=5）。
    pub fn index(self) -> usize {
        match self {
            Self::PickScene => 1,
            Self::SelectRegion => 2,
            Self::Review => 3,
            Self::TestRun => 4,
            Self::Save => 5,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::PickScene => "选场景",
            Self::SelectRegion => "框选区域",
            Self::Review => "排错复看",
            Self::TestRun => "试跑校验",
            Self::Save => "保存坐标",
        }
    }

    /// 主按钮文案（动词开头、≤6 字）。
    pub fn primary(self) -> &'static str {
        match self {
            Self::PickScene => "下一步",
            Self::SelectRegion => "对，继续",
            Self::Review => "对，继续",
            Self::TestRun => "能用，保存",
            Self::Save => "已完成",
        }
    }

    /// 「重画一次」类次要按钮是否需要显示。
    pub fn has_redo(self) -> bool {
        matches!(self, Self::SelectRegion | Self::Review | Self::TestRun)
    }
}

/// 试跑校验的结论（只读识别，不产生任何输入）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 还没试跑。
    Pending,
    /// 读到了内容。
    Read(String),
    /// 读不出来。
    Failed(String),
}

/// 向导状态机。
#[derive(Debug, Clone)]
pub struct Wizard {
    pub scene: Option<WizardScene>,
    pub step: Step,
    /// 当前正在框的区域下标。
    pub cursor: usize,
    /// 当前区域是否已框（松手即存）。
    pub drawn: Vec<bool>,
    pub verdict: Verdict,
    /// 保存完成后显示「已保存 · 换分辨率也能用」。
    pub saved: bool,
    /// 补录模板时的角色名（只有 `Recapture` 用）。
    pub role_name: String,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            scene: None,
            step: Step::PickScene,
            cursor: 0,
            drawn: Vec::new(),
            verdict: Verdict::Pending,
            saved: false,
            role_name: String::new(),
        }
    }
}

impl Wizard {
    pub fn new() -> Self {
        Self::default()
    }

    /// 选定要校准的场景，进入框选步。
    pub fn choose(&mut self, scene: WizardScene) {
        self.scene = Some(scene);
        self.step = Step::SelectRegion;
        self.cursor = 0;
        self.drawn = vec![false; targets(scene).len()];
        self.verdict = Verdict::Pending;
        self.saved = false;
        self.role_name.clear();
    }

    /// 当前场景的区域清单。
    pub fn targets(&self) -> &'static [TargetSpec] {
        self.scene.map_or(&[], targets)
    }

    /// 当前正在框的区域。
    pub fn current(&self) -> Option<&'static TargetSpec> {
        self.targets().get(self.cursor)
    }

    /// 记录一次拖框完成：松手即存，不弹确认按钮。
    pub fn record_drawn(&mut self) {
        if let Some(slot) = self.drawn.get_mut(self.cursor) {
            *slot = true;
        }
        // 还有没框的区域就自动挪到下一个，省一次点击。
        if let Some(next) = self.drawn.iter().position(|drawn| !drawn) {
            self.cursor = next;
        }
    }

    /// 已框区域数。
    pub fn drawn_count(&self) -> usize {
        self.drawn.iter().filter(|drawn| **drawn).count()
    }

    /// 能否前进：框选步要求至少框了一个区域（缺的流程灰置，允许跳过）。
    pub fn can_advance(&self) -> bool {
        match self.step {
            Step::PickScene => self.scene.is_some(),
            Step::SelectRegion => self.drawn_count() > 0,
            Step::Review => self.drawn_count() > 0,
            // 补录模板不走试跑（它不读画面，只把九格存成模板）。
            Step::TestRun => {
                self.scene == Some(WizardScene::Recapture)
                    || matches!(self.verdict, Verdict::Read(_))
            }
            Step::Save => true,
        }
    }

    /// 前进一步；不允许时返回 `false`。
    pub fn advance(&mut self) -> bool {
        if !self.can_advance() {
            return false;
        }
        self.step = match self.step {
            Step::PickScene => Step::SelectRegion,
            Step::SelectRegion => Step::Review,
            // 补录模板没有「试跑」这一步：直接进保存。
            Step::Review if self.scene == Some(WizardScene::Recapture) => Step::Save,
            Step::Review => Step::TestRun,
            Step::TestRun => Step::Save,
            Step::Save => {
                self.saved = true;
                Step::Save
            }
        };
        true
    }

    /// 回退一步（Esc）；已在第一步返回 `false`，已框区域保留。
    pub fn back(&mut self) -> bool {
        let previous = match self.step {
            Step::PickScene => return false,
            Step::SelectRegion => Step::PickScene,
            Step::Review => Step::SelectRegion,
            Step::TestRun => Step::Review,
            Step::Save => Step::TestRun,
        };
        self.step = previous;
        self.saved = false;
        true
    }

    /// 重新框当前区域（「重画一次」）。
    pub fn redo(&mut self) {
        if let Some(slot) = self.drawn.get_mut(self.cursor) {
            *slot = false;
        }
        self.verdict = Verdict::Pending;
    }

    /// 关闭向导的条件：保存完成。
    pub fn is_finished(&self) -> bool {
        self.saved
    }
}

/// 把一次框选结果（百分比矩形）落进配置。
///
/// 只写百分比，不写像素：像素换算发生在 `Config::try_px_view`，因此换分辨率也不失效（spec D9）。
pub fn apply_drawn(config: &mut Config, scene: WizardScene, spec: TargetSpec, rect: PctRect) {
    match (scene, spec.kind) {
        (WizardScene::Morse, TargetKind::Rect) => {
            let index = MORSE_TARGETS
                .iter()
                .position(|candidate| candidate.label == spec.label)
                .unwrap_or(0);
            while config.regions.len() <= index {
                config.regions.push(delta_config::RegionEntry {
                    name: format!("morse_cell_{}", config.regions.len() + 1),
                    rect: PctRect::EMPTY,
                });
            }
            config.regions[index].rect = rect;
        }
        (WizardScene::Fingerprint | WizardScene::Recapture, TargetKind::Rect) => match spec.label {
            "角色名" => config.fingerprint.name_region = rect,
            "数字区" => config.fingerprint.number_region = rect,
            _ => {
                if let Some(index) = candidate_index(spec.label) {
                    config.fingerprint.candidate_boxes = padded_boxes(config.fingerprint.candidate_boxes.clone(), index + 1);
                    config.fingerprint.candidate_boxes[index] = rect;
                }
            }
        },
        (WizardScene::Console, TargetKind::Rect) => {
            if spec.label == "目标序列" {
                config.console.led_region = rect;
            } else {
                config.console.panel_region = rect;
            }
        }
        (WizardScene::Console, TargetKind::Column(index)) => {
            config.console.col_x = padded_axes(config.console.col_x.clone(), index + 1, 0.0);
            config.console.col_x[index] = rect.x + rect.w / 2.0;
        }
        (WizardScene::Console, TargetKind::Row(index)) => {
            config.console.row_y = padded_axes(config.console.row_y.clone(), index + 1, 0.0);
            config.console.row_y[index] = rect.y + rect.h / 2.0;
        }
        (WizardScene::Console, TargetKind::Point) => {
            config.console.knob_center = PctPoint {
                x: rect.x + rect.w / 2.0,
                y: rect.y + rect.h / 2.0,
            };
        }
        // 摩斯 / 补录模板只走矩形，其余坐标轴语义不该出现。
        (WizardScene::Morse | WizardScene::Fingerprint | WizardScene::Recapture, _) => {}
    }
}

/// 从「候选 7」里取出下标 6。
fn candidate_index(label: &str) -> Option<usize> {
    label
        .strip_prefix("候选 ")?
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
}

/// 把向量补到至少 `len` 个 `PctRect::EMPTY`。
fn padded_boxes(mut boxes: Vec<PctRect>, len: usize) -> Vec<PctRect> {
    while boxes.len() < len {
        boxes.push(PctRect::EMPTY);
    }
    boxes
}

/// 把坐标轴向量补到至少 `len` 个默认值。
fn padded_axes(mut axes: Vec<f32>, len: usize, fill: f32) -> Vec<f32> {
    while axes.len() < len {
        axes.push(fill);
    }
    axes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: f32, y: f32, w: f32, h: f32) -> PctRect {
        PctRect { x, y, w, h }
    }

    #[test]
    fn target_tables_cover_spec_areas() {
        assert_eq!(targets(WizardScene::Morse).len(), 3, "摩斯 3 格");
        assert_eq!(targets(WizardScene::Fingerprint).len(), 11, "人名 + 数字 + 9 格");
        assert_eq!(targets(WizardScene::Console).len(), 11, "LED + 面板 + 4 列 + 4 行 + 旋钮");
        assert_eq!(targets(WizardScene::Recapture).len(), 9, "补录只截 9 格");
    }

    #[test]
    fn five_steps_in_order_and_back_keeps_drawn() {
        let mut wizard = Wizard::new();
        assert_eq!(wizard.step, Step::PickScene);
        assert!(!wizard.can_advance(), "未选场景不能前进");
        assert!(!wizard.back(), "第一步不能再回退");

        wizard.choose(WizardScene::Console);
        assert_eq!(wizard.step, Step::SelectRegion);
        assert_eq!(wizard.drawn.len(), 11);
        assert!(!wizard.can_advance(), "一格未框不能前进");

        wizard.record_drawn();
        assert_eq!(wizard.cursor, 1, "松手后自动挪到下一个未框区域");
        assert!(wizard.can_advance());
        // 试跑读到内容才能保存，先把结论摆好再走步序。
        wizard.verdict = Verdict::Read("01-00-01-11".to_owned());

        let mut steps = vec![wizard.step.index()];
        while wizard.advance() && wizard.step != Step::Save {
            steps.push(wizard.step.index());
        }
        steps.push(wizard.step.index());
        assert_eq!(steps, vec![2, 3, 4, 5], "必须按 2→3→4→5 走");

        assert!(wizard.back(), "Esc 可回退");
        assert_eq!(wizard.step, Step::TestRun);
        assert_eq!(wizard.drawn_count(), 1, "回退保留已框区域");
    }

    #[test]
    fn test_run_requires_a_read_result() {
        let mut wizard = Wizard::new();
        wizard.choose(WizardScene::Morse);
        wizard.record_drawn();
        wizard.advance();
        wizard.advance();
        assert_eq!(wizard.step, Step::TestRun);
        assert!(!wizard.can_advance(), "试跑没读到内容不能保存");
        wizard.verdict = Verdict::Read("01-00-01-11".to_owned());
        assert!(wizard.can_advance());
        wizard.advance();
        assert_eq!(wizard.step, Step::Save);
        wizard.advance();
        assert!(wizard.is_finished());
    }

    #[test]
    fn recapture_skips_test_run_and_needs_a_role_name() {
        let mut wizard = Wizard::new();
        wizard.choose(WizardScene::Recapture);
        assert_eq!(targets(WizardScene::Recapture).len(), 9, "只剩九格");
        wizard.record_drawn();
        assert!(wizard.advance(), "框选 → 复看");
        assert_eq!(wizard.step, Step::Review);
        assert!(wizard.advance(), "补录跳过试跑");
        assert_eq!(wizard.step, Step::Save);
        wizard.role_name = "埃德温".to_owned();
        assert_eq!(wizard.role_name, "埃德温");
    }

    #[test]
    fn applying_drawn_writes_percentage_columns_and_rows() {
        let mut config = Config::default();
        let column = CONSOLE_TARGETS[2];
        apply_drawn(&mut config, WizardScene::Console, column, rect(0.4, 0.2, 0.01, 0.1));
        assert_eq!(config.console.col_x.len(), 4, "四条列线固定 4 槽位");
        assert!((config.console.col_x[0] - 0.405).abs() < 1e-4);

        let row = CONSOLE_TARGETS[8];
        apply_drawn(&mut config, WizardScene::Console, row, rect(0.1, 0.5, 0.1, 0.02));
        assert_eq!(config.console.row_y.len(), 4, "四条行线固定 4 槽位");
        assert!((config.console.row_y[2] - 0.51).abs() < 1e-4);

        let knob = CONSOLE_TARGETS[10];
        apply_drawn(&mut config, WizardScene::Console, knob, rect(0.5, 0.5, 0.02, 0.02));
        assert!((config.console.knob_center.x - 0.51).abs() < 1e-4);
    }

    #[test]
    fn applying_drawn_fills_morse_regions_and_candidates() {
        let mut config = Config::default();
        apply_drawn(&mut config, WizardScene::Morse, MORSE_TARGETS[2], rect(0.1, 0.1, 0.05, 0.05));
        assert_eq!(config.regions.len(), 3);
        assert_eq!(config.regions[2].name, "morse_cell_3");
        assert!(!config.regions[2].rect.is_empty());

        let candidate = FINGERPRINT_TARGETS[7];
        apply_drawn(&mut config, WizardScene::Fingerprint, candidate, rect(0.2, 0.3, 0.04, 0.04));
        assert_eq!(config.fingerprint.candidate_boxes.len(), 9);
        assert!(!config.fingerprint.candidate_boxes[5].is_empty());
    }

    #[test]
    fn recapture_scene_maps_candidate_labels_to_global_indices() {
        assert_eq!(candidate_index("候选 1"), Some(0));
        assert_eq!(candidate_index("候选 9"), Some(8));
        assert_eq!(candidate_index("角色名"), None);
    }
}
