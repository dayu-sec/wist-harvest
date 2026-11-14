//! AWS 端到端：真 `AwsSource` + 假 AWS（本地 HTTP，XML）+ 记录型 sink。
//!
//! 覆盖跨云最关键的几处差异：
//!
//! 1. `NextToken` 游标分页（**没有总数**）→ 完整性证据是 `Exhausted`；
//! 2. 字段归一到同一套规范模型，AWS 不提供的字段保持 `Absent`；
//! 3. `regions = "auto"` 且四类全采时才声明账号级 coverage；
//! 4. XML 坏掉 → `Aborted` 且数据平面零帧。
//!
//! 假服务的资源 ID 带地域后缀：真实 AWS 的 ID 全局唯一，但测试里两个地域共用一份
//! fixture，不加后缀会撞 `source_asset_id`（那反而是测试自己造的错误数据）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;
use harvest_aws::AwsSource;
use harvest_core::config::{ServiceConfig, SourceConfig};
use harvest_core::error::HarvestError;
use harvest_core::run::{RunOptions, run_once};
use harvest_core::source::{CompleteSnapshot, Completeness, PublishAck, Sink, Source};
use harvest_core::state::RunState;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// ---------------------------------------------------------------------------
// 假 AWS：表单 POST + XML 响应
// ---------------------------------------------------------------------------

struct MockRequest {
    form: HashMap<String, String>,
    /// 从 `Authorization` 的 credential scope 里取出来（mock 需要按地域造不同 ID）。
    region: String,
}

impl MockRequest {
    fn action(&self) -> &str {
        self.form.get("Action").map(String::as_str).unwrap_or("")
    }
}

/// 返回 (HTTP 状态码, XML)。
type Handler = Arc<dyn Fn(&MockRequest) -> (u16, String) + Send + Sync>;

struct MockAws {
    base: String,
    requests: Arc<Mutex<Vec<MockRequest>>>,
}

impl MockAws {
    async fn spawn(handler: Handler) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<MockRequest>>> = Arc::new(Mutex::new(Vec::new()));
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

    fn requests(&self) -> Vec<MockRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| MockRequest {
                form: request.form.clone(),
                region: request.region.clone(),
            })
            .collect()
    }

    fn actions(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| request.action().to_string())
            .collect()
    }
}

async fn serve_connection(
    mut stream: TcpStream,
    handler: Handler,
    requests: Arc<Mutex<Vec<MockRequest>>>,
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
    let mut authorization = String::new();
    for line in head.lines().skip(1) {
        if let Some((key, value)) = line.split_once(':') {
            match key.trim().to_ascii_lowercase().as_str() {
                "content-length" => content_length = value.trim().parse().unwrap_or(0),
                "authorization" => authorization = value.trim().to_string(),
                _ => {}
            }
        }
    }
    // `AWS4-HMAC-SHA256 Credential=<akid>/<date>/<region>/<service>/aws4_request, ...`
    let region = authorization
        .split('/')
        .nth(2)
        .unwrap_or("us-east-1")
        .to_string();

    let mut body = buffer[header_end + 4..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Ok(());
        }
        body.extend_from_slice(&chunk[..read]);
    }
    let request = MockRequest {
        form: parse_form(&String::from_utf8_lossy(&body)),
        region,
    };
    requests.lock().unwrap().push(MockRequest {
        form: request.form.clone(),
        region: request.region.clone(),
    });

    let (status, xml) = handler(&request);
    let payload = xml.into_bytes();
    let header = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    stream.write_all(header.as_bytes()).await?;
    stream.write_all(&payload).await?;
    stream.flush().await?;
    Ok(())
}

fn parse_form(body: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in body.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            out.insert(percent_decode(key), percent_decode(value));
        }
    }
    out
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                index += 3;
            }
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

// ---------------------------------------------------------------------------
// XML 场景
// ---------------------------------------------------------------------------

fn instances(region: &str, page: u32, next: Option<&str>) -> String {
    let next_token = next
        .map(|token| format!("<nextToken>{token}</nextToken>"))
        .unwrap_or_default();
    let id = format!("i-{region}-{page}");
    format!(
        r#"<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>req-{id}</requestId>
  <reservationSet>
    <item><instancesSet><item>
      <instanceId>{id}</instanceId>
      <instanceType>t3.micro</instanceType>
      <instanceState><code>16</code><name>running</name></instanceState>
      <launchTime>2026-11-01T00:00:00.000Z</launchTime>
      <placement><availabilityZone>{region}a</availabilityZone></placement>
      <vpcId>vpc-{region}</vpcId>
      <subnetId>subnet-{region}</subnetId>
      <privateIpAddress>10.0.1.10</privateIpAddress>
      <groupSet><item><groupId>sg-{region}</groupId></item></groupSet>
      <tagSet><item><key>Name</key><value>{id}-name</value></item></tagSet>
    </item></instancesSet></item>
  </reservationSet>
  {next_token}
</DescribeInstancesResponse>"#
    )
}

fn vpcs(region: &str) -> String {
    format!(
        r#"<DescribeVpcsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <vpcSet><item>
    <vpcId>vpc-{region}</vpcId>
    <cidrBlock>10.0.0.0/16</cidrBlock>
    <isDefault>false</isDefault>
    <tagSet/>
  </item></vpcSet>
</DescribeVpcsResponse>"#
    )
}

fn subnets(region: &str) -> String {
    format!(
        r#"<DescribeSubnetsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <subnetSet><item>
    <subnetId>subnet-{region}</subnetId>
    <vpcId>vpc-{region}</vpcId>
    <cidrBlock>10.0.1.0/24</cidrBlock>
    <availableIpAddressCount>251</availableIpAddressCount>
    <availabilityZone>{region}a</availabilityZone>
    <defaultForAz>false</defaultForAz>
  </item></subnetSet>
</DescribeSubnetsResponse>"#
    )
}

fn addresses(region: &str) -> String {
    format!(
        r#"<DescribeAddressesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <addressesSet><item>
    <publicIp>203.0.113.10</publicIp>
    <allocationId>eipalloc-{region}</allocationId>
    <domain>vpc</domain>
    <instanceId>i-{region}-1</instanceId>
    <associationId>eipassoc-{region}</associationId>
  </item></addressesSet>
</DescribeAddressesResponse>"#
    )
}

fn regions_xml() -> String {
    r#"<DescribeRegionsResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <regionInfo>
    <item><regionName>us-east-1</regionName><optInStatus>opt-in-not-required</optInStatus></item>
    <item><regionName>ap-east-1</regionName><optInStatus>opted-in</optInStatus></item>
    <item><regionName>me-south-1</regionName><optInStatus>not-opted-in</optInStatus></item>
  </regionInfo>
</DescribeRegionsResponse>"#
        .to_string()
}

fn identity() -> String {
    r#"<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <GetCallerIdentityResult>
    <Arn>arn:aws:iam::123456789012:user/harvest</Arn>
    <Account>123456789012</Account>
  </GetCallerIdentityResult>
  <ResponseMetadata><RequestId>req-identity</RequestId></ResponseMetadata>
</GetCallerIdentityResponse>"#
        .to_string()
}

fn full_handler(request: &MockRequest) -> (u16, String) {
    let region = request.region.as_str();
    match request.action() {
        "GetCallerIdentity" => (200, identity()),
        "DescribeRegions" => (200, regions_xml()),
        "DescribeInstances" => {
            if request.form.contains_key("NextToken") {
                (200, instances(region, 2, None))
            } else {
                (200, instances(region, 1, Some("page-2")))
            }
        }
        "DescribeVpcs" => (200, vpcs(region)),
        "DescribeSubnets" => (200, subnets(region)),
        "DescribeAddresses" => (200, addresses(region)),
        other => (
            400,
            format!(
                r#"<Response><Errors><Error><Code>InvalidAction</Code><Message>mock 未实现 {other}</Message></Error></Errors><RequestID>req-x</RequestID></Response>"#
            ),
        ),
    }
}

// ---------------------------------------------------------------------------
// 脚手架
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

fn source_config(dir: &Path, base: &str, regions: &str, assets: &str) -> SourceConfig {
    let access_key = write_secret(dir, "aws.id", "AKIDEXAMPLEACCESSKEY");
    let secret_key = write_secret(dir, "aws.key", "SECRETKEYEXAMPLE0001");
    SourceConfig::parse(&format!(
        r#"
source_code = "aws-prod"
vendor = "aws"
regions = {regions}
endpoint_override = "{base}"
{assets}

[credential]
access_key_id = {{ file = "{access_key}" }}
secret_access_key = {{ file = "{secret_key}" }}
"#,
        access_key = access_key.display(),
        secret_key = secret_key.display()
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

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[tokio::test]
async fn aws_inventory_end_to_end_with_cursor_pagination() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockAws::spawn(Arc::new(full_handler)).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config(dir.path(), &mock.base, r#"["us-east-1"]"#, "");

    let source = AwsSource::init(cfg, &svc).await.expect("初始化");
    let scope = source.scope();
    let sink = RecordingSink::default();
    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(outcome.state, RunState::Done, "{:?}", outcome.report.errors);

    let frames = sink.frames.lock().unwrap();
    assert_eq!(frames.len(), 1);
    let payload = body_of(&frames[0]);

    assert_eq!(payload["vendor"], json!("AWS"));
    assert_eq!(payload["source_code"], json!("aws-prod"));
    // 手写地域子集 → 只能是 partial（「不在结果里」可能是「在别的地域」）。
    assert_eq!(payload["coverage"]["scope"], json!("partial"));
    assert_eq!(payload["coverage"]["complete"], json!(true));
    assert_eq!(payload["coverage"]["account"], json!("aws:123456789012"));

    let hosts = payload["assets"]["CLOUD_HOST"].as_array().unwrap();
    assert_eq!(hosts.len(), 2, "两页游标分页都要收进来");
    let first = hosts
        .iter()
        .find(|host| host["source_asset_id"] == "i-us-east-1-1")
        .expect("第一页的实例");
    assert_eq!(first["ip"], json!("10.0.1.10"));
    assert_eq!(first["vpcid"], json!("vpc-us-east-1"));
    assert_eq!(first["subvpcid"], json!("subnet-us-east-1"));
    assert_eq!(first["version"], json!("1"));
    assert_eq!(first["host_name"], json!("i-us-east-1-1-name"));

    assert_eq!(payload["assets"]["EIP"].as_array().unwrap().len(), 1);
    assert_eq!(payload["assets"]["VPC"].as_array().unwrap().len(), 1);
    assert_eq!(payload["assets"]["SUB_NET"].as_array().unwrap().len(), 1);

    // 无总数的云：每个工作项的证据都是「游标耗尽」。
    let spool_dir = svc
        .service
        .spool_dir
        .join(scope.spool_prefix())
        .join(&outcome.snap_id);
    let spool = harvest_core::spool::Spool::open_existing(&spool_dir).unwrap();
    for record in &spool.manifest().items {
        assert_eq!(
            record.completeness,
            Some(Completeness::Exhausted),
            "{} 应有游标耗尽证据",
            record.file
        );
    }

    // 分页参数：第一页没有 NextToken，第二页有。
    let instance_requests: Vec<_> = mock
        .requests()
        .into_iter()
        .filter(|request| request.action() == "DescribeInstances")
        .collect();
    assert_eq!(instance_requests.len(), 2);
    assert!(!instance_requests[0].form.contains_key("NextToken"));
    assert_eq!(
        instance_requests[1]
            .form
            .get("NextToken")
            .map(String::as_str),
        Some("page-2")
    );
    assert_eq!(
        instance_requests[0]
            .form
            .get("MaxResults")
            .map(String::as_str),
        Some("100")
    );
}

#[tokio::test]
async fn auto_regions_declare_account_coverage_and_skip_not_opted_in() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockAws::spawn(Arc::new(full_handler)).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config(dir.path(), &mock.base, r#""auto""#, "");

    let source = AwsSource::init(cfg, &svc).await.expect("初始化");
    let regions: Vec<String> = source
        .regions()
        .iter()
        .map(|entry| entry.code.clone())
        .collect();
    assert_eq!(
        regions,
        vec!["ap-east-1", "us-east-1"],
        "not-opted-in 必须排除"
    );

    let scope = source.scope();
    let sink = RecordingSink::default();
    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(outcome.state, RunState::Done, "{:?}", outcome.report.errors);
    let payload = body_of(&sink.frames.lock().unwrap()[0]);
    assert_eq!(payload["coverage"]["scope"], json!("account"));
    assert_eq!(
        payload["regions"],
        json!(["ap-east-1", "us-east-1"]),
        "地域全集要随帧发出"
    );
}

#[tokio::test]
async fn malformed_xml_aborts_with_zero_frames() {
    let dir = tempfile::tempdir().unwrap();
    let handler: Handler = Arc::new(|request| {
        if request.action() == "DescribeInstances" {
            (
                200,
                "<DescribeInstancesResponse><reservationSet>".to_string(),
            )
        } else {
            full_handler(request)
        }
    });
    let mock = MockAws::spawn(handler).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config(dir.path(), &mock.base, r#"["us-east-1"]"#, "");

    let source = AwsSource::init(cfg, &svc).await.expect("初始化");
    let scope = source.scope();
    let sink = RecordingSink::default();
    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;

    assert_eq!(outcome.state, RunState::Aborted);
    assert!(sink.frames.lock().unwrap().is_empty(), "Aborted 必须零帧");
    assert!(
        outcome
            .report
            .abort_reason
            .as_deref()
            .unwrap_or_default()
            .contains("XML"),
        "abort_reason={:?}",
        outcome.report.abort_reason
    );
}

#[tokio::test]
async fn disabling_a_type_downgrades_coverage_to_partial() {
    let dir = tempfile::tempdir().unwrap();
    let mock = MockAws::spawn(Arc::new(full_handler)).await;
    let svc = service_config(&dir.path().join("spool"), &dir.path().join("state"));
    let cfg = source_config(
        dir.path(),
        &mock.base,
        r#""auto""#,
        "[assets]\nsubnet = false\n",
    );

    let source = AwsSource::init(cfg, &svc).await.expect("初始化");
    let scope = source.scope();
    let sink = RecordingSink::default();
    let outcome = run_once(&svc, &scope, &source, &sink, &RunOptions::default()).await;
    assert_eq!(outcome.state, RunState::Done, "{:?}", outcome.report.errors);
    let payload = body_of(&sink.frames.lock().unwrap()[0]);
    assert_eq!(payload["assets"]["SUB_NET"], json!([]));
    assert_eq!(payload["coverage"]["scope"], json!("partial"));
    assert!(
        !mock
            .actions()
            .iter()
            .any(|action| action == "DescribeSubnets"),
        "关掉的类型不应该再去调云 API"
    );
}
