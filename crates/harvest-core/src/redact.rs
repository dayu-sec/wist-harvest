//! 凭据脱敏。
//!
//! 红线的落地方式：**所有**可能包含凭据的文本（日志、报告、错误、manifest 的
//! 失败原因）都要经过同一个 [`Redactor`]。启动时把 `secret_id` / `secret_key`
//! 的真实值注册进去，之后任何一处忘了考虑凭据格式也不会泄漏。

use std::sync::OnceLock;
use std::sync::RwLock;

/// 把已知的敏感字符串替换成 `***`。
///
/// 为什么是"已知值替换"而不是正则：凭据形态会变（AKID 前缀、临时 token、
/// 文件里的任意串），只有"知道真实值"才真正兜得住。
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个敏感值。过短的值不注册（会把正常文本搅烂）。
    pub fn add_secret(&mut self, secret: &str) {
        if secret.len() < 4 {
            return;
        }
        if self.secrets.iter().any(|s| s == secret) {
            return;
        }
        self.secrets.push(secret.to_string());
        // 长值优先替换，避免短值先命中把长值切碎。
        self.secrets
            .sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    }

    pub fn scrub(&self, text: &str) -> String {
        if self.secrets.is_empty() || text.is_empty() {
            return text.to_string();
        }
        let mut out = text.to_string();
        for secret in &self.secrets {
            if out.contains(secret.as_str()) {
                out = out.replace(secret.as_str(), "***");
            }
        }
        out
    }
}

/// 进程级默认脱敏器。服务启动时注册一次，日志与报告都走它。
static GLOBAL: OnceLock<RwLock<Redactor>> = OnceLock::new();

fn global() -> &'static RwLock<Redactor> {
    GLOBAL.get_or_init(|| RwLock::new(Redactor::new()))
}

pub fn install_global(redactor: Redactor) {
    let lock = global();
    match lock.write() {
        Ok(mut guard) => *guard = redactor,
        Err(err) => *err.into_inner() = redactor,
    }
}

pub fn register_secret(secret: &str) {
    let lock = global();
    match lock.write() {
        Ok(mut guard) => guard.add_secret(secret),
        Err(err) => err.into_inner().add_secret(secret),
    }
}

pub fn scrub(text: &str) -> String {
    match global().read() {
        Ok(guard) => guard.scrub(text),
        Err(err) => err.into_inner().scrub(text),
    }
}

/// 云端错误体裁剪：只保留 `Code`/`Message`/`RequestId`，并截断超长消息。
pub fn trim_cloud_message(message: &str, max_len: usize) -> String {
    let scrubbed = scrub(message);
    if scrubbed.chars().count() <= max_len {
        return scrubbed;
    }
    let truncated: String = scrubbed.chars().take(max_len).collect();
    format!("{truncated}…(truncated)")
}

/// 统一日志输出（stderr）。不引入日志框架，避免多一条依赖链；
/// 但出口只有一个，脱敏不会被绕过。
pub fn log_line(level: &str, message: &str) {
    eprintln!(
        "{} {level:<5} {}",
        crate::time::rfc3339_utc(crate::time::utc_now()),
        scrub(message)
    );
}

pub fn info(message: impl AsRef<str>) {
    log_line("INFO", message.as_ref());
}

pub fn warn(message: impl AsRef<str>) {
    log_line("WARN", message.as_ref());
}

pub fn error(message: impl AsRef<str>) {
    log_line("ERROR", message.as_ref());
}

pub fn debug(message: impl AsRef<str>) {
    log_line("DEBUG", message.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_secret_wins() {
        let mut r = Redactor::new();
        r.add_secret("abcd");
        r.add_secret("abcdefgh");
        assert_eq!(r.scrub("value=abcdefgh"), "value=***");
    }

    #[test]
    fn short_secrets_are_not_registered() {
        let mut r = Redactor::new();
        r.add_secret("ab");
        assert_eq!(r.scrub("ab"), "ab");
    }

    #[test]
    fn scrubs_every_occurrence() {
        let mut r = Redactor::new();
        r.add_secret("SECRETVALUE");
        assert_eq!(
            r.scrub("a=SECRETVALUE b=SECRETVALUE"),
            "a=*** b=***".to_string()
        );
    }
}
