# Spec 截图后端可切换 + 截图配额 — 默认 WGC，禁止实时识别

## 0. 事实基线（实测与源码核实）

| 事实 | 证据 |
|---|---|
| 上游用 `PIL.ImageGrab.grab(bbox=)`，Windows 底层即 GDI `BitBlt` | `_upstream/core/capture.py:29`；本机实测 ImageGrab 与直接 BitBlt 抓同区域，逐通道均值完全一致（仅 RGB/BGR 顺序不同） |
| 现实现 `crates/capture/src/gdi.rs` 为常驻屏幕 DC + 常驻 DIB 的 GDI | `gdi.rs:39-45` 启动时 `GetDC` 一次并长期持有，`lib.rs:5` 注释自述「常驻复用」 |
| `windows` crate 0.58 原生支持 Windows Graphics Capture | 已核实存在 `Direct3D11CaptureFramePool`、`GraphicsCaptureSession`、`Direct3D11CaptureFrame`、`Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop` |
| 操作台每拖动一小步就截图 | `flow-console/src/machine.rs:499`（行对齐循环内 `observe()`）、`:547`（横拖循环内 `observe()`），`max_steps=60` → 最坏 60 张/列 |
| 现有实际用量（非操作台） | 摩斯 3 格 ×(1+重试) ≈ 3~5；指纹整屏 1 张 |
| BetterGI 后端与频率（外部参照） | 5 种后端、默认 BitBlt、设置页 ComboBox、20Hz 定时器 + 前台 gating；我们**只抄「可切换」与「测试图像捕获按钮」，不抄定时器** |

---

## 1. 已拍板决策与显式边界不变量

- **[D1] 截图后端做成用户可选，共两档，默认 WGC。**
  - `WindowsGraphicsCapture`（默认）：能截无边框窗口与绝大多数 D3D 画面。
  - `BitBlt`（兼容模式）：保留，但**默认不启用**；UI 上必须标注「兼容模式 · 可能截不到独占全屏」。
  - 显式边界：不实现 `DwmGetDxSharedSurface`（未公开导出，灰色地带）。
  - 显式边界：切换后端立即生效，不需要重启程序；但**必须等当前识别任务结束**才允许切换（运行中下拉框置灰）。
  - 显式边界：选择写入配置 `capture_backend`，未知值读入时回落 `Wgc` 并 `eprintln` 记录。

- **[D2] 禁止常驻 GDI 屏幕 DC。**（用户原话：「禁止常驻」）
  - `BitBlt` 后端改为**每次 `grab` 申请 `GetDC`、用完整即 `ReleaseDC`**，内存 DC 与位图同样即时创建、即时销毁。
  - 显式边界：`gdi.rs` 中不得存在跨 `grab` 调用存活的 `HDC` / `HBITMAP` / 自建结构体字段。用测试断言：连续两次 `grab` 前后 `GetGuiResources(GR_GDIOBJECTS)` 不增长。

- **[D3] 采集通道按任务建、结束即关（用户拍板）。**
  - 一次识别任务开始（按 F8 / 主按钮）时创建 `CaptureSession`，任务结束（成功/失败/取消/超时）时 `Drop` 释放 D3D11 设备、帧池、采集会话。
  - 显式边界：**程序空闲时不得持有任何采集对象**。用测试断言：`CaptureSession::drop` 后内部句柄计数为 0。
  - 显式边界：「测试截图」按钮单独创建一个临时会话，抓一帧后立即释放，不复用任务会话。

- **[D4] 绝对禁止实时识别与高频截图（用户原话：「禁止实时识别画面与截取过多图片，这是绝对禁止的」）。**
  - 不提供任何 `start_stream` / `on_frame` / 回调式 / 后台线程定时截图接口。`capture` crate 对外只暴露**同步单次** `grab(rect)`。
  - **单次识别任务截图硬上限 = 20 张**。`CaptureSession` 内部计数，第 21 次 `grab` 直接返回 `CaptureError::BudgetExceeded`。
  - 显式边界：配额在任务开始时重置，任务结束清零；不打日志刷屏（只在超限时报一次）。
  - 显式边界：**不新增任何 `System::Timers` / `setInterval` / `loop { sleep; grab }` 式的定时截图**。审查时全仓搜 `Thread::sleep` 必须能逐处解释为「流程等待」而非「轮询截图」。
  - 量化目标：操作台典型路径（4 列 × 4 行、一次到位）截图 ≤ 16 张。

- **[D5] 做「测试截图」按钮（用户拍板）。**
  - 位置：设置面板，截图方式下拉框下方。
  - 行为：点击 → 用**当前选中后端**抓一帧整屏（或当前区域）→ 在设置面板内显示缩略图 + `宽×高 · 耗时ms`。
  - 显式边界：只在用户点击时发生；不点不抓；不缓存、不循环刷新。
  - 显式边界：抓取失败时显示具体错误（如 `WGC 初始化失败: 0x80070005`），不回退到另一后端。

- **[D6] 设置面板结构参照 BetterGI（结构抄、视觉不抄）。**
  - 每行 = 左侧主标题 + 下方灰色小字说明 + 右侧控件，行间 1px 分隔线。
  - 视觉仍是项目既有的深色工装 token（`theme::*`），**不引入 Fluent 浅色卡片、不引入圆角大卡片阴影**。
  - 显式边界：设置面板必须仍在 400×420 窗口内完整显示，不得溢出、不得出现滚动条。

- **[D7] 操作台控制策略改为「估算到位 → 截一张验证 → 误差大才补」。**
  - `drive_row_pressed`：不再每步 `observe()`。用已学到的 `gain`（垂直灵敏度）一次性算出一组子步完成拖动（子步注入之间**不截图**），拖完截一张验证；误差超容差再补一轮，最多 3 轮。
  - `drag_to_column_pressed`：同上，切列改为「一次横拖到位 → 截一张确认」。
  - `poll_auto_advance`：轮询间隔由 200ms 提到 500ms，次数由 3 降到 2（最多 2 张）。
  - 显式边界：自举（首次学 gain / 方向）阶段允许保留每步观察，因为那是必须的试探；自举完成后必须走开环估算。

---

## 2. 全调用链与状态机变更矩阵

| File:Line | 模块/函数 | 变更前 | 变更后契约 | 锁/原子性 |
|---|---|---|---|---|
| `crates/capture/src/lib.rs:1-254` | `Capturer` / `grab` | 单一 GDI 实现、常驻 DC | `CaptureBackend` 枚举 + `CaptureSession::new(backend, monitor)`；`grab(rect) -> Result<Frame, CaptureError>`；会话内计数 | `&mut self`，单线程独占 |
| `crates/capture/src/gdi.rs:37-160` | `GdiCapturer` | 常驻 `HDC`/`HBITMAP` | 每次 `grab` 新建、`Drop`/作用域结束即释放；无跨调用字段 | 无共享状态 |
| `crates/capture/src/wgc.rs`（新增） | WGC 后端 | — | D3D11 设备 + `Direct3D11CaptureFramePool` + `GraphicsCaptureSession`；`grab` 取一帧转 BGRA | 同上 |
| `crates/config/src/lib.rs` | `Config` | 无截图字段 | 新增 `capture_backend: CaptureBackend`（serde 默认 `Wgc`） | 配置锁 |
| `crates/app/src/worker.rs` | `run_once` | 直接用 `delta_capture::Capturer` | 任务开始建 `CaptureSession`，结束 `drop`；超限转 `UiEvent::Failed` | 单执行线程 |
| `crates/ui/src/view.rs:900-1000` | `settings_panel` | 热键 / 文字兜底 / 配置路径 | 加「截图方式」行（下拉）+「测试截图」按钮 + 缩略图区；行式布局 | 无 |
| `crates/ui/src/state.rs` | `UiState` | 无截图字段 | `capture_backend: CaptureBackend`、`capture_test: Option<CaptureTestResult>` | 无 |
| `crates/app/src/app.rs` | 设置面板分支 | — | `SetCaptureBackend` / `TestCapture` 两个 action；测试结果回填 state | 配置锁 |
| `crates/flow-console/src/machine.rs:478-560` | `drive_row_pressed` / `drag_to_column_pressed` | 每子步 `observe()` | 开环估算 + 每轮一图 + 最多 3 轮 | 无 |
| `crates/flow-console/src/machine.rs:573-585` | `poll_auto_advance` | 200ms × 3 | 500ms × 2 | 无 |

### WGC 实现链路（技术路线，供实现者按此走）
```
D3D11CreateDevice(HARDWARE, BGRA_SUPPORT) → ID3D11Device
  → QueryInterface<IDXGIDevice>
  → CreateDirect3D11DeviceFromDXGIDevice → IDirect3DDevice (WinRT)
  → Direct3D11CaptureFramePool::CreateFreeThreaded(device, B8G8R8A8UIntNormalized, 2, item.Size)
  → IGraphicsCaptureItemInterop::CreateForMonitor(hmonitor) → GraphicsCaptureItem
  → pool.CreateCaptureSession(item) → session
  → session.SetIsCursorCaptureEnabled(false)   // 必须：否则光标被画进帧里污染识别
  → session.StartCapture()
grab(rect):
  → pool.TryGetNextFrame() → Direct3D11CaptureFrame
  → frame.Surface() → IDirect3D11Texture2D
  → 建 staging texture (CPU_READ) → CopyResource → Map → 逐行拷 rect 区域
  → Unmap → 返回 Frame{ data, w, h, stride }
```
必需 `windows` features（全部加在已有依赖上，**不新增第三方 crate**）：
`Graphics_Capture`、`Graphics_DirectX_Direct3D11`、`Win32_System_WinRT_Graphics_Capture`、`Win32_Graphics_Direct3D11`、`Win32_Graphics_Dxgi`、`Win32_Graphics_Dxgi_Common`、`Win32_Graphics_Gdi`、`Foundation`、`Win32_Graphics_Dwm`。

**已知风险（必须处理）**：本机显卡列表只有虚拟显示器驱动（OrayIddDriver / GameViewer / MuMu），没有真实 GPU。WGC 可能回落 WARP 或初始化失败。因此 `CaptureSession::new` 失败必须返回可读错误，UI 上「测试截图」要能显示出来；**不得静默回退 BitBlt**（D5 边界）。

---

## 3. 曳光弹工单与独占写锁

| Ticket | 目标 | 独占文件写锁 | 依赖 | 验收命令 |
|---|---|---|---|---|
| t11-a | `CaptureBackend` 枚举 + `CaptureSession` 配额计数 + `CaptureError::BudgetExceeded`；`gdi.rs` 去常驻化 | `crates/capture/**`、`crates/config/src/lib.rs` | 无 | `cargo test -p delta-capture -p delta-config` |
| t11-b | WGC 后端实现（`wgc.rs`），含 D3D11 链路与逐行拷贝 | `crates/capture/**`、`crates/capture/Cargo.toml` | t11-a | `cargo test -p delta-capture` + 真机 `测试截图` |
| t11-c | 操作台控制策略改开环 + 轮询降频（把截图压进配额） | `crates/flow-console/src/machine.rs`、`params.rs` | t11-a | `cargo test -p delta-flow-console`（28 条必须仍全绿） |
| t11-d | 设置面板截图方式行 + 测试截图按钮 + 缩略图；app 接线 | `crates/ui/src/view.rs`、`crates/ui/src/state.rs`、`crates/app/src/app.rs`、`crates/app/src/worker.rs` | t11-b、t11-c | `cargo test -p delta-ui -p delta-app` |

> **串行铁律**：t11-a → t11-b → t11-c → t11-d。t11-d 与热键改造共用 `view.rs`/`state.rs`/`app.rs`/`worker.rs`，必须在热键改造**已提交**之后才能开工。

---

## 4. 验收门禁清单

**配额与禁止项（最重要，逐条可测）**
- [ ] `CaptureSession` 第 21 次 `grab` 返回 `CaptureError::BudgetExceeded`，且不再真正抓屏
- [ ] 任务结束后配额归零，下一次任务重新有 20 张
- [ ] `capture` crate 公开 API 里**不存在** `start`/`stop`/`subscribe`/`on_frame` 这类流式接口
- [ ] 全仓不存在定时器驱动的截图循环（人工审查 + grep `Thread::sleep` 逐处解释）
- [ ] 操作台典型路径（4 列 × 4 行）截图 ≤ 16 张（用 `FakeScreen` 的调用计数器断言）
- [ ] 操作台最坏路径触发配额时返回明确错误，不是死循环

**GDI 去常驻**
- [ ] `gdi.rs` 无跨 `grab` 存活的 `HDC` / `HBITMAP` 字段
- [ ] 连续 50 次 `grab` 后 GDI 对象数不增长（`GetGuiResources`）

**后端与配置**
- [ ] 配置里 `capture_backend` 缺省为 WGC；未知字符串读入回落 WGC 并有 eprintln
- [ ] 切换后端立即生效（下一个 `grab` 用新后端），运行中下拉框置灰
- [ ] WGC 初始化失败时返回可读错误，**不回退 BitBlt**

**测试截图按钮**
- [ ] 点击后抓一帧，显示 `宽×高 · 耗时ms`
- [ ] 失败时显示具体错误码
- [ ] 不点击时零截图（用计数器断言设置面板打开期间 `grab` 调用为 0）

**UI**
- [ ] 设置面板在 400×420 内完整显示，无溢出、无滚动条
- [ ] 行式布局与既有 token 一致，不出现浅色卡片
- [ ] 缩略图出现后不挤压底部按钮

**整体**
- [ ] `cargo test --workspace -- --test-threads=1` 全绿（基线 248 + 本轮新增）
- [ ] `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets -- -D warnings` 零告警
- [ ] 真机：设置里切到 WGC → 测试截图能出图 → 按 F8 跑一次摩斯 → 成功
- [ ] 真机：切到 BitBlt → 测试截图能出图（验证兼容档仍可用）

---

## 5. 明确不做（Non-Goals）

- 不做 `DwmGetDxSharedSurface`（未公开导出）。
- 不做 WGC HDR 变体（需要色调映射着色器，收益与成本不匹配）。
- 不做 WGC V2 限流（FFmpeg gfxcapture 式，需 26100+ 且我们本来就按需单次）。
- 不做自动选择后端 / 自动降级（用户明确要求「自己选」，且自动降级会掩盖问题）。
- 不做实时预览 / 帧流 / 录制。
- 不改 `vision`、`templates`、`flow-morse`、`flow-fingerprint` 的内部算法（只改它们在 app 层的截图接线）。
