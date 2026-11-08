//! 数字键注入（spec §2.2 不变量 5：摩斯流程要敲出已识别到的数字键）。
//!
//! 用 `KEYEVENTF_SCANCODE` 而不是 VK：游戏按物理键位读输入，VK 注入在 DirectInput
//! 场景下可能丢失。扫描码表是 PS/2 Set 1（与 `MapVirtualKey` 对 `0..=9` 的输出一致）。

use crate::inject::{plan_digit_events, Injected};
use crate::InputError;

/// 数字字符 → PS/2 Set 1 扫描码。非数字返回 `None`。
pub fn scancode_for_digit(digit: char) -> Option<u16> {
    let scancode = match digit {
        '1' => 0x02,
        '2' => 0x03,
        '3' => 0x04,
        '4' => 0x05,
        '5' => 0x06,
        '6' => 0x07,
        '7' => 0x08,
        '8' => 0x09,
        '9' => 0x0A,
        '0' => 0x0B,
        _ => return None,
    };
    Some(scancode)
}

/// 敲一个数字键（按下 + 抬起）。
pub fn key_digit(digit: char) -> Result<(), InputError> {
    for event in plan_digit_events(digit)? {
        crate::inject::inject(&event)?;
    }
    Ok(())
}

/// 依次敲出一串数字，键间默认 [`crate::CLICK_HOLD`]（spec §2.2 不变量 6：键间隔 0.05s）。
pub fn key_digits(digits: &str) -> Result<(), InputError> {
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 {
            std::thread::sleep(crate::CLICK_HOLD);
        }
        key_digit(digit)?;
    }
    Ok(())
}

/// 供测试断言事件序列的规划入口。
pub fn plan_key_digits(digits: &str) -> Result<Vec<Injected>, InputError> {
    let mut events = Vec::with_capacity(digits.chars().count() * 2);
    for digit in digits.chars() {
        events.extend(plan_digit_events(digit)?);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 扫描码必须是 Set 1 的真实值，且 10 个数字互不相同。
    #[test]
    fn scancode_table_covers_all_ten_digits_uniquely() {
        let expected = [
            ('1', 0x02u16),
            ('2', 0x03),
            ('3', 0x04),
            ('4', 0x05),
            ('5', 0x06),
            ('6', 0x07),
            ('7', 0x08),
            ('8', 0x09),
            ('9', 0x0A),
            ('0', 0x0B),
        ];
        let mut seen = Vec::new();
        for (digit, scancode) in expected {
            assert_eq!(scancode_for_digit(digit), Some(scancode), "{digit}");
            assert!(!seen.contains(&scancode), "扫描码重复：{scancode}");
            seen.push(scancode);
        }
        assert_eq!(seen.len(), 10);
    }

    #[test]
    fn non_digits_are_rejected() {
        for digit in ['a', ' ', '١', '-', '\n'] {
            assert_eq!(scancode_for_digit(digit), None, "{digit:?}");
            assert_eq!(
                plan_key_digits(&digit.to_string()).expect_err("必须报错"),
                InputError::NotADigit(digit)
            );
        }
    }

    /// 序列里出现非数字时必须在投递前整体失败，不能"敲一半再报错"。
    #[test]
    fn plan_is_all_or_nothing() {
        let error = plan_key_digits("12x").expect_err("中途非法");
        assert_eq!(error, InputError::NotADigit('x'));
    }

    #[test]
    fn plan_pairs_each_digit_with_down_and_up() {
        let events = plan_key_digits("07").expect("规划");
        assert_eq!(events.len(), 4);
        assert_eq!(
            events[0],
            Injected::Key {
                scancode: 0x0B,
                up: false
            }
        );
        assert_eq!(
            events[3],
            Injected::Key {
                scancode: 0x08,
                up: true
            }
        );
    }
}
