//! t4 离线回归门禁（spec §4「指纹离线复现」）——用 `images/` 的**真实 54 张模板**。
//!
//! 合成方式：把某角色的真实模板整体缩放 0.8 倍并叠一层确定性轻噪声当「游戏截图里的候选格」，
//! 走与线上完全相同的代码路径（非等比 resize + 中心 70% 核心 + NCC + 最大权匹配）。
//!
//! 断言四件事：
//! 1. 投票认出该角色；
//! 2. `N` 等于该角色的模板张数（spec D3）；
//! 3. `assign` 返回 N 对，且模板号无重复（spec D4，最大权匹配的核心价值）；
//! 4. 全部 N 对都通过 `min_score`，即没有靠低分边凑数。

use delta_templates::{assign, vote_role, RoleLibrary, TemplateError, Vote, IMAGES_DIR};
use delta_vision::GrayImage;

/// 确定性线性同余噪声（测试不能依赖 rand，也不能让结果随机翻转）。
struct Noise(u32);

impl Noise {
    /// 均匀落进 `[-amplitude, amplitude]`。
    fn next(&mut self, amplitude: i16) -> i16 {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let span = (amplitude * 2 + 1) as u32;
        ((self.0 >> 16) % span) as i16 - amplitude
    }
}

/// 模拟游戏截图里的一格：整体缩放 `scale` 倍 + 轻噪声（模拟压缩与抗锯齿）。
///
/// 噪声幅度必须**按对比度成比例**，不能写死 ±5：`巴斯特/3.png` 的灰度标准差只有 8.3
/// （量程 26~90），固定 ±5 会淹掉信号，自匹配得分从 0.99 塌到 0.29 —— 那是夹具失真，
/// 不是算法退化。真实游戏截图里的低对比模板同样保留自身对比度。
fn synth_cell(template: &GrayImage, scale: f32, seed: u32) -> GrayImage {
    let w = ((template.w as f32) * scale).round() as u32;
    let h = ((template.h as f32) * scale).round() as u32;
    let scaled = template.resize_to(w, h).expect("缩放候选格");
    let values = scaled.as_slice();
    let (low, high) = (
        values.iter().copied().min().unwrap_or(0),
        values.iter().copied().max().unwrap_or(0),
    );
    let amplitude = i16::from((high - low) / 40).clamp(1, 6);
    let mut noise = Noise(seed);
    let data = values
        .iter()
        .map(|&value| (i16::from(value) + noise.next(amplitude)).clamp(0, 255) as u8)
        .collect();
    GrayImage::new(w, h, data).expect("候选格")
}

fn library() -> RoleLibrary {
    RoleLibrary::load_from_dir(IMAGES_DIR).expect("真实 54 张模板必须可加载")
}

/// 主门禁：8 个角色 × 各自真实模板，逐个角色完整投票 + 指派。
#[test]
fn offline_regression_recognises_every_role_with_real_templates() {
    let library = library();
    let mut checked = 0usize;

    for (role_index, role) in library.roles().iter().enumerate() {
        let templates: Vec<GrayImage> = role.templates.iter().map(|t| t.image.clone()).collect();
        let n = role.n() as usize;

        // 存活格 = N+1（spec §2.2 不变量 9）：前 N 格是本角色的真实模板，
        // 第 N+1 格是别的角色的模板 —— 多出来的干扰格必须被匹配算法自然忽略。
        let mut cells: Vec<GrayImage> = templates
            .iter()
            .enumerate()
            .map(|(idx, template)| {
                synth_cell(template, 0.8, 0x9E37 + (role_index as u32) * 97 + idx as u32)
            })
            .collect();
        let distractor_role = &library.roles()[(role_index + 1) % library.roles().len()];
        cells.push(synth_cell(&distractor_role.templates[0].image, 0.8, 0x1234));
        assert_eq!(cells.len(), n + 1, "{} 的存活格必须是 N+1", role.name);

        let vote = vote_role(&cells, &library).expect("投票");
        let Vote::Chosen(voted) = &vote else {
            panic!(
                "{} 的投票必须是高置信 Chosen，实际 {vote:?}（最高分 {} vs 次高 {}）",
                role.name,
                vote.leader().score,
                match &vote {
                    Vote::Ambiguous { runner_up, .. } => runner_up.score,
                    Vote::Chosen(_) => f32::NAN,
                }
            );
        };
        assert_eq!(
            voted.role.name, role.name,
            "投票认成了 {}，应为 {}（得分 {}）",
            voted.role.name, role.name, voted.score
        );
        assert_eq!(voted.role.n(), n as u32, "N 必须等于该角色模板张数");

        let assignment = assign(&cells, role).expect("指派");
        assert!(
            assignment.is_complete(),
            "{} 存在低于 min_score 的模板：{:?}（全部配对 {:?}）",
            role.name,
            assignment.unmatched,
            assignment.pairs
        );
        assert_eq!(
            assignment.len(),
            n,
            "{} 必须返回 N={n} 对，实际 {}",
            role.name,
            assignment.len()
        );

        let mut template_numbers: Vec<u32> =
            assignment.pairs.iter().map(|pair| pair.tmpl_no).collect();
        let distinct = {
            let mut sorted = template_numbers.clone();
            sorted.sort_unstable();
            sorted.dedup();
            sorted
        };
        template_numbers.sort_unstable();
        assert_eq!(
            template_numbers, distinct,
            "{}：模板号重复出现，最大权匹配被破坏",
            role.name
        );
        assert_eq!(
            template_numbers,
            (1..=n as u32).collect::<Vec<u32>>(),
            "{}：每个模板号恰好用一次",
            role.name
        );

        // 点击序检查（spec D5 / §2.2 不变量 3）：模板编号升序。
        let order: Vec<u32> = assignment.pairs.iter().map(|p| p.tmpl_no).collect();
        assert!(order.windows(2).all(|w| w[0] < w[1]), "点击序非升序：{order:?}");

        checked += 1;
    }
    assert_eq!(checked, 9, "九个角色都要过一遍");
}

/// spec D3 的 N 口径：N 由模板张数给出，且与上游实测一致。
#[test]
fn offline_regression_n_equals_template_count_per_role() {
    let library = library();
    let expected = [
        ("埃德温", 4u32),
        ("克莱尔", 6),
        ("卢卡斯", 6),
        ("巴斯特", 6),
        ("德穆兰", 6),
        ("无名", 6),
        ("罗米修斯", 6),
        ("雅各布", 6),
        ("格赫罗斯", 8),
    ];
    for (name, n) in expected {
        let role = library.find(name).unwrap_or_else(|| panic!("缺少角色 {name}"));
        assert_eq!(role.n(), n, "{name}");
        assert_eq!(role.templates.len(), n as usize);
    }
}

/// 一个格子的空输入必须显式报错，不得静默返回空指派。
#[test]
fn offline_regression_empty_cells_are_rejected() {
    let library = library();
    let role = library.find("格赫罗斯").expect("角色存在");
    let error = assign(&[], role).expect_err("空格子必须报错");
    assert!(matches!(error, TemplateError::NoCells), "{error:?}");
}

/// 裁剪到极小尺寸的候选格（跨分辨率极端情形）不得 panic，只能给出低分或错误。
#[test]
fn offline_regression_tiny_cells_do_not_panic() {
    let library = library();
    let role = library.find("埃德温").expect("角色存在");
    let tiny = GrayImage::filled(3, 3, 128).expect("合法图");
    let result = assign(std::slice::from_ref(&tiny), role);
    match result {
        Ok(assignment) => {
            assert!(assignment.is_empty(), "3x3 纯色格不可能达到 0.45");
            assert_eq!(assignment.unmatched.len(), role.n() as usize);
        }
        Err(error) => assert!(matches!(error, TemplateError::NoCells), "{error:?}"),
    }
}

/// 性能基准（`#[ignore]`，不进常规门禁）：612 次匹配口径（9 格 × 68 模板）。
///
/// 运行方式：`cargo test -p delta-templates -- --ignored bench --nocapture`
#[test]
#[ignore = "性能基准，需手动 --ignored 触发"]
fn bench_vote_612_matches_under_40ms() {
    let library = library();
    let role = library.find("格赫罗斯").expect("模板最多的角色");
    let cells: Vec<GrayImage> = (0..9)
        .map(|idx| synth_cell(&role.templates[idx % role.templates.len()].image, 0.8, idx as u32))
        .collect();

    // 预热缓存与线程池，避免把首次分配算进基准。
    let _ = vote_role(&cells, &library).expect("预热");

    let started = std::time::Instant::now();
    let iterations = 3;
    for _ in 0..iterations {
        let _ = vote_role(&cells, &library).expect("投票");
    }
    let elapsed = started.elapsed() / iterations;
    println!("612 次匹配口径平均耗时：{elapsed:?}");
    assert!(
        elapsed < std::time::Duration::from_millis(40),
        "612 次匹配必须 <40ms，实测 {elapsed:?}"
    );
}

