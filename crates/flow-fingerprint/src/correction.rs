//! 人名两级纠错（spec D2 兜底引擎的第二、三级）。
//!
//! 严格等价 `_upstream/core/fingerprint.py:48-59` 的 `_correct_person_name`：
//! 1. 精确映射表 `{"克菜尔": "克莱尔"}`（可扩展）；
//! 2. 与库里所有角色名比「字符交集占比」，取最优且占比 ≥ 0.6（[`FUZZY_MIN_RATIO`]）。
//!
//! 分母是 `max(len(raw), len(role))`，与上游 `sum(1 for char in name if char in c) / max(len(name), len(c), 1)`
//! 逐字对齐 —— **不许**换成编辑距离或引入 `difflib` 类依赖。

use delta_templates::RoleLibrary;

/// 精确映射表（上游 `_OCR_CORRECTION_MAP`）。OCR 常把「莱」认成「菜」。
pub const EXACT_CORRECTIONS: [(&str, &str); 1] = [("克菜尔", "克莱尔")];

/// 模糊兜底的最低字符交集占比（上游 `>= 0.6`）。
pub const FUZZY_MIN_RATIO: f32 = 0.6;

/// 按上游两级策略纠错，返回最可能的角色名（不在库里也照实返回，交由调用方报错）。
pub fn correct_role_name(raw: &str, library: &RoleLibrary) -> String {
    let name = raw.trim();
    if let Some((_, mapped)) = EXACT_CORRECTIONS.iter().find(|(from, _)| *from == name) {
        return (*mapped).to_string();
    }
    if library.find(name).is_some() {
        return name.to_string();
    }

    let best = library
        .roles()
        .iter()
        .map(|role| (&role.name, intersection_ratio(name, &role.name)))
        .fold(None::<(&String, f32)>, |best, current| match best {
            Some(previous) if previous.1 >= current.1 => Some(previous),
            _ => Some(current),
        });

    match best {
        Some((role_name, ratio)) if ratio >= FUZZY_MIN_RATIO => role_name.clone(),
        _ => name.to_string(),
    }
}

/// 字符交集占比：`|{c ∈ raw : c ∈ candidate}| / max(len(raw), len(candidate))`。
pub fn intersection_ratio(raw: &str, candidate: &str) -> f32 {
    let raw_len = raw.chars().count();
    let candidate_len = candidate.chars().count();
    if raw_len == 0 || candidate_len == 0 {
        return 0.0;
    }
    let hits = raw
        .chars()
        .filter(|character| candidate.contains(*character))
        .count();
    hits as f32 / raw_len.max(candidate_len) as f32
}

/// 从 OCR 文本里抽数字串（等价 Python `re.findall(r"\d+")`，且额外兼容全角数字）。
///
/// 全角是必要兼容：`Windows.Media.Ocr` 的 zh-Hans 模型在部分字体下会输出 `０１２`，
/// 而 Python 的 `\d` 在 Unicode 模式下同样匹配全角数字。
pub fn parse_numbers(text: &str) -> Vec<u32> {
    let mut numbers = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        match digit_value(character) {
            Some(value) => current.push(char::from(b'0' + value)),
            None => flush(&mut current, &mut numbers),
        }
    }
    flush(&mut current, &mut numbers);
    numbers
}

fn flush(current: &mut String, numbers: &mut Vec<u32>) {
    if current.is_empty() {
        return;
    }
    if let Ok(value) = current.parse::<u32>() {
        numbers.push(value);
    }
    current.clear();
}

/// 半角 / 全角数字 → 0..9。
fn digit_value(character: char) -> Option<u8> {
    match character {
        '0'..='9' => Some(character as u8 - b'0'),
        '０'..='９' => Some((character as u32 - 0xFF10) as u8),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use delta_templates::{Role, RoleLibrary, Template};
    use delta_vision::GrayImage;

    fn library(names: &[&str]) -> RoleLibrary {
        let roles = names
            .iter()
            .map(|name| Role {
                name: (*name).to_string(),
                templates: vec![Template {
                    no: 1,
                    image: GrayImage::filled(8, 8, 200).expect("合法模板"),
                }],
            })
            .collect();
        RoleLibrary::from_roles(roles).expect("非空角色库")
    }

    #[test]
    fn exact_map_wins_over_fuzzy() {
        let library = library(&["克莱尔", "卢卡斯", "无名"]);
        assert_eq!(correct_role_name("克菜尔", &library), "克莱尔");
        assert_eq!(correct_role_name("  克菜尔 ", &library), "克莱尔", "先 trim");
    }

    #[test]
    fn exact_name_is_returned_untouched() {
        let library = library(&["克莱尔", "卢卡斯"]);
        assert_eq!(correct_role_name("卢卡斯", &library), "卢卡斯");
    }

    #[test]
    fn fuzzy_fallback_needs_sixty_percent_intersection() {
        let library = library(&["克莱尔", "卢卡斯", "格赫罗斯"]);
        // 「克莱而」与「克莱尔」交集 2/3 = 0.667 ≥ 0.6 → 命中。
        assert_eq!(correct_role_name("克莱而", &library), "克莱尔");
        // 「克」与「克莱尔」交集 1/3 = 0.333 < 0.6 → 原样返回。
        assert_eq!(correct_role_name("克", &library), "克");
        // 完全没有交集 → 原样返回。
        assert_eq!(correct_role_name("张三", &library), "张三");
    }

    #[test]
    fn intersection_ratio_matches_upstream_formula() {
        assert_eq!(intersection_ratio("克菜尔", "克莱尔"), 2.0 / 3.0);
        assert_eq!(intersection_ratio("abcd", "ab"), 0.5);
        assert_eq!(intersection_ratio("", "abc"), 0.0);
        assert_eq!(intersection_ratio("abc", ""), 0.0);
        assert_eq!(intersection_ratio("aa", "aa"), 1.0);
    }

    #[test]
    fn numbers_are_extracted_like_re_findall() {
        assert_eq!(parse_numbers("目标 7 个, 最大 9"), vec![7, 9]);
        assert_eq!(parse_numbers("0123"), vec![123], "连续数字算一个数");
        assert_eq!(parse_numbers("１２"), vec![12], "全角数字也要认");
        assert_eq!(parse_numbers("没有数字"), Vec::<u32>::new());
        assert_eq!(parse_numbers("0"), vec![0]);
        assert_eq!(parse_numbers("a0b3"), vec![0, 3]);
    }
}
