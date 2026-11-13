//! AWS Signature Version 4（`AWS4-HMAC-SHA256`）。
//!
//! 参与签名的头固定为 `content-type` / `host` / `x-amz-date`（临时凭据再加
//! `x-amz-security-token`）。请求体是 `application/x-www-form-urlencoded`，
//! 签名覆盖**实际发送的字节**，所以发送时必须原样写回这里用的 `payload`。
//!
//! 为什么不做成配置：签名是一段有严格规范的算法，写错不会报错，只会在云端
//! 得到一个 `SignatureDoesNotMatch`，或者更糟——在 mock 里全绿、真实账号全红。

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const ALGORITHM: &str = "AWS4-HMAC-SHA256";
pub const CONTENT_TYPE: &str = "application/x-www-form-urlencoded; charset=utf-8";

pub struct SigV4Request<'a> {
    pub access_key_id: &'a str,
    pub secret_access_key: &'a str,
    /// 临时凭据（STS）才有；参与签名。
    pub session_token: Option<&'a str>,
    pub region: &'a str,
    /// `ec2` / `sts`。
    pub service: &'a str,
    /// 实际请求的 host（`endpoint_override` 生效时是 override 的 authority）。
    pub host: &'a str,
    /// `yyyyMMddTHHmmssZ`（UTC）。
    pub amz_date: &'a str,
    /// `yyyyMMdd`（UTC）。
    pub date: &'a str,
    /// 与实际发送的字节完全一致的请求体。
    pub payload: &'a [u8],
}

#[derive(Debug, Clone)]
pub struct SignedRequest {
    pub authorization: String,
    pub canonical_request: String,
    pub string_to_sign: String,
    pub signature: String,
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

/// RFC 3986 百分号编码（只保留 unreserved）。
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*byte as char);
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// 把参数编成 `a=1&b=2`（按传入顺序；签名与发送用同一份字节即可）。
pub fn form_encode(params: &[(String, String)]) -> String {
    params
        .iter()
        .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn sign(req: &SigV4Request<'_>) -> SignedRequest {
    let mut canonical_headers = format!(
        "content-type:{CONTENT_TYPE}\nhost:{}\nx-amz-date:{}\n",
        req.host, req.amz_date
    );
    let mut signed_headers = "content-type;host;x-amz-date".to_string();
    if let Some(token) = req.session_token {
        canonical_headers.push_str(&format!("x-amz-security-token:{token}\n"));
        signed_headers.push_str(";x-amz-security-token");
    }

    let hashed_payload = sha256_hex(req.payload);
    let canonical_request =
        format!("POST\n/\n\n{canonical_headers}\n{signed_headers}\n{hashed_payload}");

    let credential_scope = format!("{}/{}/{}/aws4_request", req.date, req.region, req.service);
    let string_to_sign = format!(
        "{ALGORITHM}\n{}\n{credential_scope}\n{}",
        req.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );

    let secret_date = hmac_sha256(
        format!("AWS4{}", req.secret_access_key).as_bytes(),
        req.date.as_bytes(),
    );
    let secret_region = hmac_sha256(&secret_date, req.region.as_bytes());
    let secret_service = hmac_sha256(&secret_region, req.service.as_bytes());
    let secret_signing = hmac_sha256(&secret_service, b"aws4_request");
    let signature = hex::encode(hmac_sha256(&secret_signing, string_to_sign.as_bytes()));

    let authorization = format!(
        "{ALGORITHM} Credential={}/{credential_scope}, SignedHeaders={signed_headers}, Signature={signature}",
        req.access_key_id
    );

    SignedRequest {
        authorization,
        canonical_request,
        string_to_sign,
        signature,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 固定向量。期望值由**独立的 Python 实现**（hashlib/hmac）算出，
    /// 不是拿本实现自己算一遍再断言自己。
    ///
    /// 输入：akid=AKIDEXAMPLEACCESSKEY / skey=SECRETKEYEXAMPLE0001 /
    /// region=us-east-1 / service=ec2 / host=ec2.us-east-1.amazonaws.com /
    /// amz_date=20261113T000000Z / payload=`Action=DescribeRegions&Version=2016-11-15`
    #[test]
    fn fixed_vector() {
        let payload = b"Action=DescribeRegions&Version=2016-11-15";
        let signed = sign(&SigV4Request {
            access_key_id: "AKIDEXAMPLEACCESSKEY",
            secret_access_key: "SECRETKEYEXAMPLE0001",
            session_token: None,
            region: "us-east-1",
            service: "ec2",
            host: "ec2.us-east-1.amazonaws.com",
            amz_date: "20261113T000000Z",
            date: "20261113",
            payload,
        });

        assert_eq!(
            sha256_hex(payload),
            "903451d1f3cda66f6ac7101b5810af53eb3ce6b47c5162b0b2238ce9086b6a0d"
        );
        assert_eq!(
            signed.canonical_request,
            "POST\n/\n\ncontent-type:application/x-www-form-urlencoded; charset=utf-8\nhost:ec2.us-east-1.amazonaws.com\nx-amz-date:20261113T000000Z\n\ncontent-type;host;x-amz-date\n903451d1f3cda66f6ac7101b5810af53eb3ce6b47c5162b0b2238ce9086b6a0d"
        );
        assert_eq!(
            signed.signature,
            "87e8803c94450df21d49defdc5ca675292f07bdb19ed1c4d952e1be2d0caaa49"
        );
        assert_eq!(
            signed.authorization,
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLEACCESSKEY/20261113/us-east-1/ec2/aws4_request, SignedHeaders=content-type;host;x-amz-date, Signature=87e8803c94450df21d49defdc5ca675292f07bdb19ed1c4d952e1be2d0caaa49"
        );
    }

    #[test]
    fn session_token_is_in_signed_headers() {
        let signed = sign(&SigV4Request {
            access_key_id: "id",
            secret_access_key: "key",
            session_token: Some("token"),
            region: "us-east-1",
            service: "sts",
            host: "sts.us-east-1.amazonaws.com",
            amz_date: "20261113T000000Z",
            date: "20261113",
            payload: b"",
        });
        assert!(
            signed
                .canonical_request
                .contains("x-amz-security-token:token")
        );
        assert!(
            signed
                .authorization
                .contains("SignedHeaders=content-type;host;x-amz-date;x-amz-security-token")
        );
    }

    #[test]
    fn payload_change_changes_the_signature() {
        let base = SigV4Request {
            access_key_id: "id",
            secret_access_key: "key",
            session_token: None,
            region: "us-east-1",
            service: "ec2",
            host: "ec2.us-east-1.amazonaws.com",
            amz_date: "20261113T000000Z",
            date: "20261113",
            payload: b"Action=DescribeVpcs",
        };
        let a = sign(&base).signature;
        let b = sign(&SigV4Request {
            payload: b"Action=DescribeSubnets",
            ..base
        })
        .signature;
        assert_ne!(a, b);
    }

    #[test]
    fn form_encoding_is_rfc3986() {
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
        assert_eq!(
            form_encode(&[
                ("Action".into(), "DescribeInstances".into()),
                ("Filter.1.Value.1".into(), "a b".into()),
            ]),
            "Action=DescribeInstances&Filter.1.Value.1=a%20b"
        );
    }
}
