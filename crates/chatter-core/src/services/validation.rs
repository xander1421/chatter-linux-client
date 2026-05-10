//! Input validation utilities (password strength, PIN strength).

/// Password strength level (0–4).
/// 0 = empty, 1 = weak, 2 = fair, 3 = good, 4 = strong.
pub fn password_strength(password: &str) -> i32 {
    if password.is_empty() {
        return 0;
    }

    let len = password.len();
    let has_lower = password.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = password.chars().any(|c| c.is_ascii_uppercase());
    let has_digit = password.chars().any(|c| c.is_ascii_digit());
    let has_special = password.chars().any(|c| !c.is_alphanumeric());

    let variety = has_lower as i32 + has_upper as i32 + has_digit as i32 + has_special as i32;

    // Score based on length + character variety:
    //   len < 6          → weak (1) regardless of variety
    //   len 6..10        → fair (2), upgraded to good (3) if variety >= 3
    //   len 10..14       → good (3), upgraded to strong (4) if variety >= 3
    //   len >= 14        → strong (4) if variety >= 2, else good (3)
    if len < 6 {
        1
    } else if len < 10 {
        if variety >= 3 { 3 } else { 2 }
    } else if len < 14 {
        if variety >= 3 { 4 } else { 3 }
    } else {
        if variety >= 2 { 4 } else { 3 }
    }
}

/// PIN strength level: 0 = empty, 1 = weak, 2 = fair, 3 = strong.
///
/// Aligned with `crypto::svr::validate_pin` which requires
/// `MIN_PIN_LENGTH = 8` ASCII printable characters. Anything shorter
/// will be rejected at registration / change-PIN time, so the UI
/// surfaces it as weak.
pub fn pin_strength(pin: &str) -> i32 {
    let len = pin.len();
    if len == 0 {
        0
    } else if len < crate::crypto::svr::MIN_PIN_LENGTH {
        1 // weak — will be rejected by validate_pin
    } else if len < 12 {
        2 // fair — meets minimum but easy to brute force
    } else {
        3 // strong
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_password() {
        assert_eq!(password_strength(""), 0);
    }

    #[test]
    fn short_password_is_weak() {
        assert_eq!(password_strength("abc"), 1);
        assert_eq!(password_strength("Ab1!"), 1); // short even with variety
    }

    #[test]
    fn length_only_passwords() {
        // 15 lowercase chars → variety=1, len>=14 → needs variety>=2 for 4, so 3
        assert_eq!(password_strength("aaaaaaaaaaaaaaa"), 3);
        // 8 lowercase chars → variety=1, len 6..10 → 2
        assert_eq!(password_strength("abcdefgh"), 2);
    }

    #[test]
    fn mixed_passwords() {
        // "Abc1!" → len=5 → weak regardless
        assert_eq!(password_strength("Abc1!"), 1);
        // "Abc1!xyz" → len=8, variety=4 → good (3)
        assert_eq!(password_strength("Abc1!xyz"), 3);
        // "Abcdef1!" → len=8, variety=4 → good (3)
        assert_eq!(password_strength("Abcdef1!"), 3);
        // "Abcdefgh1!xy" → len=12, variety=4 → strong (4)
        assert_eq!(password_strength("Abcdefgh1!xy"), 4);
    }

    #[test]
    fn long_mixed_is_strong() {
        assert_eq!(password_strength("MyP@ssw0rd1234!"), 4);
    }

    #[test]
    fn pin_strength_levels() {
        assert_eq!(pin_strength(""), 0);
        assert_eq!(pin_strength("12"), 1, "below MIN_PIN_LENGTH = weak");
        assert_eq!(pin_strength("1234567"), 1, "7 chars still below 8 min");
        assert_eq!(pin_strength("12345678"), 2, "exactly 8 = fair");
        assert_eq!(pin_strength("12345678901"), 2, "11 chars = fair");
        assert_eq!(pin_strength("123456789012"), 3, "12+ chars = strong");
    }
}
