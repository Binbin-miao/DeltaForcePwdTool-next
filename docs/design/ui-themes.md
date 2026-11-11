# UI 主题体系

> 范围：`crates/ui` 主窗口（400×420）的配色体系、布局方向、可切换主题的实现约定。
> 本文件只出设计，不含实现代码。所有 hex 均已用 WCAG 2.1 相对亮度公式实测，见 §2.7。

---

## 1 诊断

现有界面不是"配色丑"，是**视觉重量分配失败**，四条硬伤一条比一条致命：

1. **41% 的窗口面积是两个空盒子。** 序列区 72px（`SEQUENCE_H`）+ 进度区实测 100px（`view.rs:184` 的 `primary.bottom+8 → footer.top-8`），合计 172/420。两处都画了 `paint_panel` + 描边 + 内填充，然后什么都不放（空态序列只是一个居中的 `——`，`TEXT_3`）。**空的高对比容器＝内容加载失败的视觉语言**，所以第一眼像 bug，不像待机。
2. **主按钮的视觉重量小于它上方的开关行。** `primary_button` 启用态是 `BG_PANEL` 填充 + 2px accent 描边（`view.rs:434-444`）；而开关行是 `BG_PANEL` 填充 + 1px `LINE_SUBTLE` 描边（`view.rs:377-387`）。同一块底色、同样的满宽、同样的 2px 圆角、字都是 13px —— CTA 与"设置项"在重量上几乎同级，唯一的差别只有描边粗细。**用户找不到该点哪里。**
3. **开关不是开关，是圆形指示灯。** 3.5px 圆点 + 「开/关」二字（`view.rs:396-407`），没有轨道、没有滑块位移、没有行程。三行同模板同高度同圆角堆叠，读起来是"列表"而不是"控件"。
4. **强调色被摊薄到 3.5px 的点和 1px 的描边里。** accent 在整窗的着色面积不足 0.4%，所以琥珀金完全起不到"重心"作用；同时 `FONT_BODY` 13 覆盖了 90% 的文字，15/13/11 的跨度在 400px 窗口里读不出层级。

**结论：这一版的问题不在色相，在于没有"主次"这个维度。** 换色救不了它 —— 所以下面六套方案除了给色，都同时规定了按钮填充、开关形态、结果区高度与字号跳跃。

---

## 2 六套配色

### 2.0 全局派生规则（六套共用，避免散写新魔法数）

只需表内 14 个 hex，其余全部由它们派生，不得新增 hex 常量：

| 派生 token | 规则 |
| :--- | :--- |
| `btn_fill` | `accent` |
| `btn_fill_hover` | `accent` 向 `text_1` 混合 10% |
| `btn_fill_active` | `accent` 向 `text_1` 混合 18% |
| `switch_track_off` | `bg_void` 填充 + 1px `line_strong` 描边 |
| `switch_thumb_off` | `text_3` |
| `switch_track_on` | `accent` 填充，无描边 |
| `switch_thumb_on` | `accent_text_on` |
| `progress_track` | `line_subtle` |
| `progress_fill` | `accent` |
| `focus_ring` | `accent` @ 55% alpha，2px |
| `highlight` | 深色主题 白 6%；浅色主题 黑 5%（顶部 1px 浮起高光） |
| `shadow` | 深色主题 黑 30% 外扩 2px；**浅色主题改为 1px `line_strong` 外框，不用阴影**（egui 无模糊，浅底黑边会脏） |

字体的硬事实：内嵌 simhei 是**单 face**，egui 拿不到字重，也无法合成粗体。**所以"加粗"在这个项目里不存在**，层级只能靠三件事建立：字号跳跃、颜色亮度差、留白。基于此，六套统一采用**一套字号、靠颜色分层**的策略，`font_value` 是唯一真正的字号跳跃点（24–28 mono），主按钮靠**实心填充**而不是字重取胜。

---

### 2.1 石墨 / Graphite（深色，默认推荐给"耐看派"）

**定位**：不想要任何色彩噪音的人。长时间挂着当仪表看，颜色只用来报状态；主按钮是**无彩色**的实心浅块。最像工业设计奖作品的一套。

| token | hex | | token | hex |
| :--- | :--- | :--- | :--- | :--- |
| `bg_void` | `#0A0A0B` | | `text_1` | `#EDEFEF` |
| `bg_panel` | `#121315` | | `text_2` | `#9CA2A7` |
| `bg_raised` | `#1B1D20` | | `text_3` | `#767C81` |
| `bg_inset` | `#0C0D0F` | | `accent` | `#E8EBEA` |
| `line_strong` | `#34383D` | | `accent_text_on` | `#0F1011` |
| `line_subtle` | `#25282C` | | `ok` / `warn` / `err` | `#5FBF8A` / `#E0913F` / `#E0594A` |

`radius_button` 4 · `radius_panel` 8 · `font_title` 15 · `font_body` 13 · `font_label` 11 · `font_value` 26（mono）

**主色可自定义**：可以，但会把"无彩色"气质打破。规则见 §2.7 —— 若用户改色相，本套退化为"中性底＋单点彩色"，反而是最容易驾驭的；保持默认 `#E8EBEA` 时按钮文字恒为深色，永不需二次判断。

**风险**：在光线强的房间、或显示器对比度调低时，整窗会糊成一片灰；无彩色按钮在浅色主题壁纸旁边容易被误认为"禁用"。它靠留白活着，实现时**必须**守住 12/8 的边距和 44px 的按钮高度，压扁一点就垮。

---

### 2.2 夜航 / Nightwatch（深色，最通用的"科技产品感"）

**定位**：白天也在电脑前干活的用户。深蓝黑底比纯黑更"有空气"，蓝色 accent 天然带"正在工作"的信号感。这是最不容易出错、最像成熟商业软件的一套。

| token | hex | | token | hex |
| :--- | :--- | :--- | :--- | :--- |
| `bg_void` | `#070B14` | | `text_1` | `#E6EDF7` |
| `bg_panel` | `#0F1520` | | `text_2` | `#9AABBF` |
| `bg_raised` | `#18202E` | | `text_3` | `#6E8098` |
| `bg_inset` | `#0A0F18` | | `accent` | `#4C8DFF` |
| `line_strong` | `#2C3849` | | `accent_text_on` | `#06101F` |
| `line_subtle` | `#1D2633` | | `ok` / `warn` / `err` | `#3FD68F` / `#F2A93B` / `#FF5C5C` |

`radius_button` 6 · `radius_panel` 10 · `font_title` 15 · `font_body` 13 · `font_label` 11 · `font_value` 26（mono）

**主色可自定义**：完全可换。本套是"深底＋一点彩色"的通用容器，换任何色相都不违和（换到绿/黄时注意与 `ok`/`warn` 撞车，见 §2.7 的去重规则）。

**风险**：小窗口 + 大面积深蓝，在未校准的广色域屏上偏紫，会滑向"AI 蓝";`err` 的 `#FF5C5C` 已经接近霓虹，禁止再给它加发光或 alpha 呼吸以外的特效。

---

### 2.3 暗焰 / Ember（深色，默认推荐给"游戏现场"）

**定位**：贴着《三角洲行动》挂着用的那套。暖黑褐底 + 高饱和橙，是现有琥珀的正确做法：不是"点缀"，而是"整块烧起来"。最贴情境、最不像办公软件。

| token | hex | | token | hex |
| :--- | :--- | :--- | :--- | :--- |
| `bg_void` | `#100C08` | | `text_1` | `#F2E7DA` |
| `bg_panel` | `#191310` | | `text_2` | `#B8A48E` |
| `bg_raised` | `#241B15` | | `text_3` | `#8A7663` |
| `bg_inset` | `#0D0906` | | `accent` | `#FF9E3D` |
| `line_strong` | `#3F3024` | | `accent_text_on` | `#1A0F04` |
| `line_subtle` | `#2C221A` | | `ok` / `warn` / `err` | `#6FBF73` / `#E8A33C` / `#E0553C` |

`radius_button` 3 · `radius_panel` 6 · `font_title` 15 · `font_body` 13 · `font_label` 11 · `font_value` 24（mono）

**主色可自定义**：可换，但**必须留在暖侧（色相 20–70）**才不破气质；换到蓝/紫会变成"发霉的橙底＋蓝按钮"，最丑的组合。

**风险**：暖底 + 暖强调极易糊成一片（`warn #E8A33C` 与 `accent #FF9E3D` 只差 5% 明度）。补救全靠 `line_subtle` 的 1px 分隔和 `text_3` 的亮度差 —— 若实现时删掉分隔线，这套立刻降级成"脏"。另外 `warn` 在本套几乎不可辨，建议本套的警告态**只用文案 + 图标**，不靠黄橙色差。

---

### 2.4 素纸 / Paper（浅色，印刷品气质）

**定位**：桌面是浅色、白天用、不想被深色窗口吸走注意力的人。米纸底 + 墨蓝按钮 = 钢笔写在本子上的感觉，是全六套里"最不像软件"的一套。

| token | hex | | token | hex |
| :--- | :--- | :--- | :--- | :--- |
| `bg_void` | `#DCD5C6` | | `text_1` | `#211E19` |
| `bg_panel` | `#F7F3EA` | | `text_2` | `#57503F` |
| `bg_raised` | `#FFFDF8` | | `text_3` | `#7C7362` |
| `bg_inset` | `#EDE7DA` | | `accent` | `#2C3E6B` |
| `line_strong` | `#C3B8A4` | | `accent_text_on` | `#FBF8F1` |
| `line_subtle` | `#DCD4C5` | | `ok` / `warn` / `err` | `#2F6B45` / `#8A5A00` / `#A32B1B` |

`radius_button` 4 · `radius_panel` 8 · `font_title` 15 · `font_body` 13 · `font_label` 11 · `font_value` 26（mono）

**主色可自定义**：可换，但**必须压到低明度**（浅色主题下 accent 的 L≈0.48，见 §2.7）；用户若选到柠檬黄、亮青这类高明度色相，系统必须自动降明度而不是照抄，否则白字在按钮上直接失效。

**风险**：`bg_panel #F7F3EA` 与 `bg_raised #FFFDF8` 只差 3% 亮度 —— 浅色主题的"浮起"几乎看不出来，卡片层级必须靠 1px `line_strong` 描边撑住。另外暖米底在冷白显示器上会偏黄，若用户不喜欢，切 Frost。

---

### 2.5 霜白 / Frost（浅色，临床仪表气质）

**定位**：要"精准、干净、无情绪"。冷白底 + 近乎黑的按钮，颜色只出现在状态上。最锐利的一套，也最"专业仪器"。

| token | hex | | token | hex |
| :--- | :--- | :--- | :--- | :--- |
| `bg_void` | `#DDE2E8` | | `text_1` | `#161C22` |
| `bg_panel` | `#F4F6F8` | | `text_2` | `#4E5A66` |
| `bg_raised` | `#FDFDFE` | | `text_3` | `#78848F` |
| `bg_inset` | `#E9EDF1` | | `accent` | `#1C2024` |
| `line_strong` | `#C2CAD3` | | `accent_text_on` | `#F7F9FA` |
| `line_subtle` | `#DAE0E6` | | `ok` / `warn` / `err` | `#12705A` / `#8A5A00` / `#B02A2A` |

`radius_button` 5 · `radius_panel` 10 · `font_title` 15 · `font_body` 13 · `font_label` 11 · `font_value` 28（mono）

**主色可自定义**：可换，同理必须压明度。本套换色后气质变化最小（因为黑白骨架已经定死），适合当"自定义主色"的默认承载体。

**风险**：太干净会冷到"医疗 / 杀毒软件"。`text_3 #78848F` 在浅底上只有 3.53:1，仅可用于**非关键**标签（占位符、脚注），一旦用在开关的"关"状态文字上会读不清 —— 本套的关状态请用 `text_2`。

---

### 2.6 瓷青 / Celadon（浅色，安静护眼）

**定位**：长时间盯屏、讨厌纯白刺眼的人。浅灰绿底 + 深翠按钮，单色系（同色相三档），最"安静"的一套，也是最不像默认模板的一套。

| token | hex | | token | hex |
| :--- | :--- | :--- | :--- | :--- |
| `bg_void` | `#D8E0D9` | | `text_1` | `#1B2A22` |
| `bg_panel` | `#EDF1EC` | | `text_2` | `#4A5C50` |
| `bg_raised` | `#F8FAF7` | | `text_3` | `#6F8075` |
| `bg_inset` | `#E1E8E2` | | `accent` | `#1F6B4F` |
| `line_strong` | `#B8C6BA` | | `accent_text_on` | `#F4F8F4` |
| `line_subtle` | `#D2DCD3` | | `ok` / `warn` / `err` | `#227653` / `#8A5A0A` / `#A83322` |

`radius_button` 8 · `radius_panel` 12 · `font_title` 15 · `font_body` 13 · `font_label` 11 · `font_value` 24（mono）

**主色可自定义**：可换，且**同色系微调（色相 120–190）最好看**；跨到红/紫时绿底 + 异色按钮会脏，建议锁在绿-青区间，或允许全色相但强制底 `bg` 跟随 accent 做 4% 色相偏移（进阶，非必需）。

**风险**：深翠按钮的色相接近"成功绿"，用户可能把主按钮读成"确认/已完成"。本套必须让按钮**文案本身**足够明确（「开始识别」而不是「开始」），且 `ok` 状态色在本套只用在状态点与文字，不用于大面积。

---

### 2.7 主色自定义：可执行规则

用户只选**一个色相 `hue ∈ [0,360)`**，其余全部由系统算，不允许用户改明度/饱和（否则必出低对比）：

| 项目 | 规则 | 实测 |
| :--- | :--- | :--- |
| 色彩空间 | **OKLCH**（不用 HSL：HSL 的 L=50% 在黄色上亮度爆表、在蓝色上过暗，无法用一条规则覆盖全色相） | — |
| 深色主题 accent | `oklch(L=0.72, C=0.14, hue)`，越界则 `C *= 0.93` 迭代至 in-gamut | 对 `bg_panel` 对比 **6.99–7.95:1** |
| 深色 accent 文字 | **恒定深色**（近 `#12100E` 族），因为白字在 L=0.72 上只有 **2.3–2.7:1**，永远不达标 | 深字 7.10–8.07:1 |
| 浅色主题 accent | `oklch(L=0.48, C=0.13, hue)` | 对 `bg_panel` 对比 **5.67–6.47:1** |
| 浅色 accent 文字 | **恒定近白** | 白字 **6.15–7.01:1** |
| 兜底 | 若目标底色的对比仍 <4.5，按 `L ± 0.03` 步进（深色主题往上、浅色往下）直到达标，最多 8 步 | — |
| 状态色 | **不跟随 accent**。避免"成功色变紫"；`ok`/`warn`/`err` 恒为表内值 | — |
| 去重 | 当 `ΔE00(accent, ok|warn|err) < 12` 时，状态**必须**配图标（`●`/`!`/`×`）+ 文字，不靠颜色表意 | — |

### 2.8 对比度实测（WCAG 2.1，全部通过）

| 方案 | text_1/panel | text_2/panel | text_3/panel | accent/panel | accent 上的字 | ok | warn | err |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| Graphite | 16.11 | 7.21 | 4.40 | 15.49 | 15.88 | 8.24 | 7.34 | 5.04 |
| Nightwatch | 15.52 | 7.79 | 4.53 | 5.71 | 5.96 | 9.78 | 9.15 | 6.04 |
| Ember | 15.08 | 7.65 | 4.25 | 8.94 | 9.17 | 8.21 | 8.53 | 4.85 |
| Paper | 15.00 | 7.23 | 4.23 | 9.44 | 9.86 | 5.73 | 5.35 | 6.50 |
| Frost | 15.85 | 6.51 | 3.53 | 15.13 | 15.52 | 5.55 | 5.47 | 6.04 |
| Celadon | 13.13 | 6.26 | 3.66 | 5.62 | 5.98 | 4.86 | 5.19 | 5.82 |

门槛：`text_1` ≥7（AAA 正文）、`text_2` ≥4.5、状态色 ≥4.5、`accent` 与其上文字 ≥4.5、`text_3` ≥3（仅限非关键标签）。

---

## 3 布局方向

三种都必须解决同一件事：**把 172px 的空盒子收掉，把实心重量给主按钮。** 三者不是换皮，是三种信息优先序。

### 3.1 L-A「双列开关 · 单卡主体」（推荐主布局，六段 prompt 用它）

开关从 3 行 28px（84px）压成 **3 行 × 2 列网格（32px 行高 + 6px 行距 = 108px）**，五个开关占五格，**第六格放热键键帽 `F8`** —— 顺手解决"5 个开关在 6 宫格里差一格"的尴尬，还给了热键一个固定位置。结果区升格为上半部唯一的大面板。

| 纵向区段 | y 区间 | 高 | 内容 |
| :--- | :--- | :--- | :--- |
| 标题条 | 8–40 | 32 | 状态点 (7px) + 状态文字 15px + 右侧 `□ △ ×` 三个 16px 图标按钮 |
| 结果区 | 48–144 | 96 | 单卡。有结果：mono 26 居中 + 下方 11px 来源标签；空闲：**不画边框**，只一行 13px `就绪 · 按 F8 开始` |
| 开关网格 | 156–264 | 108 | 2 列 × 3 行，列宽 178，行高 32：标签 13px 左 + 28×16 开关轨道右 |
| 主按钮 | 276–320 | 44 | 满宽 **实心 accent 填充** + `accent_text_on` 文字 15px + 右侧 30% alpha 的 `F8` |
| 状态行 | 328–360 | 32 | 左 12px 文案；底部 2px 进度轨（`progress_track` + `progress_fill`） |
| 底部文字链 | 368–396 | 28 | 左 `校准向导`、右 `设置`，11px `text_3` |

合计 404 = 420 − 上下 8px 边距，**零空盒子**。开关由圆形指示灯改为 28×16 轨道 + 12px 滑块，开态轨道实心 accent、滑块深色；「开/关」二字删除（状态由滑块位置表达，符合开关的既有认知）。

**适配**：Ember（暖色需要网格给出的秩序感）、Paper、Nightwatch。

### 3.2 L-B「细线分隔 · 无卡片」

**全窗只有一块面，不存在任何填充卡片。** 五个开关回到单列（26px 高 + 1px `line_subtle` 分隔线），靠线与线的节奏建立秩序；hover 时才浮出 `bg_raised` 底色。主按钮仍是 44px 实心。结果区是同一条通栏（88px），无边框、无填充，只有 mono 结果与一条 1px 顶线。标题条下方加一条 1px 通栏线。

纵向：8 + 32 + 1 + 8 + 88 + 12 + (26×5+4×2=138) + 12 + 44 + 8 + 30 + 8 + 28 + 8 ≈ 404。

**适配**：Graphite、Frost —— 中性色最大的敌人是多余的框，删掉框它们才显出高级。**优点**：信息密度最高、最"技术"。**代价**：没有任何卡片承载"当前结果"，成功态需要靠 mono 字号（26–28）与 accent 顶线顶住视觉重心。

### 3.3 L-C「动作优先 · 上下三分区」

把**结果放在最上面**（88px，mono 26，最大字号），**主按钮紧随其后**（48px 实心，右侧带键帽 `F8` 描边方块），最后才是五个开关（单列 24px 紧排 + 6px 行距 = 144px），底部文字链。标题条压到 28px。

纵向：8 + 28 + 8 + 88 + 12 + 48 + 12 + (24×5+6×4=144) + 8 + 28 + 8 + 20 + 8 ≈ 420（紧，可把开关行距降到 4px）。

**适配**：Celadon（柔和色 + 大留白的关系更协调）、Nightwatch。**优点**：一眼看到结果，符合"按一次键 → 看结果"的真实动线。**代价**：开关被压到最底部，说明性变弱，新手第一次需要找一下。

### 3.4 五状态与按钮谓词（必须写进实现，不允许留空态）

| 状态 | 结果区 | 主按钮 | 附加 |
| :--- | :--- | :--- | :--- |
| **Idle / 空** | 无边框单行：`就绪 · 按 F8 开始`（13px `text_2`）。**不画空卡** | **enabled 谓词**：`已校准 && 至少一个开关为开`。不满足时按钮为 `bg_raised` + 1px `line_strong`，文案改「先校准」（未校准）或「请开启至少一项」（全关），点击走对应修复动作，不静默禁用 | 顶部热键键帽常显 |
| **Loading / 识别中** | 保留上一结果（不清空），下方进度条由不定长转为实测进度；若后端无进度则只显示阶段文字，**禁止假进度** | 文案变「取消操作」，且**二次点击在 300ms 内忽略**（防连点） | 状态点呼吸 alpha，不放大 |
| **Success** | mono 26 结果居中 + 11px 来源标签（`摩斯` / `指纹` / `操作台`）+ 12px `已输入` 于右下 | 回到「开始识别」 | 结果区 1px `ok` 描边（仅描边，不铺底色） |
| **Partial / 警告** | 显示结果但带 `warn` 描边 + 一行 12px 提示「识别可能有误，请核对」（低置信 / 部分格子缺失） | 主按钮不变 + 出现「重新识别」文字链 | 不阻断流程 |
| **Error** | 结果区改 `err` 描边 + **人话原因**（`未找到游戏窗口，请先把游戏切到前台`），附「重试」文字链 | 主按钮恢复可用，文案「重试识别」 | 不用 modal、不弹窗 |

---

## 4 生成用 prompt

统一说明：以下六段都用 **L-A 布局**，只改配色、圆角、字号与气质，便于横向对比。图像模型对中文标签大概率乱码，**对照图只看配色与结构，不看文字**。共同负面词已写在每段末尾。

### 4.1 Graphite

```
Flat vector UI mockup of a compact 400x420 px Windows desktop utility window, straight-on front view, aspect ratio exactly 400:420, no perspective, no device frame, no wallpaper. Layout top to bottom, inner padding 12px horizontal and 8px vertical, background #121315 with a 1px #34383D border and 8px corner radius. (1) 32px title bar: 7px solid dot #5FBF8A at left, 15px label "待机 · 未识别" in #EDEFEF, right side three 16px glyph buttons square, triangle, x in #9CA2A7. (2) 96px result panel, 8px radius, fill #0C0D0F, 1px #25282C border, centered monospaced 26px text "4 2 7 1" in #EDEFEF, 11px caption "目标序列" in #767C81 below it. (3) 3x2 grid of six cells, cell 178x32, gaps 6px: five cells are flat rows with fill #1B1D20, 1px #25282C border, 4px radius, label at left in 13px #EDEFEF, and at right a 28x16 switch track: off = fill #0C0D0F with 1px #34383D outline and 12px thumb #767C81, on = solid #E8EBEA track with 12px thumb #0F1011. The sixth cell is a hollow keycap chip with 1px #34383D border reading "F8" in 11px #9CA2A7. (4) 44px full-width primary button, 4px radius, SOLID fill #E8EBEA, centered 15px text "开始识别" in #0F1011, small "F8" at right in #0F1011 at 30% opacity. (5) 32px status line: 12px "就绪" in #9CA2A7 at left, 2px progress track in #25282C at the bottom with a 60px segment in #E8EBEA. (6) 28px footer: "校准向导" left and "设置" right, 11px #767C81. Style: matte, achromatic, zero colored surfaces except the status dot and one thin accent line; strict optical alignment, generous whitespace, hairline borders, no gradients, no glow, no glassmorphism, no purple, no drop shadows, no emoji, no 3D, pixel-crisp typography.
```

### 4.2 Nightwatch

```
Flat vector UI mockup of a compact 400x420 px Windows desktop utility window, straight-on front view, aspect ratio exactly 400:420, no perspective, no device frame, no wallpaper. Layout top to bottom, inner padding 12px horizontal and 8px vertical, background #0F1520 with a 1px #2C3849 border and 10px corner radius. (1) 32px title bar: 7px solid dot #3FD68F at left, 15px label "待机 · 未识别" in #E6EDF7, right side three 16px glyph buttons square, triangle, x in #9AABBF. (2) 96px result panel, 10px radius, fill #0A0F18, 1px #1D2633 border, centered monospaced 26px text "4 2 7 1" in #E6EDF7, 11px caption "目标序列" in #6E8098 below it. (3) 3x2 grid of six cells, cell 178x32, gaps 6px: five cells are flat rows with fill #18202E, 1px #1D2633 border, 6px radius, label at left in 13px #E6EDF7, and at right a 28x16 switch track: off = fill #0A0F18 with 1px #2C3849 outline and 12px thumb #6E8098, on = solid #4C8DFF track with 12px thumb #06101F. The sixth cell is a hollow keycap chip with 1px #2C3849 border reading "F8" in 11px #9AABBF. (4) 44px full-width primary button, 6px radius, SOLID fill #4C8DFF, centered 15px text "开始识别" in #06101F, small "F8" at right in #06101F at 30% opacity. (5) 32px status line: 12px "就绪" in #9AABBF at left, 2px progress track in #1D2633 at the bottom with a 60px segment in #4C8DFF. (6) 28px footer: "校准向导" left and "设置" right, 11px #6E8098. Style: modern productivity software, deep navy black, single electric blue accent, soft 6-10px radii, clear elevation by lightness only; no gradients, no glow, no glassmorphism, no purple, no drop shadows, no emoji, no 3D, pixel-crisp typography.
```

### 4.3 Ember

```
Flat vector UI mockup of a compact 400x420 px Windows desktop utility window, straight-on front view, aspect ratio exactly 400:420, no perspective, no device frame, no wallpaper. Layout top to bottom, inner padding 12px horizontal and 8px vertical, background #191310 with a 1px #3F3024 border and 6px corner radius. (1) 32px title bar: 7px solid dot #6FBF73 at left, 15px label "待机 · 未识别" in #F2E7DA, right side three 16px glyph buttons square, triangle, x in #B8A48E. (2) 96px result panel, 6px radius, fill #0D0906, 1px #2C221A border, centered monospaced 24px text "4 2 7 1" in #F2E7DA, 11px caption "目标序列" in #8A7663 below it. (3) 3x2 grid of six cells, cell 178x32, gaps 6px: five cells are flat rows with fill #241B15, 1px #2C221A border, 3px radius, label at left in 13px #F2E7DA, and at right a 28x16 switch track: off = fill #0D0906 with 1px #3F3024 outline and 12px thumb #8A7663, on = solid #FF9E3D track with 12px thumb #1A0F04. The sixth cell is a hollow keycap chip with 1px #3F3024 border reading "F8" in 11px #B8A48E. (4) 44px full-width primary button, 3px radius, SOLID fill #FF9E3D, centered 15px text "开始识别" in #1A0F04, small "F8" at right in #1A0F04 at 30% opacity. (5) 32px status line: 12px "就绪" in #B8A48E at left, 2px progress track in #2C221A at the bottom with a 60px segment in #FF9E3D. (6) 28px footer: "校准向导" left and "设置" right, 11px #8A7663. Style: warm charcoal like matte field equipment, tight 3px radii, dense but orderly, one hot orange surface carrying all emphasis, hairline warm brown dividers; no gradients, no glow, no glassmorphism, no purple, no drop shadows, no emoji, no 3D, pixel-crisp typography.
```

### 4.4 Paper

```
Flat vector UI mockup of a compact 400x420 px Windows desktop utility window, straight-on front view, aspect ratio exactly 400:420, no perspective, no device frame, no wallpaper. Layout top to bottom, inner padding 12px horizontal and 8px vertical, LIGHT background #F7F3EA with a 1px #C3B8A4 border and 8px corner radius. (1) 32px title bar: 7px solid dot #2F6B45 at left, 15px label "待机 · 未识别" in #211E19, right side three 16px glyph buttons square, triangle, x in #57503F. (2) 96px result panel, 8px radius, fill #EDE7DA, 1px #DCD4C5 border, centered monospaced 26px text "4 2 7 1" in #211E19, 11px caption "目标序列" in #7C7362 below it. (3) 3x2 grid of six cells, cell 178x32, gaps 6px: five cells are flat rows with fill #FFFDF8, 1px #DCD4C5 border, 4px radius, label at left in 13px #211E19, and at right a 28x16 switch track: off = fill #EDE7DA with 1px #C3B8A4 outline and 12px thumb #7C7362, on = solid #2C3E6B track with 12px thumb #FBF8F1. The sixth cell is a hollow keycap chip with 1px #C3B8A4 border reading "F8" in 11px #57503F. (4) 44px full-width primary button, 4px radius, SOLID fill #2C3E6B, centered 15px text "开始识别" in #FBF8F1, small "F8" at right in #FBF8F1 at 30% opacity. (5) 32px status line: 12px "就绪" in #57503F at left, 2px progress track in #DCD4C5 at the bottom with a 60px segment in #2C3E6B. (6) 28px footer: "校准向导" left and "设置" right, 11px #7C7362. Style: warm paper and ink, printed matter rather than software, ink-blue solid button like a rubber stamp, hairline warm outlines, gentle 4-8px radii, no gradients, no glow, no glassmorphism, no purple, no drop shadows, no emoji, no 3D, pixel-crisp typography.
```

### 4.5 Frost

```
Flat vector UI mockup of a compact 400x420 px Windows desktop utility window, straight-on front view, aspect ratio exactly 400:420, no perspective, no device frame, no wallpaper. Layout top to bottom, inner padding 12px horizontal and 8px vertical, LIGHT COOL background #F4F6F8 with a 1px #C2CAD3 border and 10px corner radius. (1) 32px title bar: 7px solid dot #12705A at left, 15px label "待机 · 未识别" in #161C22, right side three 16px glyph buttons square, triangle, x in #4E5A66. (2) 96px result panel, 10px radius, fill #E9EDF1, 1px #DAE0E6 border, centered monospaced 28px text "4 2 7 1" in #161C22, 11px caption "目标序列" in #78848F below it. (3) 3x2 grid of six cells, cell 178x32, gaps 6px: five cells are flat rows with fill #FDFDFE, 1px #DAE0E6 border, 5px radius, label at left in 13px #161C22, and at right a 28x16 switch track: off = fill #E9EDF1 with 1px #C2CAD3 outline and 12px thumb #78848F, on = solid #1C2024 track with 12px thumb #F7F9FA. The sixth cell is a hollow keycap chip with 1px #C2CAD3 border reading "F8" in 11px #4E5A66. (4) 44px full-width primary button, 5px radius, SOLID near-black fill #1C2024, centered 15px text "开始识别" in #F7F9FA, small "F8" at right in #F7F9FA at 30% opacity. (5) 32px status line: 12px "就绪" in #4E5A66 at left, 2px progress track in #DAE0E6 at the bottom with a 60px segment in #1C2024. (6) 28px footer: "校准向导" left and "设置" right, 11px #78848F. Style: clinical precision instrument, cool white, achromatic surfaces, one near-black button, colour appears only in tiny status marks, crisp hairlines, no gradients, no glow, no glassmorphism, no purple, no drop shadows, no emoji, no 3D, pixel-crisp typography.
```

### 4.6 Celadon

```
Flat vector UI mockup of a compact 400x420 px Windows desktop utility window, straight-on front view, aspect ratio exactly 400:420, no perspective, no device frame, no wallpaper. Layout top to bottom, inner padding 12px horizontal and 8px vertical, LIGHT SAGE background #EDF1EC with a 1px #B8C6BA border and 12px corner radius. (1) 32px title bar: 7px solid dot #227653 at left, 15px label "待机 · 未识别" in #1B2A22, right side three 16px glyph buttons square, triangle, x in #4A5C50. (2) 96px result panel, 12px radius, fill #E1E8E2, 1px #D2DCD3 border, centered monospaced 24px text "4 2 7 1" in #1B2A22, 11px caption "目标序列" in #6F8075 below it. (3) 3x2 grid of six cells, cell 178x32, gaps 6px: five cells are flat rows with fill #F8FAF7, 1px #D2DCD3 border, 8px radius, label at left in 13px #1B2A22, and at right a 28x16 switch track: off = fill #E1E8E2 with 1px #B8C6BA outline and 12px thumb #6F8075, on = solid #1F6B4F track with 12px thumb #F4F8F4. The sixth cell is a hollow keycap chip with 1px #B8C6BA border reading "F8" in 11px #4A5C50. (4) 44px full-width primary button, 8px radius, SOLID fill #1F6B4F, centered 15px text "开始识别" in #F4F8F4, small "F8" at right in #F4F8F4 at 30% opacity. (5) 32px status line: 12px "就绪" in #4A5C50 at left, 2px progress track in #D2DCD3 at the bottom with a 60px segment in #1F6B4F. (6) 28px footer: "校准向导" left and "设置" right, 11px #6F8075. Style: soft monochromatic celadon glaze, calm and low-stimulus, generous 8-12px radii, no harsh pure white, gentle hairlines, no gradients, no glow, no glassmorphism, no purple, no drop shadows, no emoji, no 3D, pixel-crisp typography.
```

---

## 5 实现方案（给实现者）

1. **主题数据结构**：`theme.rs` 的颜色 `const` 全部改成一个 `Palette` 值类型；**非颜色 token（`SPACE_*` / `WINDOW_SIZE` / `TITLE_BAR_H` / `RESIZE_GRIP` 等）保持 `const` 不动**，它们与主题无关。

```rust
pub enum ThemeId { Graphite, Nightwatch, Ember, Paper, Frost, Celadon }

#[derive(Clone, Copy, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub bg_void: Color32, pub bg_panel: Color32, pub bg_raised: Color32, pub bg_inset: Color32,
    pub line_strong: Color32, pub line_subtle: Color32,
    pub text_1: Color32, pub text_2: Color32, pub text_3: Color32,
    pub accent: Color32, pub accent_text_on: Color32,
    pub ok: Color32, pub warn: Color32, pub err: Color32,
    pub radius_button: u8, pub radius_panel: u8,          // 直接喂 CornerRadius::same
    pub font_title: f32, pub font_body: f32, pub font_label: f32, pub font_value: f32,
}

impl Palette {
    pub const fn base(id: ThemeId) -> Self;              // 上表 18 个字段的静态表
    pub fn with_accent(self, hue_deg: f32) -> Self;      // §2.7 的 OKLCH 派生
}
```

2. **运行时切主题**：**以自有 `Palette` 为准，`Context::set_visuals` 只做兜底**。理由：现有 UI 100% 自绘（`view.rs` 全部走 `painter`），`Visuals` 只影响内嵌原生控件（文本编辑框、滚动条、tooltip）。做法是每帧 `let pal = Palette::of(state.theme_id, state.accent_hue);` 显式传下去；仅在 `theme_id`/`accent_hue` **变化的帧**调用一次 `ctx.set_visuals(visuals_from(&pal))`。`set_fonts` 只在启动时调一次，绝不随主题变。**不需要重启**，`egui` 立即模式天然支持。
3. **落地顺序**：`theme::install()` 拆成 `install_fonts(ctx)`（一次）+ `apply_visuals(ctx, &pal)`（可重入）。
4. **配置持久化**：`crates/config` 增加 `theme_id: ThemeId`、`accent_hue: Option<f32>`（`None` = 用该主题的默认 accent）。缺字段时用 `#[serde(default)]` 回落到 `Ember`，老配置文件不炸。
5. **40+ 处引用的迁移结论：一次性机械替换，不做访问器。**
   - 现状：`theme::` 共 251 处，其中**颜色类约 90 处**（`ACCENT` 15、`TEXT_3` 17、`TEXT_1` 14、`TEXT_2` 8、`LINE_SUBTLE` 7、`BG_PANEL` 6、`BG_INSET` 5、`WARN` 4、`OK` 3、`ERR` 3、`LINE_STRONG` 3、`BG_RAISED` 3、`SHADOW` 1、`HIGHLIGHT` 1），其余 161 处是尺寸/字号/圆角。
   - 不做全局访问器的硬理由：访问器需要 `thread_local` 或 `static Mutex`，会让 21 个自绘函数获得**隐藏全局状态**，直接破坏现有 `view.rs` 里那批纯函数单测（`fn stage()/fn rows_for()/...` 都是无副作用构造）。显式 `pal: &Palette` 参数才是可测的。
   - 改动面：`view.rs` 里约 21 个函数签名加一个 `pal: &Palette`（它们**已经**都带 `painter: &egui::Painter`，新参数落在同一层，调用点同步加一个实参，`rustc` 会全部报出来，不会漏）。`main_window` 收 `pal` 后向下传一遍即可。
6. **禁新增魔法数**：§2.0 的派生 token 一律写成 `impl Palette` 上的方法（`fn btn_fill_hover(&self)`），不得在 `view.rs` 里就地算。
