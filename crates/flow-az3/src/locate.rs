//! 自适应定位的对外门面。
//!
//! 实现住在 [`delta_scene::locate`]，这里只做转发 —— 依赖方向决定了这一点：
//! `delta-scene` 需要用 `locate_line` 做信号线台的场景判别，而 `flow-az3` 本来就依赖
//! `delta-scene`。把实现写在本 crate 会让 `scene → flow-az3 → scene` 成环；
//! 把判别改回固定坐标则会退回「时而命中、时而全落空」的老问题。
//!
//! 调用方按 `delta_flow_az3::locate::locate_line(frame)` 使用即可。

pub use delta_scene::locate::{locate_line, LocateError, LocatedLine, LINE_COLUMNS, LINE_ROWS};
