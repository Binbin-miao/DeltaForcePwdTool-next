# Spec v4.0 — 三角洲密码助手 Rust 重构

> 目标：用 Rust + egui 重写上游 Python 工具，体积 ≤12MB、冷启动 <200ms、零外部运行时依赖，
> 剔除微信 OCR，新增「一键场景自动识别」与「操作台旋钮小游戏自动代操作」，
> 并把硬编码 2K 坐标改为可视化校准的百分比坐标。

- 上游参照：`_upstream/`（1068 行 Python，微信 OCR 依赖）
- 规格冻结时间：2026-11-09
- 状态：**FROZEN**

---

## 1. 已拍板决策与显式边界不变量

| 编号 | 决策 | 显式边界 / 零值条件 |
|---|---|---|
| D1 | 技术栈 Rust + eframe(egui) 0.37，渲染后端 **glow**（禁用 wgpu） | 单 exe ≤12MB；冷启动 <200ms；无 .NET/VC 运行时依赖（MSVC 静态 CRT） |
| D2 | 剔除微信 OCR，改**双引擎认角色** | 主：全模板投票（离线、5ms）；兜底：`Windows.Media.Ocr` zh-Hans-CN（本地 `%WINDIR%\OCR\zh-cn` 已确认存在）。**引擎失败不得静默**，必须回落到另一引擎或明确报错 |
| D3 | 模式 N **不再读数字 OCR** | 依据：角色模板张数 == 该角色模式 N（上游 images/ 实测：埃德温4/克莱尔6/卢卡斯6/巴斯特6/德穆兰6/无名6/罗米克斯6/雅各布6/格赫罗斯8）。**若同时读到数字且与 N 冲突，以 N 为准并记录 warn**；读不到数字属正常路径，不得报错 |
| D4 | 格子-模板匹配用**二分图最大权匹配**（匈牙利），不得各自独立 argmax | 存活格数 = N+1（上游 settings.json:41-43 的 9/7/5 即 indices 长度），模板数 = N。**边界：N=0 或无模板 → 中止并留证；不得静默 return** |
| D5 | 点击序 = 模板编号升序，非格子位置序 | N 次格点点击；确认点击仅在 `confirm_clicks 非空` 时触发（空数组 → 跳过，不报错） |
| D6 | 新增单热键「一键场景识别」 | 默认 F8，可改键。判别 `Morse\|Fingerprint\|Console\|Unknown`；**Unknown 必须拒绝执行**（不得兜底乱点），并在 UI 明示「未识别到游戏界面」 |
| D7 | 新增「操作台旋钮」流程 | 4 列 × 4 行，行标签自上而下 `10/11/01/00`，右上 LED 目标序列 `NN-NN-NN-NN`（仅字符 0/1）。**完成判据（主）= 面板青色结构特征消失**（整块 UI 关闭，已由视频第 25s 帧实证）；**辅助 = 局部黄黑斜纹浮层出现**（含义是「该列已被接受」，由第 21s 帧实证，位置/大小会移动，不可当全局完成用）；**兜底 = 全局超时**。任一完成信号命中后立即停手，不得再发任何输入 |
| D8 | 旋钮拖动 = **相对位移**，不分轴，两轴都驱动线纵向移动 | 采用 `SendInput MOUSEEVENTF_MOVE` 分步增量（每步 ≤20px + 帧间 2ms），**禁用绝对坐标瞬移**（游戏 raw input 收不到）。灵敏度、方向、列推进方式**全部运行时自举**，不写死 |
| D9 | 坐标存**百分比**（相对主显示器），配可视化校准向导 | 百分比换算用主显示器物理分辨率；Per-Monitor V2 DPI 感知在任何 GetDC 之前完成。**校准缺失的必填区域 → 禁止启动对应流程**，UI 灰置并提示 |
| D10 | 保留上游全部行为不变量（见 §2.2）并修正 3 个上游缺陷 | 缺陷：① `[0]` 落入 4 模式静默降级 → 改为中止；② 模式不在表内静默 return → 改为留证+报错；③ 失败分支部分无日志 → 统一「失败必留证」 |
| D11 | 不做：16 宫格撬锁、多显示器、独占全屏专用捕获、账号/云同步 | 16 宫格仅留 `trait Puzzle` 插件位与置灰入口 |
| D12 | 输入注入必然带 `LLMHF_INJECTED` 标记，无法伪装真实输入 | ~~首次启动弹一次免责声明，写入配置 `accepted_disclaimer:true` 后不再提示~~ → **已废弃（2026-11-09 用户决策）**：免责声明整套移除，启动直接进主界面，`accepted_disclaimer` 字段一并删除（旧配置中的该 key 被 serde 静默忽略） |

---

## 2. 调用链与状态机变更矩阵

### 2.1 上游 → 新版映射

| 上游 File:Line | 模块/函数 | 变更前逻辑 | 变更后契约 | 锁/原子性 |
|---|---|---|---|---|
| `main.py:82-90` | 热键注册 | `keyboard.add_hotkey` × 3，需管理员 | `RegisterHotKey` + 专用线程 `GetMessageW`，单键 + `MOD_NOREPEAT`，免管理员 | 句柄 RAII 释放 |
| `main.py:77-79` | OCR 初始化 | 缺 `wcocr.pyd` 直接崩 | OCR 惰性初始化，失败仅降级不崩 | `OnceLock` |
| `core/capture.py:29` | 截图 | 每次新建 `ImageGrab` | GDI 常驻 DC/位图复用，`BitBlt`+`GetDIBits`（32bpp top-down） | 单线程独占 DC |
| `core/morse.py:24-55` | 摩斯解码 | Otsu→findContours→`aspect>2.0`→按 x 排序→查表；三类异常统一压成 `"?"` | 同算法；异常拆为 `Empty/Unmapped/Error` 三态 | 纯函数 |
| `core/fingerprint.py:121-197` | 指纹主流程 | 人名 OCR→数字 OCR→`range(1,N+1)`→逐格独立 argmax→同号去重取高分 | 投票认角色→N 由模板数得出→二分图最大权匹配→按模板号升序点击 | 纯函数 + 输入串行 |
| `core/fingerprint.py:86-96` | 模板匹配 | `cv2.matchTemplate TM_CCOEFF_NORMED`，模板 resize 到候选尺寸后取中心 70% | 手写 NCC（积分图 O(1) 窗口统计 + 模板预减均值），**保留「非等比 resize + 中心 70% 核心」语义**（跨分辨率可用的根本，不得"优化"成等比） | 纯函数，rayon 并行 |
| `utils/config.py:44-123` | 坐标缩放 | **原地改写** cfg，重复 load 会二次缩放污染 | 百分比存储，加载时换算为只读像素视图；`save` 只写百分比，**禁止回写像素值** | 原子写 temp→fsync→rename |
| `utils/mouse.py:56-71` | 点击 | `SetCursorPos` + `mouse_event` | `SendInput` 批量投递 down+up；拖动用分步相对 MOVE | 单线程串行 |
| `core/selector.py` | 预览 | 存 png 后 `os.startfile` | 改为内置叠加预览（egui 上直接画框），可选另存 | 只读 |

### 2.2 必须保留的行为不变量（逐条已在代码核对）

1. 模式区间：`max∈[1,4]→4`、`[5,6]→6`、`[7,9]→8`（新版 N 由模板数给出，此区间仅作交叉校验）。
2. 同模板号只能落一格（去重），冲突取高分 —— 升级为最大权匹配后天然满足。
3. 点击序按模板编号升序（`fp:223` / README:134），**不是**格子位置序。
4. `confirm_clicks == []` → 跳过确认点击；`auto_click == false` → 只存标注图不点击。
5. 摩斯：任一格非数字 → **仍敲出已识别的数字键**，仅跳过确认点击（非对称副作用，`morse.py:87-107`）。
6. 时序常量默认值：摩斯输入后 0.6s、指纹点击后 1.7s、键间隔 0.05s、点击内含 0.05s —— 全部转配置但默认一致。
7. 噪点门 `w*h < 10`；点划阈 `aspect > 2.0`（严格大于，`=2.0` 判点）。
8. 找窗标题白名单 `"三角洲" / "Delta Force" / "游戏"` + 强制置前（点击是全局光标事件，必须）。
9. 存活格数 = N+1，取 `candidate_boxes` 行主序前 N+1 个。
10. **勿迁移的死配置**：`match_threshold`（真阈值硬编码 0.45）、`target_centers`、`big_fp_region`、`mode_config.candidates`、README 里的 `p`/`r` 热键（上游未实现）。

### 2.3 操作台旋钮状态机

```
Idle
 └─(热键/场景判别命中 Console)→ Detect        捕获 → 定位面板/行标签/LED/旋钮
      ├─ 未定位 → Unknown(提示校准，不动作)
      └─ 成功 → Parse                        读 LED 4 组 + 黄块(列,行)
           ├─ 解析失败重试 3 帧 → Aborted
           └─ 成功 → Plan                   列→目标行 映射 + 校验已完成列
                └→ ColumnAlign              黄块不在目标列 → 横向拖动（结果重检测）
                     ├─ 列未变 → 判定「自动跳列」模式，等 200ms 重检测
                     └─ 列已变 → RowAlign
                └→ RowAlign                 纵向小步拖动，每步后重检测黄块 Y
                     ├─ |ΔY| ≤ 容差 → Dwell
                     └─ 超过 max_steps → Aborted
                └→ Dwell                    停住 dwell_ms（默认 900ms，可配置）
                     └→ Verify              重捕获：面板是否消失 / 斜纹是否出现 / 列是否推进
                          ├─ 面板消失 → Done（立即停手，不发任何后续输入）
                          ├─ 斜纹浮层出现 → 该列已被接受 → 回 ColumnAlign（下一列）
                          ├─ 列推进 → 回 ColumnAlign（下一列）
                          └─ 无变化 → retry（单列 ≤3 次）→ 否则 Aborted
Aborted：**禁止自动重试**，仅提示；需人再按键重入（防失控连点）
超时：Parse 3s / 单列 12s / 全局 90s
```

**运行时自举**（对应「部分得进游戏实测」的 12 项未知）：

| 未知项 | 自举方式 |
|---|---|
| 拖动灵敏度（鼠标 px → 行的 Y px） | 首次拖动先走 20px 试探，量出 `Δy_panel/Δy_mouse` 存本次会话 |
| 拖动方向（上滑=上一行 or 下一行） | 试探步后看黄块 Y 变化方向，反向则取负号 |
| 横向拖动是否切列 | 横拖一次后重检测列索引；不变则切「等自动跳列」策略 |
| 列与目标序列的左右对应 | 首列成功锁定后，比对落在序列的哪一端，锁定映射方向 |
| 停留确认时长 | 默认 900ms，Verify 未见推进则递增 300ms 重试（上限 2400ms） |
| 是否需先点面板取焦 | 首次拖动无反应 → 先点面板中央再拖（只做一次） |

---

## 3. 曳光弹工单与独占写锁

| Ticket | 目标 | 独占写锁 | Blocked By | 验收命令 |
|---|---|---|---|---|
| t1 | workspace 骨架 + `delta-geom`（百分比↔像素）+ `delta-config`（原子写、校准结构） | `Cargo.toml`, `crates/geom/`, `crates/config/` | — | `cargo test -p delta-geom -p delta-config` |
| t2 | `delta-capture`：GDI 常驻 DC、`BitBlt`+`GetDIBits`、黑帧检测降级 | `crates/capture/` | t1 | `cargo test -p delta-capture` |
| t3 | `delta-vision`：手写 NCC（积分图）、Otsu、轮廓 boundingRect、点阵读数字 | `crates/vision/` | t1 | `cargo test -p delta-vision` + `cargo bench` |
| t4 | `delta-templates`：角色库加载、投票认角色、匈牙利最大权匹配 | `crates/templates/` | t1,t3 | `cargo test -p delta-templates`（用 `_upstream/images` 真实 54 张） |
| t5 | `delta-input` + `delta-hotkey`：SendInput 分步相对拖动、RegisterHotKey 线程 | `crates/input/`, `crates/hotkey/` | t1 | `cargo test -p delta-input -p delta-hotkey` |
| t6 | `delta-ocr`：`Windows.Media.Ocr` MTA 线程封装 + 不可用降级 | `crates/ocr/` | t1 | `cargo test -p delta-ocr` |
| t7 | `delta-scene` + `flow-morse` + `flow-fingerprint` | `crates/scene/`, `crates/flow-morse/`, `crates/flow-fingerprint/` | t2,t3,t4,t5,t6 | `cargo test -p delta-scene -p delta-flow-morse -p delta-flow-fingerprint` |
| t8 | `flow-console`：旋钮闭环状态机（§2.3 全量）+ 自举标定 | `crates/flow-console/` | t2,t3,t5 | `cargo test -p delta-flow-console` |
| t9 | `delta-ui` + `delta-app`：主界面五态、校准向导、单实例、打包体积门禁 | `crates/ui/`, `crates/app/`, `assets/` | t7,t8 | `cargo build --release` + 体积断言 ≤12MB |

---

## 4. 验收门禁清单

- [ ] `cargo build --release` 通过，产物 ≤12MB，双击冷启动 <200ms（秒表实测）
- [ ] 全量 `cargo test` 通过；`cargo clippy -- -D warnings` 无告警
- [ ] 指纹离线复现：用 `_upstream/images` 真实模板 + 一份真实游戏截图，投票能正确认出角色，N 与模板张数一致，匹配结果为 N 个不同模板号且无重复
- [ ] 摩斯：给定 3 张点划截图，解码结果与人工判读一致；`(aspect==2.0)` 判为点
- [ ] 场景判别：三个界面的截图分别判为 Morse/Fingerprint/Console；**桌面截图判为 Unknown 且不产生任何输入**
- [ ] 操作台：目标序列读取正确率 100%（4 组 × 2 字符）；黄黑斜纹命中后**零后续输入事件**（用输入事件计数器断言）
- [ ] 配置：连续 `load()` 100 次坐标值不漂移（复现上游二次缩放污染并证明已修复）
- [ ] 校准缺失任一项 → 对应流程按钮灰置且不响应热键
- [ ] 单实例：重复启动第二个进程自动退出并激活已有窗口

---

## 5. Non-Goals

- 16 宫格图案撬锁（仅保留 `trait Puzzle` 插件位 + UI 置灰入口）
- 微信 OCR / wcocr.pyd 任何形式依赖
- 多显示器、HDR、超宽屏特殊适配（v1 仅主显示器）
- 内存读写、游戏文件修改、任何反检测/伪装技术
- 网络功能、账号、云端配置同步

---

## 6. 风险登记

| 风险 | 影响 | 缓解 |
|---|---|---|
| 游戏 raw input 忽略 SendInput 注入 | 拖动无效 | 分步相对移动 + 必要时 `SetCursorPos` 兜底；无效则明示失败 |
| 独占全屏导致 GDI 黑帧 | 截图全黑 | 黑帧检测 → 提示切无边框窗口；不强行上 WGC（体积/复杂度代价） |
| 54 张模板为上游来源，可能与现版本游戏素材不符 | 匹配率低 | 校准向导支持「一键补录模板」：截取 9 格并自动命名入库 |
| 12 项旋钮未知项与实测不符 | 流程卡住 | 全部运行时自举（§2.3），并有 Aborted 明确退出路径 |
| msyh.ttc 是 TTC，epaint 只取 face 0 | 中文豆腐块 | 改内嵌 `simhei.ttf` 单 face 子集或运行时校验首帧渲染 |
| **已发生**：`lto="fat"`+`codegen-units=1` 在本机链接阶段死锁（39 分钟零进展、无 exe 产出） | 出包不可行 | **已改为 `lto="thin"` + `codegen-units=16`**；若体积超 12MB，优先降 `opt-level` 至 `"s"`，不回到 fat LTO |
| egui 依赖 426 个包，首次全量编译耗时长 | 迭代慢 | 开发期一律 `cargo check` / `cargo build`（dev），仅在出包时跑一次 release；热路径 crate 已单独 O3 |

---

## 7. 视频实测标定数据（主理人已从 `D:/Downloads/e98cb38795d601cfbff356c5cdf60938.mp4` 提取）

> 视频规格 1280×720 / 30fps / 68s。以下坐标仅为「相对比例」参考，最终以用户校准结果为准。

### 7.1 LED 目标序列字符判别（实测 11/11 全对）

Otsu 二值化 → 列投影切字符（列内亮像素 >2 视为字符列）→ 按包围盒判别：

| 字符 | 包围盒宽 | 包围盒高 | 宽高比 | 判别规则 |
|---|---|---|---|---|
| `0` | 21~22 px | 32 px | 0.66~0.69 | `h ≥ 0.6H` 且 `w/h > 0.55` → `0` |
| `1` | 12~13 px | 32 px | 0.33~0.41 | `h ≥ 0.6H` 且 `w/h ≤ 0.55` → `1` |
| `-` | 13 px | 7~8 px | 1.62 | `h < 0.6H` → `-` |

- 真实字形已导出到 `delta-pwd-tool/assets/led/{led_0,led_1,led_dash}.png`（32×32 灰度，内容包围盒缩放后居中），供 NCC 二次校验。
- **必须双判据**：宽高比先分类（快、鲁棒），NCC 得分 <0.6 时抛出 `LedAmbiguous` 而非硬猜。
- 样例读数：`01-00-01-11`（4 组 × 2 位，整局不变）。

### 7.2 面板几何（1280×720 帧内实测，占比换算后作为校准默认值）

| 元素 | 帧内像素 | 占屏比例 |
|---|---|---|
| 面板外框 | x 85~680，y 90~605 | x 6.6%~53.1%，y 12.5%~84.0% |
| 行标签 4 框 | x 118~160，y 依次 250/330/415/500 附近 | 左侧贴边 |
| LED 区 | x 855~1245，y 85~140 | x 66.8%~97.3%，y 11.8%~19.4% |
| 旋钮中心（红色圆钮） | 约 (745, 340) | 约 (58%, 47%) |

### 7.3 斜纹浮层（阶段确认信号）

- 形态：**亮黄色 2~3px 边框 + 内部黄黑相间 45° 斜条纹**的矩形浮层，尺寸约 250×70（帧内），**位置会移动**（第 21s 位于面板中部偏左）。
- 用途：**表示「该列已被接受」**，不是全局完成。
- 检测：先按黄色边框（HSV H∈[20,35], S≥120, V≥180）找矩形候选，再在候选内验证 45° 条纹方向性（沿 45° 与 135° 的差分均值比 >1.15 且行剖面 std >0.3）。

### 7.4 完成态（主判据）

第 25s 帧：**整个解谜面板消失**，画面回到第一人称场景。因此 `Verify` 必须采「面板青色结构特征是否还在」：

- 面板特征 = 区域内「青绿色调（H∈[75,105], S∈[40,160], V∈[40,120]）」像素占比；正常态实测约 25%~40%，面板关闭后趋近 0。
- **判定阈值：占比 < 8% 即视为面板消失 → `Done`**（留 3 倍安全边界）。

### 7.5 仍必须实机自举的项

拖动灵敏度、拖动方向、横拖是否切列、列与序列的左右对应、停留确认时长、是否需要先点面板取焦 —— 见 §2.3 自举表，全部运行时测定，**禁止写死本节任何像素常数**。
