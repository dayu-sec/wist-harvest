//! 阿里云 RPC 签名（`SignatureVersion=1.0` + `HMAC-SHA256`）。
//!
//! 规范（阿里云 RPC 风格 API）：
//!
//! ```text
//! CanonicalizedQueryString =
//!   所有参数（不含 Signature）按参数名排序 → `percentEncode(k)=percentEncode(v)` → `&` 连接
//!
//! StringToSign =
//!   HTTPMethod + "&" + percentEncode("/") + "&" + percentEncode(CanonicalizedQueryString)
//!
//! Signature = Base64(HMAC-SHA256(AccessKeySecret + "&", StringToSign))
//! ```
//!
//! `percentEncode` 是 RFC3986：除 `A-Z a-z 0-9 - _ . ~` 外全部编码，空格编成 `%20`
//! （不是 `+`）。签完名后 `Signature` 本身也要 percentEncode 再放进查询串。

use std::collections::BTreeMap;

use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const SIGNATURE_VERSION: &str = "1.0";
pub const SIGNATURE_METHOD: &str = "HMAC-SHA256";

fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~')
}

/// RFC3986 percent-encode（阿里云签名要求）。
pub fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        if is_unreserved(byte) {
            out.push(byte as char);
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// 排序 + 编码后的查询串（不含 `Signature`）。
pub fn canonicalized_query(params: &BTreeMap<String, String>) -> String {
    let mut pairs: Vec<(&String, &String)> = params.iter().collect();
    // BTreeMap 已经按 key 排序；显式再排一次，避免以后换成别的容器时静默改变语义。
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .into_iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn string_to_sign(method: &str, canonicalized: &str) -> String {
    format!(
        "{method}&{}&{}",
        percent_encode("/"),
        percent_encode(canonicalized)
    )
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 支持任意长度密钥");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// 用 AccessKeySecret 签名；返回 Base64 文本。
pub fn sign(secret: &str, string_to_sign: &str) -> String {
    let key = format!("{secret}&");
    let digest = hmac_sha256(key.as_bytes(), string_to_sign.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(digest)
}

/// 拼出最终请求的查询串（含签名）。
pub fn signed_query(secret: &str, params: &BTreeMap<String, String>) -> String {
    let canonical = canonicalized_query(params);
    let sts = string_to_sign("GET", &canonical);
    let signature = sign(secret, &sts);
    format!("{canonical}&Signature={}", percent_encode(&signature))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定向量。
    ///
    /// 期望值由独立的 Python 实现（`hmac` + `hashlib` + `urllib.parse.quote`）算出来，
    /// 再抄进测试；不是拿本实现自己算一遍再断言自己，否则签名写错也测不出来。
    fn vector() -> BTreeMap<String, String> {
        [
            ("AccessKeyId", "AKIDEXAMPLE"),
            ("Action", "DescribeInstances"),
            ("Format", "JSON"),
            ("PageNumber", "1"),
            ("PageSize", "100"),
            ("RegionId", "cn-hangzhou"),
            ("SignatureMethod", "HMAC-SHA256"),
            ("SignatureNonce", "fixed-nonce-0001"),
            ("SignatureVersion", "1.0"),
            ("Timestamp", "2025-10-09T08:53:20Z"),
            ("Version", "2014-05-26"),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
    }

    #[test]
    fn fixed_vector() {
        let params = vector();
        let canonical = canonicalized_query(&params);
        assert_eq!(
            canonical,
            "AccessKeyId=AKIDEXAMPLE&Action=DescribeInstances&Format=JSON&PageNumber=1&PageSize=100&RegionId=cn-hangzhou&SignatureMethod=HMAC-SHA256&SignatureNonce=fixed-nonce-0001&SignatureVersion=1.0&Timestamp=2025-10-09T08%3A53%3A20Z&Version=2014-05-26"
        );

        let sts = string_to_sign("GET", &canonical);
        assert_eq!(
            sts,
            "GET&%2F&AccessKeyId%3DAKIDEXAMPLE%26Action%3DDescribeInstances%26Format%3DJSON%26PageNumber%3D1%26PageSize%3D100%26RegionId%3Dcn-hangzhou%26SignatureMethod%3DHMAC-SHA256%26SignatureNonce%3Dfixed-nonce-0001%26SignatureVersion%3D1.0%26Timestamp%3D2025-10-09T08%253A53%253A20Z%26Version%3D2014-05-26"
        );

        assert_eq!(
            sign("SECRETKEYEXAMPLE", &sts),
            "4cjZiihUuFwJN3gaVMnTVA/akMDNCTNbhcbzeBUHpMI="
        );

        let query = signed_query("SECRETKEYEXAMPLE", &params);
        assert!(
            query.ends_with("&Signature=4cjZiihUuFwJN3gaVMnTVA%2FakMDNCTNbhcbzeBUHpMI%3D"),
            "{query}"
        );
    }

    #[test]
    fn percent_encoding_follows_rfc3986() {
        assert_eq!(percent_encode("a b/c:10"), "a%20b%2Fc%3A10");
        assert_eq!(percent_encode("a-_.~"), "a-_.~");
        assert_eq!(percent_encode("中文"), "%E4%B8%AD%E6%96%87");
    }
}
