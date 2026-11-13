//! 端到端：真 `TencentSource` + 假云 API（本地 HTTP 服务器）+ 记录型 sink。
//!
//! 覆盖三个容易做错的点：
//!
//! 1. EIP 每地域两路分页合并后按 `AddressId` 去重（不能相加 `TotalCount`）；
//! 2. `DescribeVpcs` / `DescribeSubnets` 的 `Limit`/`Offset` 用字符串形式；
//! 3. 分页总数不一致 → `Aborted` 且数据平面零帧。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use harvest_core::config::{ServiceConfig, SourceConfig};
use harvest_core::error::HarvestError;
use harvest_core::run::{RunOptions, run_once};
use harvest_core::source::{CompleteSnapshot, PublishAck, Sink, Source};
use harvest_core::state::RunState;
use harvest_tencent::TencentSource;
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
    body: Value,
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
    let mut content_length = 0usize;
    let mut action = String::new();
    for line in head.lines().skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "x-tc-action" => action = value.trim().to_string(),
                _ => {}
            }
        }
    }

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        body.extend_from_slice(&chunk[..read]);
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    requests.lock().unwrap().push(RecordedRequest {
        action: action.clone(),
        body: body.clone(),
    });

    let response = handler(&action, &body);
    let payload = serde_json::to_vec(&json!({ "Response": response })).unwrap();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// 场景数据
// ---------------------------------------------------------------------------

fn address(id: &str, kind: &str) -> Value {
    json!({
        "AddressId": id,
        "AddressName": format!("{id}-name"),
        "AddressType": kind,
        "AddressIp": "203.0.113.10",
        "AddressStatus": "BIND",
        "InstanceType": "CVM",
        "InstanceId": "ins-1",
        "InternetServiceProvider": "BGP",
        "Bandwidth": 10,
        "InternetChargeType": "TRAFFIC_POSTPAID_BY_HOUR",
        "DeadlineDate": ""
    })
}

fn instance(id: &str) -> Value {
    json!({
        "InstanceId": id,
        "InstanceName": format!("{id}-name"),
        "InstanceState": "RUNNING",
        "RestrictState": "NORMAL",
        "PrivateIpAddresses": ["10.0.1.10"],
        "PublicIpAddresses": [],
        "IPv6Addresses": [],
        "SecurityGroupIds": ["sg-1"],
        "CPU": 4,
        "Memory": 8,
        "OsName": "Ubuntu 22.04 LTS",
        "InstanceType": "S5.LARGE8",
        "InstanceChargeType": "POSTPAID_BY_HOUR",
        "CreatedTime": "2026-11-01T02:00:00Z",
        "Placement": {"Zone": "ap-guangzhou-6"},
        "VirtualPrivateCloud": {"VpcId": "vpc-1", "SubnetId": "subnet-1"}
    })
}

fn inventory_handler(action: &str, body: &Value) -> Value {
    match action {
        "DescribeInstances" => {
            let mut instances = vec![instance("ins-1"), instance("ins-2"), instance("ins-3")];
            // 三态：ins-1 明确 null、ins-2 空数组、ins-3 字段缺席。
            instances[0]["Tags"] = Value::Null;
            instances[1]["Tags"] = json!([]);
            json!({
                "TotalCount": 3,
                "InstanceSet": instances,
                "RequestId": "req-instances"
            })
        }
        "DescribeAddresses" => {
            // 带 address-type 过滤的那一路 → wanip；不带过滤的那一路 → all。
            if body.get("Filters").is_some() {
                json!({
                    "TotalCount": 2,
                    "AddressSet": [address("eip-a", "WanIP"), address("eip-b", "EIP")],
                    "RequestId": "req-addr-wanip"
                })
            } else {
                json!({
                    // 与 wanip 那路重叠 2 条，真正新增 1 条。
                    "TotalCount": 3,
                    "AddressSet": [
                        address("eip-a", "WanIP"),
                        address("eip-b", "EIP"),
                        address("eip-c", "EIP")
                    ],
                    "RequestId": "req-addr-all"
                })
            }
        }
        "DescribeVpcs" => json!({
            "TotalCount": 1,
            "VpcSet": [{
                "VpcId": "vpc-1",
                "VpcName": "production-vpc",
                "CidrBlock": "10.0.0.0/16",
                "CreatedTime": "2026-10-01 10:00:00",
                "TagSet": [{"Key": "Environment", "Value": "production"}],
                "IsDefault": false,
                "AssistantCidrSet": [],
                "Ipv6CidrBlockSet": [],
                "EnableDhcp": true,
                "DhcpOptionsId": "dopt-1",
                "DnsServerSet": ["10.0.0.2"],
                "DomainName": "internal.example"
            }],
            "RequestId": "req-vpcs"
        }),
        "DescribeSubnets" => json!({
            "TotalCount": 1,
            "SubnetSet": [{
                "SubnetId": "subnet-1",
                "SubnetName": "zone-6",
                "VpcId": "vpc-1",
                "CidrBlock": "10.0.1.0/24",
                "Zone": "ap-guangzhou-6",
                "TagSet": [],
                "RouteTableId": "rtb-1",
                "NetworkAclId": "acl-1",
                "AvailableIpAddressCount": 240,
                "TotalIpAddressCount": 256
            }],
            "RequestId": "req-subnets"
        }),
        "DescribeProjects" => json!({
            "TotalCount": 0,
            "Projects": [],
            "RequestId": "req-projects"
        }),
        "GetCallerIdentity" => json!({
            "AccountId": "1300000000",
            "Arn": "qcs::cam::uin/100000000001:uin/100000000001",
            "Type": "CAMUser",
            "UserId": "100000000001",
            "RequestId": "req-caller"
        }),
        "DescribeRegions" => json!({
            "TotalCount": 1,
            "RegionSet": [{
                "Region": "ap-guangzhou",
                "RegionName": "广州",
                "RegionState": "AVAILABLE"
            }],
            "RequestId": "req-regions"
        }),
        other => json!({
            "Error": {"Code": "InvalidAction", "Message": format!("mock 未实现 {other}")},
            "RequestId": "req-unknown"
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
    source_config_with_regions(dir, base, r#"["ap-guangzhou"]"#)
}

/// `regions` 直接拼进 TOML：`["ap-guangzhou"]` 或 `"auto"`。
fn source_config_with_regions(dir: &Path, base: &str, regions: &str) -> SourceConfig {
    source_config_with_assets(dir, base, regions, "")
}

/// 同上，额外给一段 `[assets]` 覆写（例如 `eip = false`）。
fn source_config_with_assets(dir: &Path, base: &str, regions: &str, assets: &str) -> SourceConfig {
    let secret_id = write_secret(dir, "id.secret", "AKIDTESTSECRETID");
    let secret_key = write_secret(dir, "key.secret", "TESTSECRETKEYVALUE");
    SourceConfig::parse(&format!(
        r#"
source_code = "tencent-cloud-prod"
tenant_id = "tenant-example"
vendor = "TENCENT_CLOUD"
group = "inventory"
env_ref = "prod"
regions = {regions}
endpoint_override = "{base}"
{assets}
[credential]
secret_id = {{ file = "{id}" }}
secret_key = {{ file = "{key}" }}
"#,
        id = secret_id.display(),
        key = secret_key.display()
    ))
    .unwrap()
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
    let source = TencentSource::init(cfg, &svc).await.expect("数据源初始化");
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
    // 线上记录是 `<JSON 头>CLOUDINV:<正文>\0`，正文才是快照本身。
    let bytes = &frames[0].bytes;
    let symbol = harvest_core::wire::WIRE_SYMBOL.as_bytes();
    let at = bytes
        .windows(symbol.len())
        .position(|w| w == symbol)
        .expect("记录里必须有 CLOUDINV: 符号");
    let payload: Value = serde_json::from_slice(&bytes[at + symbol.len()..]).unwrap();

    assert_eq!(payload["schema_native"], json!("wist.harvest.snapshot.v1"));
    assert_eq!(payload["vendor"], json!("TENCENT_CLOUD"));
    assert_eq!(payload["source_code"], json!("tencent-cloud-prod"));
    assert_eq!(payload["env_ref"], json!("prod"));
    assert_eq!(payload["regions"], json!(["ap-guangzhou"]));
    // 覆盖范围：手写地域子集只能是 partial —— 「不在结果里」可能是「在别的地域」。
    assert_eq!(payload["coverage"]["scope"], json!("partial"));
    assert_eq!(
        payload["coverage"]["complete"],
        json!(true),
        "能发出来就说明 verify 通过了"
    );
    assert_eq!(
        payload["coverage"]["account"],
        json!("tencent:1300000000"),
        "账号键来自 GetCallerIdentity"
    );

    assert!(
        payload["collected_at"]
            .as_str()
            .unwrap()
            .ends_with("+08:00"),
        "collected_at 沿用 +08:00 形态: {}",
        payload["collected_at"]
    );

    // 云主机：3 台，兼容字段名齐全。
    let hosts = payload["assets"]["CLOUD_HOST"].as_array().unwrap();
    assert_eq!(hosts.len(), 3);
    let ins1 = hosts
        .iter()
        .find(|h| h["source_asset_id"] == "ins-1")
        .unwrap();
    assert_eq!(ins1["ip"], json!("10.0.1.10"));
    assert_eq!(ins1["public_ip"], json!(""));
    assert_eq!(ins1["vpcid"], json!("vpc-1"));
    assert_eq!(ins1["subvpcid"], json!("subnet-1"));
    assert_eq!(ins1["version"], json!("1"));
    assert_eq!(ins1["ins_charge_type"], json!("POSTPAID_BY_HOUR"));

    // tags 三态在真实链路上也必须保留。
    assert!(ins1["tags"].is_null(), "Tags:null 要保留 null");
    let ins2 = hosts
        .iter()
        .find(|h| h["source_asset_id"] == "ins-2")
        .unwrap();
    assert_eq!(ins2["tags"], json!([]));
    let ins3 = hosts
        .iter()
        .find(|h| h["source_asset_id"] == "ins-3")
        .unwrap();
    assert!(
        ins3.as_object().unwrap().get("tags").is_none(),
        "Tags 字段缺席时不输出"
    );

    // EIP 双路合并：2 + 3 行 → 去重后 3 个地址。
    let eips = payload["assets"]["EIP"].as_array().unwrap();
    assert_eq!(eips.len(), 3, "双路合并必须按 AddressId 去重");
    let ids: Vec<&str> = eips
        .iter()
        .map(|e| e["source_asset_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["eip-a", "eip-b", "eip-c"]);
    assert_eq!(payload["stats"]["EIP"]["total"], json!(3));

    // VPC / 子网 + 兼容常量。
    let vpcs = payload["assets"]["VPC"].as_array().unwrap();
    assert_eq!(vpcs.len(), 1);
    assert_eq!(vpcs[0]["parent_source_asset_id"], json!("-1"));
    assert_eq!(vpcs[0]["risk_level"], json!("MEDIUM"));
    assert_eq!(vpcs[0]["enable"], json!(1));
    assert_eq!(vpcs[0]["network_type"], json!(""));

    let subnets = payload["assets"]["SUB_NET"].as_array().unwrap();
    assert_eq!(subnets.len(), 1);
    assert_eq!(subnets[0]["parent_source_asset_id"], json!("vpc-1"));
    assert_eq!(subnets[0]["asset_type"], json!("SUB_NET"));

    // 四个类型都进 stats，且 CLOUD_HOST 带地域分布。
    assert_eq!(payload["stats"]["CLOUD_HOST"]["total"], json!(3));
    assert_eq!(
        payload["stats"]["CLOUD_HOST"]["regions"]["ap-guangzhou"],
        json!(3)
    );

    // VPC / 子网的分页参数必须是字符串形式（沿用既有实现）。
    let requests = mock.requests();
    let vpc_request = requests
        .iter()
        .find(|r| r.action == "DescribeVpcs")
        .expect("必须调用过 DescribeVpcs");
    assert_eq!(
        vpc_request.body["Limit"],
        json!("100"),
        "DescribeVpcs 的 Limit 用字符串"
    );
    assert_eq!(vpc_request.body["Offset"], json!("0"));
    let subnet_request = requests
        .iter()
        .find(|r| r.action == "DescribeSubnets")
        .unwrap();
    assert_eq!(subnet_request.body["Limit"], json!("100"));

    // 云主机 / EIP 的 Limit 是整数。
    let cvm_request = requests
        .iter()
        .find(|r| r.action == "DescribeInstances")
        .unwrap();
    assert_eq!(cvm_request.body["Limit"], json!(100));

    // EIP 两路都打到了，且其中一路带 Filters。
    let address_requests: Vec<&RecordedRequest> = requests
        .iter()
        .filter(|r| r.action == "DescribeAddresses")
        .collect();
    assert_eq!(address_requests.len(), 2, "每地域两路分页");
    assert!(
        address_requests
            .iter()
            .any(|r| r.body.get("Filters").is_some()),
        "一路带 address-type=WanIP 过滤"
    );
    assert!(
        address_requests
            .iter()
            .any(|r| r.body.get("Filters").is_none()),
        "另一路不带过滤"
    );
}

#[tokio::test]
async fn page_total_mismatch_aborts_and_sends_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // 云端声称 5 台，只给 2 台：分页没抓完。
    let handler: Handler = Arc::new(|action: &str, body: &Value| {
        if action == "DescribeInstances" {
            // 第一页给 2 条，之后给空页：模拟“分页提前结束”而云端总数仍是 5。
            let offset = body["Offset"].as_u64().unwrap_or(0);
            let set = if offset == 0 {
                json!([instance("ins-1"), instance("ins-2")])
            } else {
                json!([])
            };
            json!({
                "TotalCount": 5,
                "InstanceSet": set,
                "RequestId": "req-broken"
            })
        } else {
            inventory_handler(action, body)
        }
    });
    let mock = MockApi::spawn(handler).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config(dir.path(), &mock.base);
    let source = TencentSource::init(cfg, &svc).await.expect("数据源初始化");
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
            .contains("分页总数不一致"),
        "abort_reason={:?}",
        outcome.report.abort_reason
    );
    assert!(
        !mock
            .requests()
            .iter()
            .any(|r| r.action == "DescribeAddresses"),
        "第一个工作项失败后不该继续抓后面的类型"
    );
}

#[tokio::test]
async fn auth_error_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let handler: Handler = Arc::new(|_action: &str, _body: &Value| {
        json!({
            "Error": {"Code": "AuthFailure.SecretIdNotFound", "Message": "secret id not found"},
            "RequestId": "req-auth"
        })
    });
    let mock = MockApi::spawn(handler).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config(dir.path(), &mock.base);

    // 项目表获取失败不阻断库存（交接文档 §2.2）。
    let source = TencentSource::init(cfg, &svc)
        .await
        .expect("项目表失败不应阻断初始化");
    assert_eq!(source.project_count(), 0);

    let project_calls = mock
        .requests()
        .iter()
        .filter(|r| r.action == "DescribeProjects")
        .count();
    assert_eq!(project_calls, 1, "鉴权失败重试无用，必须只调一次");
}

/// 从数据面记录里取出快照正文。
fn body_of(frame: &CompleteSnapshot) -> Value {
    let symbol = harvest_core::wire::WIRE_SYMBOL.as_bytes();
    let at = frame
        .bytes
        .windows(symbol.len())
        .position(|w| w == symbol)
        .expect("记录里必须有 CLOUDINV: 符号");
    serde_json::from_slice(&frame.bytes[at + symbol.len()..]).expect("正文是 JSON")
}

#[tokio::test]
async fn auto_regions_declare_account_level_coverage() {
    // `regions = "auto"` 时计划覆盖账号可见地域全集，这才有资格当退役证据。
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::spawn(Arc::new(inventory_handler)).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config_with_regions(dir.path(), &mock.base, r#""auto""#);

    let source = TencentSource::init(cfg, &svc).await.expect("初始化");
    let scope = source.scope();
    let sink = RecordingSink::default();
    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(outcome.state, RunState::Done, "{:?}", outcome.report.errors);

    let frames = sink.frames.lock().unwrap();
    let payload = body_of(&frames[0]);
    assert_eq!(payload["coverage"]["scope"], json!("account"));
    assert_eq!(payload["coverage"]["complete"], json!(true));
    assert_eq!(payload["coverage"]["account"], json!("tencent:1300000000"));
}

#[tokio::test]
async fn disabling_a_type_downgrades_coverage_to_partial() {
    // 关掉一类资产后，这一类就是空数组 + 没采。帧里仍是 `[]`，
    // 但**不能**再声称 `scope=account, complete=true`——那等于告诉下游「这类都没了」。
    let dir = tempfile::tempdir().unwrap();
    let mock = MockApi::spawn(Arc::new(inventory_handler)).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config_with_assets(
        dir.path(),
        &mock.base,
        r#""auto""#,
        "[assets]\neip = false\n",
    );

    let source = TencentSource::init(cfg, &svc).await.expect("初始化");
    let scope = source.scope();
    let sink = RecordingSink::default();
    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(outcome.state, RunState::Done, "{:?}", outcome.report.errors);

    let frames = sink.frames.lock().unwrap();
    let payload = body_of(&frames[0]);
    assert_eq!(payload["assets"]["EIP"], json!([]));
    assert_eq!(
        payload["coverage"]["scope"],
        json!("partial"),
        "少一类就不能声明账号级全集，否则空数组会被当成退役证据"
    );
    assert_eq!(
        payload["coverage"]["complete"],
        json!(true),
        "采集器自身的完整性校验仍然通过"
    );
    assert!(
        mock.requests()
            .iter()
            .all(|request| request.action != "DescribeAddresses"),
        "关掉的类型不应该再去调云 API"
    );
}

#[tokio::test]
async fn account_lookup_failure_still_publishes_but_without_an_account_key() {
    // 拿不到账号不能阻断采集，但必须让下游看出来：没有账号键 = 本轮不能用于退役。
    let dir = tempfile::tempdir().unwrap();
    let handler: Handler = Arc::new(|action: &str, body: &Value| {
        if action == "GetCallerIdentity" {
            return json!({
                "Error": {"Code": "AuthFailure.UnauthorizedOperation", "Message": "denied"},
                "RequestId": "req-deny"
            });
        }
        inventory_handler(action, body)
    });
    let mock = MockApi::spawn(handler).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config_with_regions(dir.path(), &mock.base, r#""auto""#);

    let source = TencentSource::init(cfg, &svc)
        .await
        .expect("账号失败不阻断");
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
