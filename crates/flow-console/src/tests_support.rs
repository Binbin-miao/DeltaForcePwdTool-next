//! 测试专属的假实现：假屏幕（脚本化像素）、假指针（记账 + 驱动假世界）、假时钟（虚拟时间）。
//!
//! 全部用 `Rc<RefCell<..>>` 共享同一个「假游戏世界」，因此状态机的一次 `step` 会立刻
//! 反映到下一次 `grab` 的帧里 —— 这就是闭环行为的可复现替身。
//!
//! **测试全程不碰真实屏幕与光标**：这里没有任何 `delta_capture` / `delta_input` 的实机类型。

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Instant;

use delta_capture::Frame;
use delta_config::PxConsole;
use delta_geom::{PxPoint, PxRect};

use crate::params::{parse_led_sequence, ConsoleParams, TargetSequence};
use crate::{Clock, Pointer, Screen};

/// 面板在假屏幕上的位置（与 [`view`] 一致）。
pub const PANEL: PxRect = PxRect {
    x: 100,
    y: 100,
    w: 400,
    h: 500,
};
/// LED 区在假屏幕上的位置。
pub const LED: PxRect = PxRect {
    x: 100,
    y: 20,
    w: 380,
    h: 40,
};

pub const CYAN: [u8; 3] = [55, 95, 120];
pub const KNOB: [u8; 3] = [230, 200, 120];
pub const YELLOW: [u8; 3] = [250, 220, 60];
pub const DARK: [u8; 3] = [20, 20, 20];
pub const INK: [u8; 3] = [255, 255, 255];

/// 校准后的操作台像素视图（4 列 × 4 行，全部落在 [`PANEL`] 内）。
pub fn view() -> PxConsole {
    PxConsole {
        led_region: LED,
        panel_region: PANEL,
        col_x: vec![160, 260, 360, 460],
        row_y: vec![240, 320, 400, 480],
        knob_center: PxPoint::ZERO,
    }
}

/// BGRA 画布（假屏幕的画笔）。
pub struct Canvas {
    pub w: u32,
    pub h: u32,
    data: Vec<u8>,
}

impl Canvas {
    pub fn filled(w: u32, h: u32, rgb: [u8; 3]) -> Self {
        let mut data = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for _ in 0..(w as usize) * (h as usize) {
            data.extend_from_slice(&[rgb[2], rgb[1], rgb[0], 255]);
        }
        Self { w, h, data }
    }

    pub fn set(&mut self, x: i32, y: i32, rgb: [u8; 3]) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 {
            return;
        }
        let index = ((y as usize) * (self.w as usize) + x as usize) * 4;
        self.data[index] = rgb[2];
        self.data[index + 1] = rgb[1];
        self.data[index + 2] = rgb[0];
        self.data[index + 3] = 255;
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, rgb: [u8; 3]) {
        for dy in 0..h {
            for dx in 0..w {
                self.set(x + dx, y + dy, rgb);
            }
        }
    }

    /// 45° 黄黑斜纹（spec §7.3 的浮层形态：亮黄边框 + 内部斜条纹）。
    pub fn diagonal_overlay(&mut self, x: i32, y: i32, w: i32, h: i32) {
        for dy in 0..h {
            for dx in 0..w {
                let band = ((x + dx) - (y + dy)).div_euclid(8).rem_euclid(2);
                let rgb = if band == 0 { YELLOW } else { DARK };
                self.set(x + dx, y + dy, rgb);
            }
        }
    }

    pub fn into_frame(self) -> Frame {
        Frame::new(self.w, self.h, self.data, Instant::now()).expect("画布尺寸与缓冲自洽")
    }
}

/// 点阵字符（与 `delta-vision` 内嵌模板同源，便于走真实读取路径）。
fn glyph_matrix(character: char) -> Option<[[u8; 3]; 5]> {
    match character {
        '0' => Some([[1, 1, 1], [1, 0, 1], [1, 0, 1], [1, 0, 1], [1, 1, 1]]),
        '1' => Some([[0, 1, 0], [1, 1, 0], [0, 1, 0], [0, 1, 0], [1, 1, 1]]),
        '-' => Some([[0, 0, 0], [0, 0, 0], [1, 1, 1], [0, 0, 0], [0, 0, 0]]),
        _ => None,
    }
}

/// 把 `01-00-01-11` 这样的读数渲染成 LED 帧。
pub fn render_led(text: &str, w: u32, h: u32) -> Frame {
    let mut canvas = Canvas::filled(w, h, DARK);
    let cell = (h / 5).max(1) as i32;
    let advance = cell * 4;
    let mut pen_x = 2;
    for character in text.chars() {
        if let Some(matrix) = glyph_matrix(character) {
            for (row, line) in matrix.iter().enumerate() {
                for (column, &ink) in line.iter().enumerate() {
                    if ink == 1 {
                        canvas.rect(
                            pen_x + column as i32 * cell,
                            row as i32 * cell,
                            cell,
                            cell,
                            INK,
                        );
                    }
                }
            }
        }
        pen_x += advance;
    }
    canvas.into_frame()
}

/// 假游戏世界的状态（一次闭环的全部可变量）。
pub struct World {
    pub led: String,
    /// 目标行（由 `led` 解析而来；解析失败时全 0，此时流程走不到接受阶段）。
    pub target_rows: [usize; 4],
    pub panel_visible: bool,
    /// 旋钮中心（绝对屏幕坐标，连续值）。
    pub knob_x: f32,
    pub knob_y: f32,
    /// 灵敏度：面板像素 / 鼠标像素。
    pub gain: f32,
    /// 灵敏度是否随位移非线性变化（用于自举收敛测试）。
    pub nonlinear: bool,
    /// 方向：`+1` 表示鼠标下移使旋钮下移，`-1` 表示反向映射。
    pub direction: i32,
    /// 横轴映射符号：`+1` 表示 `+dx` 让列索引增大。
    pub horizontal_dir: i32,
    /// 横拖是否能切列（游戏旋钮是「一格一格」的棘轮）。
    pub horizontal_supported: bool,
    /// 纵拖时横移的串扰（spec D8：两轴都会影响纵向线）。
    pub cross_coupling: f32,
    /// 拖动是否生效（`false` = 游戏完全不理，用于取焦重试测试）。
    pub responsive: bool,
    /// 接受一列所需的最少「已对齐」抓取次数。
    pub accept_grabs: u32,
    /// 已对齐的连续抓取次数。
    pub settled: u32,
    /// 对齐容差（略大于状态机容差，模拟游戏吸附）。
    pub accept_tolerance: f32,
    /// 接受后是否自动跳到下一列（spec §2.3 的「自动跳列」）。
    pub auto_advance: bool,
    /// 累计接受列数后关闭面板（4 = 正常通关；1 = 中途关闭）。
    pub close_after: usize,
    pub accepted: usize,
    pub col_x: Vec<i32>,
    pub row_y: Vec<i32>,
    pub panel_grabs: u64,
}

impl World {
    /// 正常通路：LED 读数取 spec §7.1 的实测样例。
    pub fn new() -> Self {
        Self::with_led("01-00-01-11")
    }

    pub fn with_led(led: &str) -> Self {
        let target_rows = parse_led_sequence(led)
            .map(|sequence: TargetSequence| sequence.rows())
            .unwrap_or([0; 4]);
        let col_x: Vec<i32> = view().col_x;
        let row_y: Vec<i32> = view().row_y;
        Self {
            led: led.to_owned(),
            target_rows,
            panel_visible: true,
            knob_x: col_x[0] as f32,
            knob_y: row_y[0] as f32,
            gain: 2.0,
            nonlinear: false,
            direction: 1,
            horizontal_dir: 1,
            horizontal_supported: true,
            cross_coupling: 0.0,
            responsive: true,
            accept_grabs: 2,
            settled: 0,
            accept_tolerance: 8.0,
            auto_advance: true,
            close_after: 4,
            accepted: 0,
            col_x,
            row_y,
            panel_grabs: 0,
        }
    }

    pub fn column(&self) -> usize {
        nearest(&self.col_x, self.knob_x)
    }

    pub fn gain_for(&self, mouse_px: i32) -> f32 {
        if self.nonlinear {
            self.gain * (1.0 + mouse_px.abs() as f32 / 40.0)
        } else {
            self.gain
        }
    }

    /// 每次抓面板帧时推进世界：对齐足够多次 → 该列被接受；接受够多 → 面板关闭。
    /// 返回本次帧是否应显示斜纹浮层。
    fn on_grab(&mut self) -> bool {
        self.panel_grabs = self.panel_grabs.saturating_add(1);
        if !self.panel_visible {
            return false;
        }
        let column = self.column();
        let target_y = self.row_y[self.target_rows[column]] as f32;
        let aligned = (self.knob_y - target_y).abs() <= self.accept_tolerance;
        if !aligned {
            self.settled = 0;
            return false;
        }
        self.settled = self.settled.saturating_add(1);
        if self.settled < self.accept_grabs || self.accepted > column {
            return false;
        }
        self.settled = 0;
        self.accepted = column + 1;
        if self.accepted >= self.close_after {
            self.panel_visible = false;
        } else if self.auto_advance {
            self.knob_x = self.col_x[self.accepted.min(self.col_x.len() - 1)] as f32;
        }
        true
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

fn nearest(values: &[i32], target: f32) -> usize {
    let mut best = 0usize;
    let mut best_distance = f32::INFINITY;
    for (index, value) in values.iter().enumerate() {
        let distance = (*value as f32 - target).abs();
        if distance < best_distance {
            best = index;
            best_distance = distance;
        }
    }
    best
}

/// 假屏幕：按脚本渲染面板与 LED。
pub struct FakeScreen {
    pub world: Rc<RefCell<World>>,
    pub time: Rc<Cell<u64>>,
    pub per_grab_ms: u64,
}

impl Screen for FakeScreen {
    fn grab(&mut self, rect: PxRect) -> Result<Frame, String> {
        self.time
            .set(self.time.get().saturating_add(self.per_grab_ms));
        if rect == PANEL {
            let (show_overlay, visible, knob_x, knob_y) = {
                let mut world = self.world.borrow_mut();
                let show_overlay = world.on_grab();
                (
                    show_overlay,
                    world.panel_visible,
                    world.knob_x,
                    world.knob_y,
                )
            };
            let mut canvas = if visible || show_overlay {
                Canvas::filled(rect.w as u32, rect.h as u32, CYAN)
            } else {
                Canvas::filled(rect.w as u32, rect.h as u32, DARK)
            };
            if visible || show_overlay {
                let local_x = knob_x.round() as i32 - rect.x - 10;
                let local_y = knob_y.round() as i32 - rect.y - 10;
                canvas.rect(local_x, local_y, 20, 20, KNOB);
            }
            if show_overlay {
                // 浮层放在面板顶部，与 4 行旋钮位置都不重叠（浮层位置会移动，spec §7.3）。
                canvas.diagonal_overlay(60, 20, 200, 60);
            }
            return Ok(canvas.into_frame());
        }
        if rect == LED {
            let led = self.world.borrow().led.clone();
            return Ok(render_led(&led, rect.w as u32, rect.h as u32));
        }
        Err(format!("假屏幕不认这个区域：{rect:?}"))
    }
}

/// 假指针：记账 + 把位移写回假世界。
pub struct FakePointer {
    pub world: Rc<RefCell<World>>,
    pub events: u64,
    pub steps: u64,
    pub begins: u64,
    pub clicks: u64,
    pub max_abs_step: i32,
    pub min_abs_step: i32,
    pressed: bool,
}

impl FakePointer {
    pub fn new(world: Rc<RefCell<World>>) -> Self {
        Self {
            world,
            events: 0,
            steps: 0,
            begins: 0,
            clicks: 0,
            max_abs_step: 0,
            min_abs_step: i32::MAX,
            pressed: false,
        }
    }

    /// 全部非零步长的绝对值区间（用于断言「夹在 `[min_step, max_step]`」）。
    pub fn step_band(&self) -> (i32, i32) {
        (self.min_abs_step, self.max_abs_step)
    }
}

impl Pointer for FakePointer {
    fn begin(&mut self, _at: PxPoint) -> Result<(), String> {
        if self.pressed {
            return Err("重复按下".to_owned());
        }
        self.events = self.events.saturating_add(1);
        self.begins = self.begins.saturating_add(1);
        self.pressed = true;
        Ok(())
    }

    fn step(&mut self, dx: i32, dy: i32) -> Result<(), String> {
        if !self.pressed {
            return Err("未按下就 step".to_owned());
        }
        self.events = self.events.saturating_add(1);
        if dx != 0 || dy != 0 {
            self.steps = self.steps.saturating_add(1);
            let magnitude = dx.abs().max(dy.abs());
            self.max_abs_step = self.max_abs_step.max(magnitude);
            self.min_abs_step = self.min_abs_step.min(magnitude);
        }
        let mut world = self.world.borrow_mut();
        if world.responsive {
            let gain = world.gain_for(dy);
            let direction = world.direction as f32;
            world.knob_y += (dy as f32 + dx as f32 * world.cross_coupling) * gain * direction;
            // 旋钮是棘轮：一次横拖 = 跳一格列（与实机「一格一格」的手感一致）。
            if dx != 0 && world.horizontal_supported {
                let column = world.column() as i32;
                let stepped = column + dx.signum() * world.horizontal_dir;
                let bounded = stepped.clamp(0, world.col_x.len() as i32 - 1) as usize;
                world.knob_x = world.col_x[bounded] as f32;
            }
        }
        Ok(())
    }

    fn end(&mut self) -> Result<(), String> {
        if !self.pressed {
            return Err("未按下就 end".to_owned());
        }
        self.pressed = false;
        self.events = self.events.saturating_add(1);
        Ok(())
    }

    fn click(&mut self, _at: PxPoint) -> Result<(), String> {
        self.events = self.events.saturating_add(2);
        self.clicks = self.clicks.saturating_add(1);
        Ok(())
    }

    fn event_count(&self) -> u64 {
        self.events
    }
}

/// 假时钟：时间只在 `sleep_ms` 与抓帧时前进，测试零真实等待。
pub struct FakeClock {
    pub time: Rc<Cell<u64>>,
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.time.get()
    }

    fn sleep_ms(&mut self, ms: u64) {
        self.time.set(self.time.get().saturating_add(ms));
    }
}

/// 一次闭环的完整测试替身。
pub struct Harness {
    pub screen: FakeScreen,
    pub pointer: FakePointer,
    pub clock: FakeClock,
    pub params: ConsoleParams,
    pub view: PxConsole,
    pub world: Rc<RefCell<World>>,
}

impl Harness {
    pub fn new(world: World) -> Self {
        Self::with_params(world, ConsoleParams::default())
    }

    pub fn with_params(world: World, params: ConsoleParams) -> Self {
        let view = view();
        let world = Rc::new(RefCell::new(world));
        let time = Rc::new(Cell::new(0u64));
        Self {
            screen: FakeScreen {
                world: Rc::clone(&world),
                time: Rc::clone(&time),
                per_grab_ms: 1,
            },
            pointer: FakePointer::new(Rc::clone(&world)),
            clock: FakeClock { time },
            params,
            view,
            world,
        }
    }

    pub fn run(&mut self) -> crate::ConsoleOutcome {
        crate::run(
            &mut self.screen,
            &mut self.pointer,
            &mut self.clock,
            &self.view,
            &self.params,
        )
        .expect("视图已校准")
    }
}
