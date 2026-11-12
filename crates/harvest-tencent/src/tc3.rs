//! TC3-HMAC-SHA256 签名。
//!
//! 规范要点（腾讯云签名方法 v3）：
//!
//! ```text
//! CanonicalRequest =
//!   HTTPMethod + "\n" + CanonicalURI + "\n" + CanonicalQueryString + "\n" +
//!   CanonicalHeaders + "\n" + SignedHeaders + "\n" + HashedRequestPayload
//!
//! StringToSign =
//!   "TC3-HMAC-SHA256" + "\n" + Timestamp + "\n" + CredentialScope + "\n" +
//!   SHA256(CanonicalRequest)
//!
//! Signature = HMAC(HMAC(HMAC("TC3"+SecretKey, Date), Service), "tc3_request", StringToSign)
//! ```
//!
//! 参与签名的头固定为 `content-type;host;x-tc-action`；`x-tc-timestamp` 等作为
//! 普通头随请求发送，不进 `SignedHeaders`。

use chrono::{TimeZone, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "TC3-HMAC-SHA256";
pub const CONTENT_TYPE: &str = "application/json; charset=utf-8";
pub const SIGNED_HEADERS: &str = "content-type;host;x-tc-action";

pub struct Tc3Request<'a> {
    pub secret_id: &'a str,
    pub secret_key: &'a str,
    /// 服务名，例如 `cvm` / `vpc` / `tag`。
    pub service: &'a str,
    /// 实际请求的 host（`endpoint_override` 生效时是 override 的 authority）。
    pub host: &'a str,
    /// 动作名，例如 `DescribeInstances`（签名时用小写）。
    pub action: &'a str,
    /// UNIX 秒。
    pub timestamp: i64,
    /// **与实际发送的字节完全一致**的请求体。
    pub payload: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct SignedRequest {
    pub authorization: String,
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
    pub date: String,
}

pub fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

fn hmac_sha256(key: &[u8], message: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC 支持任意长度密钥");
    mac.update(message);
    mac.finalize().into_bytes().to_vec()
}

/// 时间戳 → UTC 日期（签名里的 `Date`）。
pub fn utc_date(timestamp: i64) -> String {
    Utc.timestamp_opt(timestamp, 0)
        .single()
        .unwrap_or_else(Utc::now)
        .format("%Y-%m-%d")
        .to_string()
}

pub fn sign(req: &Tc3Request<'_>) -> SignedRequest {
    let date = utc_date(req.timestamp);
    let canonical_headers = format!(
        "content-type:{CONTENT_TYPE}\nhost:{}\nx-tc-action:{}\n",
        req.host,
        req.action.to_lowercase()
    );
    let hashed_payload = sha256_hex(req.payload);
    let canonical_request =
        format!("POST\n/\n\n{canonical_headers}\n{SIGNED_HEADERS}\n{hashed_payload}");

    let credential_scope = format!("{date}/{}/tc3_request", req.service);
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{credential_scope}\n{}",
        req.timestamp,
        sha256_hex(canonical_request.as_bytes())
    );

    let secret_date = hmac_sha256(format!("TC3{}", req.secret_key).as_bytes(), date.as_bytes());
    let secret_service = hmac_sha256(&secret_date, req.service.as_bytes());
    let secret_signing = hmac_sha256(&secret_service, b"tc3_request");
    let signature = hex::encode(hmac_sha256(&secret_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "{ALGORITHM} Credential={}/{credential_scope}, SignedHeaders={SIGNED_HEADERS}, Signature={signature}",
        req.secret_id
    );

    SignedRequest {
        authorization,
        canonical_request,
        string_to_sign,
        signature,
        date,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定向量。
    ///
    /// 期望值由**独立的 Python 实现**（hashlib/hmac）算出来，再抄进测试；
    /// 不是拿本实现自己算一遍再断言自己，否则签名写错也测不出来。
    /// 输入：sid=AKIDEXAMPLEsecretid0001 / skey=SECRETKEYEXAMPLE0001 /
    /// service=cvm / host=cvm.tencentcloudapi.com / action=DescribeInstances /
    /// ts=1760000000 / payload={"Limit":1,"Offset":0}
    #[test]
    fn fixed_vector() {
        let payload = br#"{"Limit":1,"Offset":0}"#;
        let signed = sign(&Tc3Request {
            secret_id: "AKIDEXAMPLEsecretid0001",
            secret_key: "SECRETKEYEXAMPLE0001",
            service: "cvm",
            host: "cvm.tencentcloudapi.com",
            action: "DescribeInstances",
            timestamp: 1_760_000_000,
            payload,
        });

        assert_eq!(signed.date, "2025-10-09");
        assert_eq!(
            sha256_hex(payload),
            "664100f264daf37deefde55f8b8f8dfe1bdf7f118a83d0254fbf44747163f464"
        );
        assert_eq!(
            signed.canonical_request,
            "POST\n/\n\ncontent-type:application/json; charset=utf-8\nhost:cvm.tencentcloudapi.com\nx-tc-action:describeinstances\n\ncontent-type;host;x-tc-action\n664100f264daf37deefde55f8b8f8dfe1bdf7f118a83d0254fbf44747163f464"
        );
        assert_eq!(
            signed.string_to_sign,
            "TC3-HMAC-SHA256\n1760000000\n2025-10-09/cvm/tc3_request\nce5f58f6ff45693c4c2b8babfb8bc02239edd665e56777cc46045945fd02112f"
        );
        assert_eq!(
            signed.signature,
            "d83d41c53f1bc036e5055059f202d0ca174506c5c4bcf8f7c6a7befe5a1163c0"
        );
        assert_eq!(
            signed.authorization,
            "TC3-HMAC-SHA256 Credential=AKIDEXAMPLEsecretid0001/2025-10-09/cvm/tc3_request, SignedHeaders=content-type;host;x-tc-action, Signature=d83d41c53f1bc036e5055059f202d0ca174506c5c4bcf8f7c6a7befe5a1163c0"
        );
    }

    #[test]
    fn action_is_lowercased_in_canonical_headers() {
        let signed = sign(&Tc3Request {
            secret_id: "id",
            secret_key: "key",
            service: "vpc",
            host: "vpc.tencentcloudapi.com",
            action: "DescribeVpcs",
            timestamp: 1_760_000_000,
            payload: b"{}",
        });
        assert!(
            signed
                .canonical_request
                .contains("x-tc-action:describevpcs")
        );
    }

    #[test]
    fn payload_change_changes_signature() {
        let base = Tc3Request {
            secret_id: "id",
            secret_key: "key",
            service: "cvm",
            host: "cvm.tencentcloudapi.com",
            action: "DescribeInstances",
            timestamp: 1_760_000_000,
            payload: b"{}",
        };
        let a = sign(&base).signature;
        let b = sign(&Tc3Request {
            payload: b"{\"Limit\":1}",
            ..base
        })
        .signature;
        assert_ne!(a, b);
    }
}
