use fancy_regex::Regex;
use rand::seq::SliceRandom;
use rand::{thread_rng, Rng};

pub const DEFAULT_REGEX: &str = r"^(?=.{8,128}$)(?=.*[a-z])(?=.*[A-Z])(?=.*[^A-Za-z0-9\s]).*$";
pub const DEFAULT_HINT: &str =
    "密码必须为 8–128 位，并至少包含一个大写字母、一个小写字母和一个特殊字符";
const DEFAULT_GENERATED_LENGTH: usize = 8;
const MIN_GENERATED_LENGTH: usize = 8;
const MAX_GENERATED_LENGTH: usize = 128;
const GENERATION_ATTEMPTS: usize = 1024;

pub struct PasswordPolicy {
    regex: Regex,
    hint: String,
    generated_length: usize,
}

impl PasswordPolicy {
    pub fn from_env() -> Result<Self, String> {
        let pattern =
            std::env::var("CANGLING_PASSWORD_REGEX").unwrap_or_else(|_| DEFAULT_REGEX.to_string());
        let hint = std::env::var("CANGLING_PASSWORD_HINT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_HINT.to_string());
        let generated_length = match std::env::var("CANGLING_PASSWORD_GENERATED_LENGTH") {
            Ok(value) => value.parse::<usize>().map_err(|_| {
                "CANGLING_PASSWORD_GENERATED_LENGTH 必须是 8–128 的整数".to_string()
            })?,
            Err(_) => DEFAULT_GENERATED_LENGTH,
        };
        Self::new(&pattern, hint, generated_length)
    }

    fn new(pattern: &str, hint: String, generated_length: usize) -> Result<Self, String> {
        if !(MIN_GENERATED_LENGTH..=MAX_GENERATED_LENGTH).contains(&generated_length) {
            return Err("CANGLING_PASSWORD_GENERATED_LENGTH 必须在 8–128 之间".to_string());
        }
        let regex = Regex::new(pattern)
            .map_err(|error| format!("CANGLING_PASSWORD_REGEX 无效：{error}"))?;
        Ok(Self {
            regex,
            hint,
            generated_length,
        })
    }

    pub fn validate(&self, password: &str) -> Result<(), String> {
        match self.regex.is_match(password) {
            Ok(true) => Ok(()),
            Ok(false) => Err(self.hint.clone()),
            Err(error) => Err(format!("密码策略执行失败：{error}")),
        }
    }

    pub fn generate(&self) -> Result<String, String> {
        let mut rng = thread_rng();
        const LOWER: &[u8] = b"abcdefghijkmnopqrstuvwxyz";
        const UPPER: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
        const DIGITS: &[u8] = b"23456789";
        const SPECIAL: &[u8] = b"!@#$%^&*-_=+";
        const ALL: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789!@#$%^&*-_=+";

        for _ in 0..GENERATION_ATTEMPTS {
            let mut bytes = Vec::with_capacity(self.generated_length);
            bytes.push(LOWER[rng.gen_range(0..LOWER.len())]);
            bytes.push(UPPER[rng.gen_range(0..UPPER.len())]);
            bytes.push(DIGITS[rng.gen_range(0..DIGITS.len())]);
            bytes.push(SPECIAL[rng.gen_range(0..SPECIAL.len())]);
            while bytes.len() < self.generated_length {
                bytes.push(ALL[rng.gen_range(0..ALL.len())]);
            }
            bytes.shuffle(&mut rng);
            let password = String::from_utf8(bytes).expect("password alphabet is ASCII");
            if self.validate(&password).is_ok() {
                return Ok(password);
            }
        }
        Err("无法按 CANGLING_PASSWORD_REGEX 自动生成密码，请使用 -p 指定符合策略的密码".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_requires_case_special_and_length() {
        let policy = PasswordPolicy::new(DEFAULT_REGEX, DEFAULT_HINT.into(), 20).unwrap();
        assert!(policy.validate("Good-password-12").is_ok());
        assert!(policy.validate("good-password-12").is_err());
        assert!(policy.validate("GOOD-PASSWORD-12").is_err());
        assert!(policy.validate("GoodPassword12").is_err());
        assert!(policy.validate("Aa-short!").is_ok());
        assert!(policy.validate("Aa-shr!").is_err());
    }

    #[test]
    fn generated_password_matches_default_policy() {
        let policy = PasswordPolicy::new(DEFAULT_REGEX, DEFAULT_HINT.into(), 24).unwrap();
        for _ in 0..32 {
            let password = policy.generate().unwrap();
            assert_eq!(password.len(), 24);
            assert!(policy.validate(&password).is_ok());
        }
    }

    #[test]
    fn custom_policy_is_enforced() {
        let policy = PasswordPolicy::new(r"^X[0-9]{11}$", "custom".into(), 12).unwrap();
        assert!(policy.validate("X12345678901").is_ok());
        assert_eq!(policy.validate("Y12345678901").unwrap_err(), "custom");
    }
}
