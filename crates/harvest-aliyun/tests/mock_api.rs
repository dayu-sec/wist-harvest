//! 端到端：真 `AliyunSource` + 假云 API（本地 HTTP 服务器）+ 记录型 sink。
//!
//! 覆盖四个容易做错的点：
//!
//! 1. RPC 请求必须带签名与 `SignatureMethod=HMAC-SHA256`（不是裸查询串）；
//! 2. `DescribeVpcs` / `DescribeVSwitches` 的 `PageSize` 上限是 50，
//!    配置写 100 也必须夹到 50；
//! 3. 分页总数不一致 → `Aborted` 且数据平面零帧；
//! 4. `regions="auto"` + 四类资产全采才允许声明账号级覆盖。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use harvest_aliyun::AliyunSource;
use harvest_aliyun::client::{AliyunClient, ClientOptions};
use harvest_core::config::{SecretString, ServiceConfig, SourceConfig};
use harvest_core::cost::{CostBasis, CostRange};
use harvest_core::error::HarvestError;
use harvest_core::model::Vendor;
use harvest_core::ratelimit::{RateLimiter, RateSpec};
use harvest_core::run::{RunOptions, run_once};
use harvest_core::source::{CompleteSnapshot, PublishAck, Sink, Source};
use harvest_core::state::RunState;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// 假云 API
// ---------------------------------------------------------------------------

type Handler = Arc<dyn Fn(&str, &Value) -> Value + Send + Sync>;

#[derive(Debug, Clone)]
struct RecordedRequest {
    action: String,
    params: Value,
}

struct MockApi {
    base: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockApi {
    async fn spawn(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("绑定本地端口");
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let reqs = Arc::clone(&requests);
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let handler = Arc::clone(&handler);
                let reqs = Arc::clone(&reqs);
                tokio::spawn(async move {
                    let _ = serve_connection(stream, handler, reqs).await;
                });
            }
        });
        Self {
            base: format!("http://{addr}"),
            requests,
        }
    }

    fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    handler: Handler,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
) -> std::io::Result<()> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(pos) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");
    let query = target.split_once('?').map(|(_, query)| query).unwrap_or("");

    let mut params = serde_json::Map::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key), Value::String(percent_decode(value)));
    }
    let action = params
        .get("Action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = Value::Object(params);
    requests.lock().unwrap().push(RecordedRequest {
        action: action.clone(),
        params: params.clone(),
    });

    let response = handler(&action, &params);
    let payload = serde_json::to_vec(&response).unwrap();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// ---------------------------------------------------------------------------
// 场景数据
// ---------------------------------------------------------------------------

fn instance(id: &str) -> Value {
    json!({
        "InstanceId": id,
        "InstanceName": format!("{id}-name"),
        "Status": "Running",
        "VpcAttributes": {
            "VpcId": "vpc-1",
            "VSwitchId": "vsw-1",
            "PrivateIpAddress": {"IpAddress": ["10.0.1.10"]}
        },
        "PublicIpAddress": {"IpAddress": []},
        "Cpu": 2,
        "Memory": 4096,
        "OSName": "Alibaba Cloud Linux 3",
        "InstanceType": "ecs.g6.large",
        "InstanceChargeType": "PostPaid",
        "CreationTime": "2026-11-01T02:00:00Z",
        "ZoneId": "cn-hangzhou-h",
        "Tags": {"Tag": [{"TagKey": "App", "TagValue": "order"}]}
    })
}

fn eip(id: &str) -> Value {
    json!({
        "AllocationId": id,
        "Name": format!("{id}-name"),
        "IpAddress": "203.0.113.10",
        "Status": "InUse",
        "InstanceType": "EcsInstance",
        "InstanceId": "i-1",
        "ISP": "BGP",
        "Bandwidth": 10,
        "InternetChargeType": "PayByTraffic"
    })
}

fn inventory_handler(action: &str, params: &Value) -> Value {
    match action {
        "DescribeRegions" => json!({
            "RequestId": "req-regions",
            "Regions": {"Region": [{"RegionId": "cn-hangzhou", "LocalName": "华东1（杭州）"}]}
        }),
        "GetCallerIdentity" => json!({
            "RequestId": "req-caller",
            "AccountId": "1234567890",
            "Arn": "acs:ram::1234567890:user/harvest",
            "UserId": "2000000001"
        }),
        "DescribeInstances" => {
            let mut instances = vec![instance("i-1"), instance("i-2"), instance("i-3")];
            // 三态：i-1 明确 null、i-2 空数组、i-3 字段缺席。
            instances[0]["Tags"] = Value::Null;
            instances[1]["Tags"] = json!({"Tag": []});
            instances[2].as_object_mut().unwrap().remove("Tags");
            json!({
                "RequestId": "req-instances",
                "TotalCount": 3,
                "PageNumber": params["PageNumber"],
                "PageSize": params["PageSize"],
                "Instances": {"Instance": instances}
            })
        }
        "DescribeEipAddresses" => json!({
            "RequestId": "req-eips",
            "TotalCount": 2,
            "EipAddresses": {"EipAddress": [eip("eip-1"), eip("eip-2")]}
        }),
        "DescribeVpcs" => json!({
            "RequestId": "req-vpcs",
            "TotalCount": 1,
            "Vpcs": {"Vpc": [{
                "VpcId": "vpc-1",
                "VpcName": "production-vpc",
                "CidrBlock": "10.0.0.0/16",
                "CreationTime": "2026-10-01T10:00:00Z",
                "IsDefault": false,
                "Tags": {"Tag": [{"TagKey": "Environment", "TagValue": "production"}]}
            }]}
        }),
        "DescribeVSwitches" => json!({
            "RequestId": "req-vswitches",
            "TotalCount": 1,
            "VSwitches": {"VSwitch": [{
                "VSwitchId": "vsw-1",
                "VSwitchName": "zone-h",
                "VpcId": "vpc-1",
                "CidrBlock": "10.0.1.0/24",
                "ZoneId": "cn-hangzhou-h",
                "CreationTime": "2026-10-01T10:05:00Z",
                "IsDefault": false,
                "AvailableIpAddressCount": 240,
                "Tags": {"Tag": []}
            }]}
        }),
        other => json!({
            "RequestId": "req-unknown",
            "Code": "InvalidAction",
            "Message": format!("mock 未实现 {other}")
        }),
    }
}

// ---------------------------------------------------------------------------
// 测试脚手架
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingSink {
    frames: Mutex<Vec<CompleteSnapshot>>,
}

#[async_trait]
impl Sink for RecordingSink {
    async fn publish_snapshot(&self, snap: &CompleteSnapshot) -> Result<PublishAck, HarvestError> {
        self.frames.lock().unwrap().push(snap.clone());
        Ok(PublishAck {
            bytes: snap.bytes.len() as u64,
            ack: "ok".into(),
            duration_ms: 1,
        })
    }
}

fn write_secret(dir: &Path, name: &str, value: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, value).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    path
}

fn service_config(spool_dir: &Path, state_dir: &Path) -> ServiceConfig {
    ServiceConfig::parse(&format!(
        r#"
[service]
spool_dir = "{spool}"
state_dir = "{state}"
page_size = 100

[rate]
default_qps = 1000.0
default_burst = 1000.0

[sink]
endpoint = "127.0.0.1:1"
allow_plaintext = true
"#,
        spool = spool_dir.display(),
        state = state_dir.display()
    ))
    .unwrap()
}

fn source_config(dir: &Path, base: &str) -> SourceConfig {
    source_config_with_regions(dir, base, r#""auto""#, "")
}

/// `regions` 直接拼进 TOML：`["cn-hangzhou"]` 或 `"auto"`。
fn source_config_with_regions(dir: &Path, base: &str, regions: &str, assets: &str) -> SourceConfig {
    let access_key_id = write_secret(dir, "id.secret", "ALIYUNTESTACCESSKEYID");
    let access_key_secret = write_secret(dir, "key.secret", "ALIYUNTESTSECRETVALUE");
    SourceConfig::parse(&format!(
        r#"
source_code = "aliyun-prod"
vendor = "ALIYUN"
group = "inventory"
env_ref = "prod"
regions = {regions}
endpoint_override = "{base}"
{assets}
[credential]
access_key_id = {{ file = "{id}" }}
access_key_secret = {{ file = "{key}" }}
"#,
        id = access_key_id.display(),
        key = access_key_secret.display()
    ))
    .unwrap()
}

fn body_of(frame: &CompleteSnapshot) -> Value {
    let symbol = harvest_core::wire::WIRE_SYMBOL.as_bytes();
    let at = frame
        .bytes
        .windows(symbol.len())
        .position(|w| w == symbol)
        .expect("记录里必须有 DSNAP: 符号");
    serde_json::from_slice(&frame.bytes[at + symbol.len()..]).expect("正文是 JSON")
}

fn cost_client(base: &str) -> AliyunClient {
    AliyunClient::new(
        SecretString::new("ALIYUNTESTACCESSKEYID"),
        SecretString::new("ALIYUNTESTSECRETVALUE"),
        Arc::new(RateLimiter::new(RateSpec::default(), Default::default())),
        ClientOptions {
            endpoint_override: Some(base.to_string()),
            ..Default::default()
        },
    )
    .expect("client")
}

fn daily_range() -> CostRange {
    let day = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).expect("date");
    CostRange {
        source_code: "aliyun-prod".into(),
        vendor: Vendor::Aliyun,
        billing_account_uin: "1234567890".into(),
        cost_basis: CostBasis::Consumption,
        period_start: day,
        period_end: day,
    }
}

fn bill_row(instance: &str) -> Value {
    json!({
        "BillAccountID": "1234567890",
        "ProductCode": "ecs",
        "ProductName": "云服务器ECS",
        "BillingItem": "云服务器ECS按量付费",
        "InstanceID": instance,
        "BillingDate": "2026-10-15",
        "PretaxAmount": "1.00",
        "Currency": "CNY"
    })
}

#[tokio::test]
async fn cost_fetch_reads_every_row() {
    let handler: Handler = Arc::new(|action: &str, _params: &Value| {
        assert_eq!(action, "QueryAccountBill");
        json!({
            "RequestId": "req-bill",
            "Success": true,
            "Code": "Success",
            "Data": {
                "BillingCycle": "2026-10",
                "TotalCount": 2,
                "PageNum": 1,
                "PageSize": 300,
                "Items": {"Item": [bill_row("i-1"), bill_row("i-2")]}
            }
        })
    });
    let mock = MockApi::spawn(handler).await;
    let client = cost_client(&mock.base);
    let mut seen = 0u64;
    let stat = harvest_aliyun::readers::billing::fetch_range(&client, &daily_range(), |_row| {
        seen += 1;
        Ok(())
    })
    .await
    .expect("成本抓取");
    assert_eq!(stat.rows, 2);
    assert_eq!(seen, 2);
    assert_eq!(stat.pages, 1);
}

#[tokio::test]
async fn cost_short_page_before_total_is_an_integrity_error() {
    // PageNum 分页：非末页短页会让下一页跳行，必须中止而不是少账。
    let handler: Handler = Arc::new(|_action: &str, _params: &Value| {
        json!({
            "RequestId": "req-bill-short",
            "Success": true,
            "Code": "Success",
            "Data": {
                "BillingCycle": "2026-10",
                "TotalCount": 5,
                "Items": {"Item": [bill_row("i-1"), bill_row("i-2")]}
            }
        })
    });
    let mock = MockApi::spawn(handler).await;
    let client = cost_client(&mock.base);
    let err = harvest_aliyun::readers::billing::fetch_range(&client, &daily_range(), |_row| Ok(()))
        .await
        .expect_err("短页必须失败");
    assert_eq!(err.class, harvest_core::ErrorClass::Integrity);
    assert!(err.message.contains("短页"), "{}", err.message);
}

#[tokio::test]
async fn cost_extra_rows_beyond_total_is_an_integrity_error() {
    // 云端少报总数、实际多给行：同样不能发布（少报的行会永远对不上）。
    let handler: Handler = Arc::new(|_action: &str, _params: &Value| {
        json!({
            "RequestId": "req-bill-extra",
            "Success": true,
            "Code": "Success",
            "Data": {
                "BillingCycle": "2026-10",
                "TotalCount": 1,
                "Items": {"Item": [bill_row("i-1"), bill_row("i-2")]}
            }
        })
    });
    let mock = MockApi::spawn(handler).await;
    let client = cost_client(&mock.base);
    let err = harvest_aliyun::readers::billing::fetch_range(&client, &daily_range(), |_row| Ok(()))
        .await
        .expect_err("多出的行必须失败");
    assert_eq!(err.class, harvest_core::ErrorClass::Integrity);
    assert!(err.message.contains("总数不一致"), "{}", err.message);
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[tokio::test]
async fn inventory_group_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::spawn(Arc::new(inventory_handler)).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config(dir.path(), &mock.base);
    let source = AliyunSource::init(cfg, &svc).await.expect("数据源初始化");
    let scope = source.scope();
    let sink = RecordingSink::default();

    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(
        outcome.state,
        RunState::Done,
        "应跑通：{:?}",
        outcome.report.errors
    );

    let frames = sink.frames.lock().unwrap();
    assert_eq!(frames.len(), 1);
    let payload = body_of(&frames[0]);

    assert_eq!(payload["schema_native"], json!("wist.harvest.snapshot.v1"));
    assert_eq!(payload["vendor"], json!("ALIYUN"));
    assert_eq!(payload["source_code"], json!("aliyun-prod"));
    assert_eq!(payload["env_ref"], json!("prod"));
    assert_eq!(payload["regions"], json!(["cn-hangzhou"]));
    assert_eq!(payload["coverage"]["scope"], json!("account"));
    assert_eq!(payload["coverage"]["complete"], json!(true));
    assert_eq!(payload["coverage"]["account"], json!("aliyun:1234567890"));

    let hosts = payload["assets"]["CLOUD_HOST"].as_array().unwrap();
    assert_eq!(hosts.len(), 3);
    let host = hosts
        .iter()
        .find(|h| h["source_asset_id"] == "i-1")
        .unwrap();
    assert_eq!(host["ip"], json!("10.0.1.10"));
    assert_eq!(host["public_ip"], json!(""));
    assert_eq!(host["vpcid"], json!("vpc-1"));
    assert_eq!(host["subvpcid"], json!("vsw-1"));
    assert_eq!(host["memory_capacity"], json!(4));
    assert_eq!(host["ins_charge_type"], json!("PostPaid"));
    assert_eq!(host["ins_charge_type_name"], json!("按量付费"));
    assert_eq!(host["region_name"], json!("华东1（杭州）"));
    assert!(host["tags"].is_null(), "Tags:null 要保留 null");
    let host2 = hosts
        .iter()
        .find(|h| h["source_asset_id"] == "i-2")
        .unwrap();
    assert_eq!(host2["tags"], json!([]));
    let host3 = hosts
        .iter()
        .find(|h| h["source_asset_id"] == "i-3")
        .unwrap();
    assert!(
        host3.as_object().unwrap().get("tags").is_none(),
        "Tags 字段缺席时不输出"
    );

    let eips = payload["assets"]["EIP"].as_array().unwrap();
    assert_eq!(eips.len(), 2);
    assert_eq!(eips[0]["eip_address_type"], json!("EIP"));

    let vpcs = payload["assets"]["VPC"].as_array().unwrap();
    assert_eq!(vpcs.len(), 1);
    assert_eq!(vpcs[0]["parent_source_asset_id"], json!("-1"));
    assert_eq!(vpcs[0]["risk_level"], json!("MEDIUM"));
    assert_eq!(vpcs[0]["enable"], json!(1));

    let subnets = payload["assets"]["SUB_NET"].as_array().unwrap();
    assert_eq!(subnets.len(), 1);
    assert_eq!(subnets[0]["parent_source_asset_id"], json!("vpc-1"));
    assert_eq!(subnets[0]["zone"], json!("cn-hangzhou-h"));

    assert_eq!(payload["stats"]["CLOUD_HOST"]["total"], json!(3));
    assert_eq!(
        payload["stats"]["CLOUD_HOST"]["regions"]["cn-hangzhou"],
        json!(3)
    );

    let requests = mock.requests();
    // RPC 签名：每个请求都必须带 Signature，且用 HMAC-SHA256。
    assert!(
        requests.iter().all(|request| {
            request.params["SignatureMethod"] == json!("HMAC-SHA256")
                && request.params["Signature"]
                    .as_str()
                    .is_some_and(|v| !v.is_empty())
        }),
        "所有请求都必须签名"
    );

    // 各接口的 PageSize：ECS/EIP 用 100，VPC/VSwitch 必须夹到 50。
    let page_size = |action: &str| -> String {
        requests
            .iter()
            .find(|request| request.action == action)
            .map(|request| {
                request.params["PageSize"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default()
    };
    assert_eq!(page_size("DescribeInstances"), "100");
    assert_eq!(page_size("DescribeEipAddresses"), "100");
    assert_eq!(page_size("DescribeVpcs"), "50", "VPC 接口上限是 50");
    assert_eq!(
        page_size("DescribeVSwitches"),
        "50",
        "VSwitch 接口上限是 50"
    );
}

#[tokio::test]
async fn page_total_mismatch_aborts_and_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // 云端声称 5 台，只给 2 台：分页没抓完。
    let handler: Handler = Arc::new(|action: &str, params: &Value| {
        if action == "DescribeInstances" {
            let page = params["PageNumber"].as_str().unwrap_or("1");
            let instances = if page == "1" {
                json!([instance("i-1"), instance("i-2")])
            } else {
                json!([])
            };
            json!({
                "RequestId": "req-broken",
                "TotalCount": 5,
                "Instances": {"Instance": instances}
            })
        } else {
            inventory_handler(action, params)
        }
    });
    let mock = MockApi::spawn(handler).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config_with_regions(dir.path(), &mock.base, r#"["cn-hangzhou"]"#, "");
    let source = AliyunSource::init(cfg, &svc).await.expect("数据源初始化");
    let scope = source.scope();
    let sink = RecordingSink::default();

    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(outcome.state, RunState::Aborted);
    assert!(
        sink.frames.lock().unwrap().is_empty(),
        "Aborted 路径必须零帧"
    );
    assert!(
        outcome
            .report
            .abort_reason
            .as_deref()
            .unwrap_or_default()
            .contains("总数不一致"),
        "abort_reason={:?}",
        outcome.report.abort_reason
    );
    assert!(
        !mock
            .requests()
            .iter()
            .any(|request| request.action == "DescribeEipAddresses"),
        "第一个工作项失败后不该继续抓后面的类型"
    );
}

#[tokio::test]
async fn auth_error_is_not_retried_and_does_not_block_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let handler: Handler = Arc::new(|_action: &str, _params: &Value| {
        json!({
            "RequestId": "req-auth",
            "Success": false,
            "Code": "InvalidAccessKeyId.NotFound",
            "Message": "access key not found"
        })
    });
    let mock = MockApi::spawn(handler).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    // 显式地域：初始化只会调 GetCallerIdentity（失败不阻断）。
    let cfg = source_config_with_regions(dir.path(), &mock.base, r#"["cn-hangzhou"]"#, "");

    let source = AliyunSource::init(cfg, &svc)
        .await
        .expect("账号失败不阻断库存");
    assert_eq!(source.account_key(), None);

    let caller_calls = mock
        .requests()
        .iter()
        .filter(|request| request.action == "GetCallerIdentity")
        .count();
    assert_eq!(caller_calls, 1, "鉴权失败重试无用，必须只调一次");
}

#[tokio::test]
async fn account_lookup_failure_still_publishes_but_without_an_account_key() {
    let dir = tempfile::tempdir().unwrap();
    let handler: Handler = Arc::new(|action: &str, params: &Value| {
        if action == "GetCallerIdentity" {
            return json!({
                "RequestId": "req-deny",
                "Code": "Forbidden.RAM",
                "Message": "denied"
            });
        }
        inventory_handler(action, params)
    });
    let mock = MockApi::spawn(handler).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config(dir.path(), &mock.base);

    let source = AliyunSource::init(cfg, &svc).await.expect("账号失败不阻断");
    let scope = source.scope();
    let sink = RecordingSink::default();
    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(
        outcome.state,
        RunState::Done,
        "账号取不到只影响退役资格，不影响采集"
    );

    let frames = sink.frames.lock().unwrap();
    let payload = body_of(&frames[0]);
    assert_eq!(payload["coverage"]["scope"], json!("account"));
    assert!(
        payload["coverage"].get("account").is_none(),
        "取不到账号就不输出这个键，宁可不退役",
    );
}

#[tokio::test]
async fn disabling_a_type_downgrades_coverage_to_partial() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::spawn(Arc::new(inventory_handler)).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config_with_regions(
        dir.path(),
        &mock.base,
        r#""auto""#,
        "[assets]\neip = false\n",
    );

    let source = AliyunSource::init(cfg, &svc).await.expect("初始化");
    let scope = source.scope();
    let sink = RecordingSink::default();
    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(outcome.state, RunState::Done, "{:?}", outcome.report.errors);

    let frames = sink.frames.lock().unwrap();
    let payload = body_of(&frames[0]);
    assert_eq!(payload["assets"]["EIP"], json!([]));
    assert_eq!(payload["coverage"]["scope"], json!("partial"));
    assert_eq!(payload["coverage"]["complete"], json!(true));
    assert!(
        mock.requests()
            .iter()
            .all(|request| request.action != "DescribeEipAddresses"),
        "关掉的类型不应该再去调云 API"
    );
}
