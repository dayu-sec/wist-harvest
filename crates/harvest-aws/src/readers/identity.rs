//! 账号身份：`sts:GetCallerIdentity`。
//!
//! 拿不到账号就不声明 `coverage.account`（下游因此不退役），并打 WARN。
//! 账号键是 `aws:<12 位账号号>`。

use serde::Deserialize;

use super::{parse_xml, text};
use crate::client::GET_CALLER_IDENTITY;

#[derive(Debug, Default, Deserialize)]
pub struct GetCallerIdentityResponse {
    #[serde(rename = "GetCallerIdentityResult", default)]
    result: Option<CallerIdentityResult>,
}

#[derive(Debug, Default, Deserialize)]
struct CallerIdentityResult {
    #[serde(rename = "Account", default)]
    account: Option<String>,
}

/// 成功返回账号号（`123456789012`）；解析不到返回 `None`。
pub fn map_account(xml: &str) -> Option<String> {
    let parsed: GetCallerIdentityResponse = parse_xml(xml, GET_CALLER_IDENTITY.name).ok()?;
    let account = text(parsed.result?.account);
    if account.is_empty() {
        None
    } else {
        Some(account)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_the_account_id() {
        let xml = r#"<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <GetCallerIdentityResult>
    <Arn>arn:aws:iam::123456789012:user/harvest</Arn>
    <UserId>AIDAEXAMPLE</UserId>
    <Account>123456789012</Account>
  </GetCallerIdentityResult>
  <ResponseMetadata><RequestId>req-1</RequestId></ResponseMetadata>
</GetCallerIdentityResponse>"#;
        assert_eq!(map_account(xml).as_deref(), Some("123456789012"));
    }

    #[test]
    fn missing_account_is_none_not_a_guessed_value() {
        let xml =
            "<GetCallerIdentityResponse><GetCallerIdentityResult/></GetCallerIdentityResponse>";
        assert_eq!(map_account(xml), None);
    }
}
