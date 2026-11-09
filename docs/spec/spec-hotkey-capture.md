# Spec 热键捕获改造 — 把「自由文本输入」换成「按键捕获」

## 0. 缺陷事实（已定位，非推测）

用户真机报告：设置面板「全局热键」可以打出 `/`、`sb250` 这类任意文字，且**看起来保存成功了但热键不生效**。

三层叠加根因：

| 层 | 文件:行 | 现状 | 后果 |
|---|---|---|---|
| 交互 | `crates/ui/src/view.rs:926-948` | 编辑态用 `egui::TextEdit::singleline` | 可输入任意字符，包括非法键名 |
| 提交 | `crates/app/src/app.rs:476-488` | `CommitHotkey` 只判 `!name.is_empty()` 就写配置 | 非法键名直接落盘 |
| 解析 | `crates/app/src/platform.rs:162-179` | `parse_hotkey` 无法识别 `sb250` 时 `Hotkey::default()`（= F8） | 界面显示 `sb250`，实际注册 F8 → 假保存 |
| 注册 | `crates/app/src/worker.rs:121-135` | 改键注册失败 `if let Ok(id) = ...` 静默丢弃 | 被占用时用户零感知 |

---

## 1. 已拍板决策与显式边界不变量

- **[D1] 捕获支持「单键 + 组合键」**。可用键：`F1~F24`、`A~Z`、`0~9`、`Space`、`Insert`、`Delete`、`Home`、`End`、`PageUp`、`PageDown`、方向键。修饰键：`Ctrl` / `Alt` / `Shift`（不引入 `Win` 键，避免与系统热键冲突不可控）。
  - 显式边界：**纯修饰键本身不构成热键**（只能按 Ctrl 不能提交，必须继续等主键）。
  - 显式边界：大小写不敏感，内部统一大写；`Ctrl+F8` 与 `CTRL+f8` 等价。
  - 显式边界：键名序列化固定修饰键顺序 `Ctrl+Alt+Shift+主键`（保证同一组合只有一种字符串表示，避免配置里出现两个等价键）。

- **[D2] 热键被占用时必须提示并保留旧键**。
  - 显式边界：注册新键失败 → **重新注册旧键**（回滚），界面显示「热键冲突 · 仍为 F8」（红 `ERR`，2.5 秒后自动消失，也可点掉）。
  - 显式边界：旧键回滚也失败（连接口都占不到）→ 提示「热键不可用 · 用按钮」且不阻断主按钮。
  - 显式边界：**不允许静默失败**。任何一次改键都必须有可见结果（成功则显示新键，失败则显示冲突）。

- **[D3] 删除自由文本输入，只保留捕获**。
  - 显式边界：`SettingsAction::CommitHotkey` 的语义从「提交输入框文本」改为「提交捕获到的键」，**不再接受任意字符串**。
  - 显式边界：`parse_hotkey` 从「失败静默回落 F8」改为「返回 `Result`」，回落只允许发生在**启动读配置**这一处（且必须 `eprintln` 记录）。
  - 显式边界：`hotkey_buffer: &mut String` 字段删除（不再需要缓冲文本）。

- **[D4] 捕获态下 `Esc` 只取消捕获，不关闭设置面板**。
  - 显式边界：`settings_panel` 开头现有的 `key_pressed(Escape)` 关闭逻辑，必须先判断 `hotkey_editing`；捕获态下 Esc 只把 `hotkey_editing` 置 false 并 `return None`。
  - 显式边界：捕获态下点击热键行外部区域也取消（不提交）。

- **[D5] 沿用既有热更新链路**：改键成功立即生效，无需重启（`Worker::set_hotkey`）。其余三个键（F5/F6/F7）不在本次范围，仍由配置文件 + 重启生效。

---

## 2. 全调用链与状态机变更矩阵

| File:Line | 模块/函数 | 变更前逻辑 | 变更后契约 | 锁/原子性要求 |
|---|---|---|---|---|
| `crates/ui/src/view.rs:926-948` | `settings_panel` 热键编辑态 | `ui.put(TextEdit)` 自由输入 + 回车提交 | 画「请按下新热键…」提示 + 从 `ui.input` 事件流捕获；Esc 取消 | 无 |
| `crates/ui/src/view.rs:889-891` | `SettingsAction::EditHotkey / CommitHotkey` | 提交文本 | `CommitHotkey` 携带 `KeyChord`（不再从 buffer 读） | 无 |
| `crates/ui/src/view.rs:884-893` | `SettingsView` 字段 | 含 `hotkey_buffer: &'a mut String` | 删该字段；新增 `hotkey_conflict: Option<&'a str>` 供提示渲染 | 无 |
| `crates/ui/src/state.rs` | `UiState` | `hotkey_editing: bool`、`hotkey_buffer: String` | 删 `hotkey_buffer`；新增 `hotkey_conflict: Option<String>` 与 `hotkey_conflict_until: f64`（自动消失计时） | 无 |
| `crates/app/src/platform.rs:162-179` | `parse_hotkey` | `-> Hotkey`，失败回落 default | `-> Result<Hotkey, HotkeyParseError>`；支持 `Ctrl+Alt+Shift+PREFIX` | 纯函数 |
| `crates/app/src/platform.rs` 新增 | `format_hotkey(&Hotkey)` / `key_name(egui::Key)` | 无 | 捕获结果 ↔ 配置字符串双向转换，单一来源 | 纯函数 |
| `crates/app/src/app.rs:472-488` | `EditHotkey` / `CommitHotkey` 分支 | 写 buffer 文本 | `CommitHotkey(chord)` → `format_hotkey` → 写配置 → `set_hotkey`；先不改 `state.hotkey`，等 worker 回执再改 | 配置锁 `Mutex<Config>`（已有） |
| `crates/app/src/worker.rs:22-41` | `UiEvent` | 无热键相关变体 | 新增 `HotkeyApplied(String)` 与 `HotkeyConflict { wanted: String, kept: String }` | mpsc |
| `crates/app/src/worker.rs:121-135` | 热键线程改键分支 | 注销旧键 → 注册新键（失败静默） | 记住 `current_name`；注册失败则重注册 `current_name` 并发 `HotkeyConflict`；成功发 `HotkeyApplied` | 通道内单线程，天然串行 |
| `crates/app/src/worker.rs:110-120` | 启动注册循环 | `parse_hotkey(name)` 直接返回 | `parse_hotkey` 失败 → `eprintln` + 用 F8 兜底 | 无 |
| `crates/app/src/main.rs` | 启动读配置 | 无 | 同上（若键在配置层解析，`warn` 不阻断启动） | 无 |

---

## 3. 曳光弹工单与独占写锁

| Ticket | 目标 | 独占文件写锁 | 依赖 | 验收测试命令 |
|---|---|---|---|---|
| t10-a | 键名映射表 + `parse_hotkey` 改 `Result` + `format_hotkey` 往返 | `crates/app/src/platform.rs` | 无 | `cargo test -p delta-app parse_`、`cargo test -p delta-app hotkey_` |
| t10-b | 捕获交互：事件流读键、Esc 取消、纯修饰键忽略、删 TextEdit | `crates/ui/src/view.rs`、`crates/ui/src/state.rs` | t10-a | `cargo test -p delta-ui` |
| t10-c | 热键线程冲突回执 + 回滚旧键 + UI 冲突提示渲染 | `crates/app/src/worker.rs`、`crates/app/src/app.rs`、`crates/ui/src/view.rs` | t10-a、t10-b | `cargo test -p delta-app`、`cargo test -p delta-ui` |

> 独占写锁说明：t10-b 与 t10-c 都碰 `view.rs`，必须**串行**（t10-b 完成后 t10-c 再动）。

---

## 4. 验收门禁清单

- [ ] `parse_hotkey("F8")` → `Ok(vk=0x77, modifiers=0)`
- [ ] `parse_hotkey("f24")` → `Ok(vk=0x87, modifiers=0)`（小写等价）
- [ ] `parse_hotkey("Ctrl+F8")` → `Ok(vk=0x77, modifiers=MOD_CONTROL)`
- [ ] `parse_hotkey("Ctrl+Alt+Shift+A")` → `Ok(vk=0x41, modifiers=0x1|0x2|0x4)`
- [ ] `parse_hotkey("Shift+Ctrl+A")` 与 `parse_hotkey("Ctrl+Shift+A")` 等价（顺序归一）
- [ ] **`parse_hotkey("sb250")` → `Err`**（不再回落 F8）
- [ ] **`parse_hotkey("F99")` → `Err`**
- [ ] `parse_hotkey("")` → `Err`
- [ ] 往返不变量：任意合法 `Hotkey` → `format_hotkey` → `parse_hotkey` 得到同一个 `(vk, modifiers)`
- [ ] 捕获态按 `Esc`：`hotkey_editing == false` 且设置面板**仍然打开**
- [ ] 捕获态只按 `Ctrl`（纯修饰键）：不提交，`hotkey_editing` 仍为 true
- [ ] 捕获态按 `F9`：提交 `F9`，`hotkey_editing == false`
- [ ] 新键注册失败（用已占用的 F8 做对照）：`state.hotkey_conflict.is_some()`，且 `state.hotkey` **仍是旧值**
- [ ] 冲突提示 2.5 秒后自动消失
- [ ] `cargo test --workspace -- --test-threads=1` 全绿（基线 248）
- [ ] `CARGO_INCREMENTAL=0 cargo clippy --workspace --all-targets -- -D warnings` 零告警
- [ ] 真机：设置面板点热键行 → 显示「请按下热键」→ 按 F9 → 显示 F9 → 关掉设置 → 按 F9 能触发识别

---

## 5. 明确不做（Non-Goals）

- 不支持 `Win` 键组合（系统级热键冲突不可控，收益低）。
- 不支持鼠标侧键、多媒体键（本轮只解决键盘热键）。
- 不改动 F5/F6/F7 三个专用键的绑定流程（仍在配置文件里改）。
- 不引入新的第三方依赖（egui 已能提供键事件与 Key 枚举）。

---

## 6. 复审批次追加（2026-11-09 第二轮，必须修完才算闭环）

第一轮复审 `55da112` 判 PASS，但提出两条会残留原症状的 S2，追加为本轮必做项：

- **[S2-1] 旧配置仍显示假值（必须修）**：`app.rs:547` 的 `initial_state` 把 `config.hotkeys.scene` **原文**塞给界面，而 `worker.rs:240` 启动时把不可解析的名字回落成 F8。老配置若留着 `"sb250"`，面板继续显示 `sb250` 而实际是 F8 —— 原缺陷对该用户原样保留。
  - 修法：`initial_state` 用**启动解析结果经 `format_hotkey` 规范化后**的字符串显示；解析失败时显示 `F8` 并保留 eprintln 记录。仍属 D3「唯一允许兜底在启动读配置」这一处。

- **[S2-2] 改键可能完全无声（必须修）**：`app.rs:161-163` 先 `persist()` 再 `worker.set_hotkey(name)` 且丢弃 send 结果。若热键线程已因 `Hotkeys::new()` 失败而退出（`worker.rs:120-122` 直接 return），则零回执：界面不换键、无提示，但配置已落盘、下次启动静默生效。违反 D2「不允许静默失败」。
  - 修法：判断 send 结果，失败即 `show_hotkey_conflict(conflict_copy(None), now)`。

- **[S2-3] 两表单向绑定（必须修）**：`platform.rs:431` 的用例只遍历 `CAPTURABLE`，`KEY_TABLE` 多一项不会变红。
  - 修法：补 `assert_eq!(KEY_TABLE.len(), CAPTURABLE.len())` 双向钉死。

- **[精简，同轮做掉]**：
  1. 删 `app.rs:154-157` 的重复解析（结果只判 `is_err()` 就丢，worker 里会再解析一次；且失败时会把主界面切成「未识别」，热键错误不该动主状态机）。
  2. `state.rs:246-259` 的 `KeyChord::label()` 与 `platform.rs:325-340` 的 `format_hotkey` 重复拼接修饰键 → 提交路径改用 `Hotkey::new(vk, chord.modifiers())`，同时去掉 format→parse 往返。
  3. `app.rs:126` 与 `app.rs:161` 各 persist 一次 → 成功路径只落盘一次。
  4. 冲突文案用 `FONT_BODY(13)` 右对齐，长组合键在 340 最小宽度下会压到左侧标题 → 降 `FONT_LABEL(11)`。

> 本轮 S2 修复与工单 t11-d（设置面板截图方式 + 测试截图按钮）**合并为同一轮施工**，因为二者共用 `app.rs` / `view.rs` / `state.rs` / `worker.rs`，分两轮会重复改动同一批文件并重复跑门禁。

> 本轮已完成：S2-1 / S2-2 / S2-3 三条必修 + 四条精简（提交路径改用 `hotkey_from_chord` 直出虚拟键码，去掉 format→parse 往返；成功路径只在 `HotkeyApplied` 回执处落盘一次）。
