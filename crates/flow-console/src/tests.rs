//! 操作台闭环的端到端单元测试。
//!
//! 全部用 [`crate::tests_support`] 的假屏幕 / 假指针 / 假时钟：**不碰真实屏幕与光标**，
//! 也不真实等待（虚拟时钟只在 `sleep_ms` 与抓帧时前进）。

use crate::tests_support::{Harness, World};
use crate::{AbortReason, ConsoleOutcome, ConsoleParams, Pointer};

fn expect_done(outcome: ConsoleOutcome) -> (usize, u64) {
    match outcome {
        ConsoleOutcome::Done { columns, events } => (columns, events),
        other => panic!("期望 Done，实际 {other:?}"),
    }
}

fn expect_abort(outcome: ConsoleOutcome) -> AbortReason {
    match outcome {
        ConsoleOutcome::Aborted { reason, .. } => reason,
        other => panic!("期望 Aborted，实际 {other:?}"),
    }
}

#[test]
fn normal_path_aligns_four_columns_and_stops_on_panel_disappearance() {
    let mut harness = Harness::new(World::new());
    let (columns, events) = expect_done(harness.run());

    assert_eq!(columns, 4, "四列都应被确认接受");
    assert_eq!(harness.world.borrow().accepted, 4);
    assert!(!harness.world.borrow().panel_visible, "最后一列后整块面板应关闭");
    assert_eq!(harness.pointer.clicks, 0, "正常通路不该需要取焦");

    // 完成后零新增输入：返回时的事件数就是最终值，再读一次必须不变。
    assert_eq!(events, harness.pointer.event_count());
    assert_eq!(events, harness.pointer.event_count());

    // 步长必须被夹在配置区间内（自举灵敏度的硬约束）。
    let (smallest, largest) = harness.pointer.step_band();
    assert!(
        smallest >= harness.params.min_step_px && largest <= harness.params.max_step_px,
        "步长越界：{smallest}..{largest}"
    );
}

#[test]
fn panel_vanishing_midway_yields_done_with_zero_further_input() {
    let mut world = World::new();
    world.close_after = 1;
    let mut harness = Harness::new(world);

    let (columns, events) = expect_done(harness.run());
    assert_eq!(columns, 1, "只确认了第一列");
    assert!(!harness.world.borrow().panel_visible);

    // 面板消失后再抓帧不会产生任何输入事件。
    assert_eq!(harness.pointer.event_count(), events);
    assert_eq!(harness.pointer.event_count(), events);
}

#[test]
fn unresponsive_drag_takes_exactly_one_focus_retry_then_aborts() {
    let mut world = World::new();
    world.responsive = false;
    let mut harness = Harness::new(world);

    assert_eq!(expect_abort(harness.run()), AbortReason::DragNoEffect);
    assert_eq!(harness.pointer.clicks, 1, "取焦只允许做一次");
    assert_eq!(harness.world.borrow().accepted, 0);
}

#[test]
fn global_timeout_aborts_without_unbounded_looping() {
    let mut harness = Harness::new(World::new());
    harness.screen.per_grab_ms = 50;
    harness.params.global_timeout_ms = 200;

    assert_eq!(expect_abort(harness.run()), AbortReason::GlobalTimeout);
}

#[test]
fn led_zero_value_boundaries_abort_before_any_drag() {
    for broken in ["", "01-00", "01-00-01"] {
        let mut harness = Harness::new(World::with_led(broken));
        assert_eq!(
            expect_abort(harness.run()),
            AbortReason::ParseFailed,
            "读数 {broken:?} 必须 ParseFailed"
        );
        assert_eq!(harness.pointer.event_count(), 0, "解析失败不得发出任何输入");
        assert_eq!(harness.pointer.begins, 0, "解析失败不得按下鼠标");
    }
}

#[test]
fn reversed_vertical_mapping_is_bootstrapped_from_one_probe() {
    let mut world = World::new();
    world.direction = -1; // 鼠标下移反而让旋钮上移
    let mut harness = Harness::new(world);

    let (columns, _) = expect_done(harness.run());
    assert_eq!(columns, 4, "反向映射必须被自举纠正后仍能收敛");
}

#[test]
fn nonlinear_gain_converges_inside_the_step_budget() {
    let mut world = World::new();
    world.gain = 0.5;
    world.nonlinear = true; // 增益随位移变化，步长反算必然失准 → 靠夹取与重观测兜住
    let mut harness = Harness::new(world);

    let (columns, _) = expect_done(harness.run());
    assert_eq!(columns, 4);
    let (smallest, largest) = harness.pointer.step_band();
    assert!(
        smallest >= harness.params.min_step_px && largest <= harness.params.max_step_px,
        "步长必须夹在 [{}, {}] 内，实际 {smallest}..{largest}",
        harness.params.min_step_px,
        harness.params.max_step_px
    );
    assert!(
        harness.pointer.steps <= u64::from(harness.params.max_steps) * 4,
        "每列步数上限 60，四列不应超过 240 步，实际 {}",
        harness.pointer.steps
    );
}

#[test]
fn horizontal_drag_bootstraps_direction_from_the_column_index() {
    let mut world = World::new();
    world.knob_x = 460.0; // 旋钮一开始在最右列，第 1 列目标在最左列
    world.knob_y = 240.0;
    let mut harness = Harness::new(world);

    let (columns, _) = expect_done(harness.run());
    assert_eq!(columns, 4, "横拖切列 + 自动跳列组合下仍应通关");
    assert!(
        harness.pointer.begins > 4,
        "横拖阶段必须真的按下过鼠标，实际 {}",
        harness.pointer.begins
    );
}

#[test]
fn horizontal_drag_that_never_switches_falls_back_to_polling_and_gives_up() {
    let mut world = World::new();
    world.knob_x = 260.0; // 旋钮在第 2 列，而第 1 列目标在第 1 列
    world.horizontal_supported = false; // 横拖完全无效
    let mut harness = Harness::new(world);

    assert_eq!(expect_abort(harness.run()), AbortReason::DragNoEffect);
    assert_eq!(harness.pointer.begins, 1, "判定不支持横拖后不得再拖");
    assert_eq!(
        harness.pointer.steps,
        u64::from(harness.params.horizontal_attempts),
        "只允许分 {} 次小步试探列索引",
        harness.params.horizontal_attempts
    );
}

#[test]
fn exceeding_the_per_column_step_budget_aborts() {
    let mut world = World::new();
    world.gain = 0.1; // 每 20px 鼠标只换来 2px 面板位移 → 必然超步数上限
    let mut harness = Harness::new(world);

    assert_eq!(expect_abort(harness.run()), AbortReason::StepOverflow);
}

#[test]
fn a_column_that_is_never_accepted_ends_in_dwell_timeout() {
    let mut world = World::new();
    world.accept_grabs = u32::MAX; // 游戏永远不确认
    let mut harness = Harness::new(world);

    assert_eq!(expect_abort(harness.run()), AbortReason::DwellTimeout);
}

#[test]
fn aborted_flag_never_retries_itself() {
    // 「Aborted 禁止自动重试」：一次 run 只推进到第一个失败点就返回，事件数封顶。
    let mut world = World::new();
    world.responsive = false;
    let mut harness = Harness::new(world);
    let outcome = harness.run();
    let events = outcome.events();
    assert!(outcome.abort_reason().is_some());
    assert_eq!(harness.pointer.event_count(), events);
    assert!(
        events < 10,
        "失败路径不该狂发输入，实际 {events} 个事件"
    );
}

#[test]
fn uncalibrated_view_returns_error_instead_of_capturing() {
    let mut harness = Harness::with_params(World::new(), ConsoleParams::default());
    harness.view.col_x.truncate(2);
    let error = crate::run(
        &mut harness.screen,
        &mut harness.pointer,
        &mut harness.clock,
        &harness.view,
        &harness.params,
    )
    .expect_err("列坐标不足必须拒绝启动");
    assert!(matches!(error, crate::ConsoleError::ColumnCount { found: 2, .. }));
    assert_eq!(harness.pointer.event_count(), 0);
    assert_eq!(harness.world.borrow().panel_grabs, 0, "拒绝启动时一帧都不该抓");
}
