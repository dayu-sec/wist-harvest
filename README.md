# wist-harvest

数据平面上的一个**生产者**：到云账号拉取事实 → 归一成既有字段名 → 落本地 spool →
完整性校验 → 一个采集组一条完整快照帧写进数据平面。

```
wist-harvest ──原生快照帧──► warp-parse ──atlas.in.<family>.v1──► wist-atlas
```

设计依据：[`../wist-harvest-设计说明.md`](../wist-harvest-设计说明.md)（权威）与
[`../云资产与成本采集说明.md`](../云资产与成本采集说明.md) §2/§3。

## 范围

- 采集组 `inventory`，四个类型；目前两家云：**腾讯云**、**AWS**。

| 类型 | 腾讯云 | AWS | 备注 |
| --- | --- | --- | --- |
| `CLOUD_HOST` | `cvm.DescribeInstances` | `ec2.DescribeInstances` | 腾讯云每地域 1 道分页；AWS 用 `NextToken` 游标 |
| `EIP` | `vpc.DescribeAddresses` | `ec2.DescribeAddresses` | 腾讯云**每地域 2 道**（`wanip`/`all`）按 `AddressId` 去重；AWS 1 道 |
| `VPC` | `vpc.DescribeVpcs` | `ec2.DescribeVpcs` | 腾讯云 `Limit`/`Offset` 用字符串形式 |
| `SUB_NET` | `vpc.DescribeSubnets` | `ec2.DescribeSubnets` | 同上 |

- 状态机 `Planned → Fetching → Spooled → Verified → Published → Done`，
  任一步失败进 `Aborted` 且**数据平面零帧**。
- 凭据只从 `${SEC_*}` 或 0600 文件读，任何日志/报告/错误都过统一的脱敏器。
- 不做：安全组/路由/CLB、任何 git 操作。

## 多云：什么东西放在哪一层

新增一家云 = 新增一个 `harvest-{vendor}` crate + 在 `vendor` 注册表加一条分支
+ 在 `sources.d` 里写 `vendor = "..."`。**core 不需要改**，这是分层的验收线。

| 层 | 位置 | 放什么 | 不放什么 |
| --- | --- | --- | --- |
| 配置 | `harvest.toml` / `sources.d/*.toml` | 租户/数据源身份、`vendor`、地域范围、`[assets]` 开关、凭据引用、限速/超时/页大小、`allow_empty`、`max_resume_age_ms`、账单账号与币种、`[vendor_options]` | 签名、分页协议、字段映射、金额解析、完整性判据 |
| 共享代码 | `harvest-core` | 快照契约/规范模型、scope/snap_id、spool/manifest/续跑、状态机、完整性**框架**、分页**驱动**、错误分类、限速/重试、脱敏、report、wire/sink | 任何云 API 的形状 |
| 厂商代码 | `harvest-{vendor}` | 签名、端点、分页协议、错误码→`ErrorClass`、字段归一、必填/去重键、完整性**证据**、地域/身份解析、coverage 判定、账单 reader | 现场值（租户、地域、配额） |

两条硬规则：

1. **配置只能收紧能力，不能扩张能力**：`assets.vpc = false` 合法；打开一个该云不提供的类型
   必须在启动时失败（`capabilities` / `plan` 层面拦住）。
2. **有逻辑的东西不进配置**：字段路径、分页终止条件、金额/时间解析、状态枚举、去重键、
   签名算法。它们写错不会报错，只会静默错数据——必须是可编译、可 fixture 测试的代码。

已落地的跨云抽象：

- `Vendor` 闭枚举 + `vendor` 必填字段（写错/漏写直接启动失败）；
- `harvest-core::page`：`PageFetcher` + 驱动（总数一致、空页但有下一页、游标不前进、页数上限）；
- `Completeness::{Total, Exhausted}`：**没有总数的云靠「游标耗尽」作为完整性证据**，
  不再出现「没有 TotalCount 就跳过校验」；
- 凭据表 `[credential]` 的键名由厂商定义（腾讯云 `secret_id`/`secret_key`，
  AWS `access_key_id`/`secret_access_key`/`session_token`），core 只负责「从哪读」与脱敏；
- `[vendor_options]` 由厂商 crate 强类型解析（`deny_unknown_fields`），core 不解析。

## 成本链路（并列主干，不进数据平面）

成本不是资产的一类：它是事实表，O(资源×天×18 个月)，要按范围替换与月聚合，
所以**不走数据平面**（设计 §7.1）。harvest 只做「云 → 分片 + manifest」，
`wist-cost` 在计算区按 manifest 入账，atlas 不参与。

| 类型 | 云 API | 备注 |
| --- | --- | --- |
| 账单明细（腾讯云） | `billing.DescribeBillDetail`（2018-07-09） | 单页上限 **300**（1000 会被 `#Limit` 拒）；按 `bill_month` 整月拉一遍，日口径在本地筛 |
| 账单（AWS） | — | **尚未实现**：`cost` 会明确报错，不会静默按腾讯云跑 |

```bash
# 整月账单（CHARGE），默认上一个自然月
wist-harvest cost --config ... --source ... --month 2026-10 --output-dir /data/cost

# 单日消耗（CONSUMPTION）
wist-harvest cost --config ... --source ... --day 2026-10-15 --output-dir /data/cost
```

产物（与 `wist-cost` 的 `ShardKeyLayout` 逐字一致）：

```
<output_dir>/cost/{tenant}/{vendor}/{cost_basis}/{yyyy}/{MM}/0001.ndjson
<output_dir>/cost/{tenant}/{vendor}/{cost_basis}/{yyyy}/{MM}/manifest.json
```

发布顺序是**先暂存、后入位**：抓取期间分片只落在同目录的 `.staging-<batch_id>/`，
完整抓完才 `rename` 进最终键，manifest 最后原子替换。所以**抓取失败永远不会改写
已发布的字节**（旧 manifest 与旧分片保持有效）；同一范围重发若分片数变少，
旧的高序号分片会被清掉，不留孤儿。

三条不能松的口子：分片**不可变**（补采 = 新 `batch_id`）、金额全程十进制字符串
（`DecimalSum`，不碰浮点；**解析不出来就报错**，绝不静默归零）、`null`（未知）与
`"0"`（确认为零）严格区分。归日用**账单归属日 `BillDay`**，不是 `PayTime` —— 月底结算跨零点，
按付款时刻归日会把 10 月的行推到 11 月，而 11 月的查询不包含它们。

## 目录

```
wist-harvest/
├── _gal/                       # gx 工作区（gx init project --path rust 生成）
├── version.txt                 # 版本唯一来源
├── Cargo.toml                  # workspace（版本 marker 在 [workspace.package]）
├── docs/examples/              # harvest.toml + 腾讯云/AWS 的 sources.d 样例
└── crates/
    ├── harvest-core/           # 与厂商无关：config/scope/page/spool/manifest/完整性/
    │                           #   租约/限速/脱敏/状态机/调度/编排
    ├── harvest-tencent/        # TC3-HMAC-SHA256 签名 + 四个 reader + 字段归一
    ├── harvest-aws/            # SigV4 签名 + ec2 reader（XML / NextToken）+ STS 身份
    ├── harvest-warp/           # 数据平面 sink：TLS + 长度前缀帧
    └── harvest-app/            # lib（once/run/dry-run/diagnose）+ bin `wist-harvest`
```

## 构建与门禁

仓库带 `_gal/`，按 `rust-gx` skill 走 gx；验收用的等价 cargo 命令也列在这里。

```bash
gx run build -e debug     # 版本同步 + cargo build + cargo test + 拷到 ~/bin
gx run lint               # cargo fmt（会改文件）+ cargo fix + clippy -D warnings
gx run test

# 等价的直接命令
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

> 注意：`gx run lint` 里的 `cargo fix` 需要版本库；仓库已入库，可直接使用。
> 等价的三行门禁如下。

## 用法

```bash
# 只打印生效配置与采集计划（不联网、不写盘）
wist-harvest dry-run --config /etc/wist-harvest/harvest.toml \
                     --source /etc/wist-harvest/sources.d/tencent-cloud-prod.toml

# 自检（容器 healthcheck 可直接用）
wist-harvest diagnose --config ... --source ... [--quiet]

# 跑一轮就退出
wist-harvest once --config ... --source ...

# 常驻调度（容器主进程）
wist-harvest run --config ... --source ...
```

退出码：`once` → `0` = `Done`；`2` = `Aborted`（已有报告与 spool）；`3` = 配置/凭据/数据源初始化失败。
`run` 优雅退出后恒为 `0`；`diagnose` → 全 OK `0`，任一 FAIL `1`。

配置文件样例见 [`docs/examples/`](docs/examples/)（腾讯云与 AWS 各一份）。
凭据永远不写在配置里，而且**键名由厂商定义**：

```toml
# 腾讯云
vendor = "TENCENT_CLOUD"
[credential]
secret_id  = { env = "SEC_TENCENT_SECRET_ID" }
secret_key = { env = "SEC_TENCENT_SECRET_KEY" }

# AWS
vendor = "AWS"
[credential]
access_key_id     = { env = "SEC_AWS_ACCESS_KEY_ID" }
secret_access_key = { env = "SEC_AWS_SECRET_ACCESS_KEY" }
# session_token 可选（临时凭据）
# 或 { file = "/path/secret.json", field = "secret_key" }（文件必须 0600）
```

AWS 的专属参数放在 `[vendor_options]`（core 不解析，由 `harvest-aws` 强类型校验）：

```toml
[vendor_options]
partition = "aws"   # aws / aws-cn / aws-us-gov
```

## 常驻调度：`run`

```bash
wist-harvest run --config ... --source ... \
  [--run-at 00:30]      # 每天几点（Asia/Shanghai），默认 00:30；与 --interval 互斥
  [--interval 30m]      # 或固定间隔
  [--jitter 5m]         # 抖动上限，默认 5m：避免多站点同秒打云 API
  [--max-retries 3]     # 同一周期内最多额外重试几次，默认 3
  [--backoff-base 60s] [--backoff-max 15m]
  [--shutdown-grace 30s]
  [--now]               # 启动后立即跑一轮，不等第一个定点
  [--fresh]
```

两条语义是有意分开的，别混成一条：

- **到点就跑**：下一轮定点由墙上时钟算出，失败与重试都**不推迟**它
  （`Scheduler::next_scheduled` 与失败计数完全解耦）。
- **失败退避**：失败后按 `base * 2^(n-1)`、上限 `--backoff-max` 安排一次**额外重试**；
  每个周期最多 `--max-retries` 次，重试时间若晚于下一个定点就直接丢弃。
  所以退避只影响「补救」，不会把日常巡检拖成越来越晚。

另外三条行为：

- **单轮并发**：到点时若上一轮还没结束，本轮直接跳过并记 `warn`；
  跨进程还有一层 `flock` 租约（`run_once` 内部），跳过不会变成双采。
- **每轮重新读配置与凭据**：配置改了不用重启容器，凭据轮换下一轮就生效。
- **优雅退出**：SIGTERM/SIGINT → 通知在飞轮次取消。抓取中的轮次会在工作项边界
  停下来进 `Aborted`（**数据平面零帧**，spool 留着下次续跑）；已经 `Verified`
  的轮次会正常发完再退。等不到 `--shutdown-grace` 就直接退出，退出码 `0`。

## 容器 healthcheck

```yaml
healthcheck:
  test: ["CMD", "/usr/local/bin/wist-harvest", "diagnose",
         "--config", "/etc/wist-harvest/harvest.toml",
         "--source", "/etc/wist-harvest/sources.d/tencent-cloud-prod.toml",
         "--quiet"]
  interval: 60s
  timeout: 10s
  retries: 3
```

`diagnose` 是**离线、非交互**的：检查配置可读、凭据来源存在且文件 0600、
spool/state 目录可写、数据平面端点可解析、是否有可续跑 spool。
端点**只校验 `host:port` 形态、不做 TCP 拨号**——对端一忙就重启采集器只会放大故障。
`--quiet` 只输出失败项；输出里不会出现凭据值（成功读到的值会注册进脱敏器做兜底）。

## 运行不变量（都有测试）

1. **不完整不发**：`Aborted` → 零帧。发送只在 `Verified` 之后发生，
   `run::fill_and_publish` 是唯一调用点。
2. **空 ≠ 失败**：某类型 0 行时，除"范围完整 + 云端确实为空 + `service.allow_empty`
   放行该类型"三者同时成立外，一律 `Aborted`。`allow_empty` 可以写 `true`/`false`，
   也可以写 `["EIP"]` 这样的类型列表——别为了养一个空类型把其余三类的保护一起关掉。
3. **少一类就不声称账号级全集**：`coverage.scope = account` 需要「账号可见地域全集」
   **且四类资产全采**。关掉任一 `assets.<type>` 时自动降为 `partial`（下游只 upsert、
   不判死），否则空数组会被当成"这类资产都没了"的退役证据。
4. **重放幂等**：同一 spool 重发同一 `snap_id`，帧体逐字节一致
   （`collected_at` 固定记录在 manifest 里，不是每次取当前时间）。
5. **续跑**：manifest 记录每个工作项的 `completed`，重启后只重抓失败项；
   未完成项整项重抓（`begin_item` 先截断文件）。**计划变了或超过
   `service.max_resume_age_ms`（默认 24h）就放弃旧 spool**（状态 `Abandoned`，保留供诊断），
   开新 `snap_id` 重采——否则一份永远续不完的旧 spool 会让采集器永不发布。
6. **读文件本身**：组装前核对每个工作项的实际行数与 manifest 一致，文件缺失/行数不符
   一律 `Aborted`（只信 manifest 会把"文件没了"发成"这类资产 0 行"）。
7. **完整性证据两种形态**：有总数的云（腾讯云/阿里云）看「实收 == `TotalCount`」；
   无总数的云（AWS/Azure）看「游标耗尽」。两者都记到 manifest 的 `completeness` 里，
   **不存在「没有 TotalCount 就不校验」的路径**；旧 manifest 回落 `expected_rows`。
8. **凭据零泄漏**：凭据表里的真实值全部注册到全局脱敏器，
   日志/报告/manifest 失败原因都过一遍；云 API 错误体裁到 `Code`/`Message`/`RequestId`。

## spool 目录

```
<spool_dir>/<tenant>/<producer>/<snap_id>/
  manifest.json                                   # 原子写（tmp + rename）
  items/CLOUD_HOST/ap-guangzhou.default.ndjson
  items/EIP/ap-guangzhou.wanip.ndjson             # Filters=[address-type=WanIP]
  items/EIP/ap-guangzhou.all.ndjson               # 不带过滤
  items/VPC/ap-guangzhou.default.ndjson
  items/SUB_NET/ap-guangzhou.default.ndjson
```

运行报告落 `<state_dir>/runs/<snap_id>.json`，租约锁在 `<state_dir>/lease/`。

## 取舍与偏离（P0 明确做了选择的地方）

1. **没有 `harvest-source` crate。** 设计说明 §3 的目录里有一个独立的
   `harvest-source`（`Source` 契约）。两家厂商（腾讯云/AWS）共用的只有接口与驱动，
   量级还撑不起一个独立 crate，所以 `Source`/`Sink`/`CostSource` 契约与 `page` 驱动
   都在 `harvest-core`。真要再膨胀时再抽，改动只是 `pub use`。
2. **`snap_id` 里的 `:` 换成 `-`。** 公式是 `{producer}-{scope_hash}-{ts}`，而
   `producer = {source_code}:{group}`。`snap_id` 要当目录名用，所以实际落盘形态是
   `tencent-cloud-prod-inventory-<hash>-<ts>`。另外 `snap_id` 时间戳只到秒，
   同一秒内的两轮会撞目录名，因此新一轮会追加 `-1`/`-2` 后缀（`Spool::next_available_snap_id`）。
3. **`collected_at` 存在 manifest 里。** 为了"同 `snap_id` 重放逐字节一致"，
   观测时间在建 spool 时固定，续跑/重发沿用，而不是每次取 `now`。
4. **帧格式是 ASCII 十进制长度前缀**（`{"<字节数>"} {正文}`），不是二进制前缀。
   这是按 `wist-agentd` 的 `TcpFraming::Len` 与真实 wparse 验证过的：早先按 BE u32 实现时
   帧看起来"发出去了"，引擎却一条也收不到。集中在 `harvest_warp::frame`，改一处即可。
5. **`DescribeVpcs`/`DescribeSubnets` 的 `Limit`/`Offset` 用字符串。** 这是按交接
   文档的既有实现沿用的；腾讯云官方接口签名是整型。**接真实账号前必须验一次**，
   若被拒绝，把 `PageSpec::limit_as_string` 改成 `false` 即可（一处 switch）。
6. **`Aborted` 的保留与清理。** 报告与 spool 都保留，但没有实现按 TTL 自动清理
   （设计 §5 要求"按 TTL 清理"），见 TODO。
7. **spool 用同步文件 IO。** 每页写一次、量级可控；没有改成 `spawn_blocking`。
8. **`concurrency` 只解析不生效。** P0 顺序抓取，字段留着以免下一阶段改配置结构。
9. **`allow_empty` 按类型判定。** 某类型整体 0 行即视为可疑（需要显式开关），
   而不是按整帧 0 行判断——这样"少采了一类"不会静默变成"这类资产没了"。开关本身也
   可以只给某几类（`allow_empty = ["EIP"]`），不必为了一类牺牲其余三类的保护。
10. **TLS 只跑到配置构建层。** 测试用明文 TCP 覆盖帧协议与 ACK 语义；
    mTLS 握手路径需要真实证书，P0 没有做端到端测试。
11. **调度时区固定 `+08:00`。** `--run-at` 解释为 Asia/Shanghai 挂钵时间；
    该时区 1991 年后无夏令时，固定偏移就够，不为它引入时区数据库。
12. **抖动用 splitmix64 采样**（seed 取系统时间纳秒），不引入随机数依赖；
    采样函数是纯函数，测试可直接带入固定 seed。
13. **`run` 启动后不立即采集**，只等第一个定点（防容器重启风暴→连续打云 API）；
    需要"起来先跑一轮"就加 `--now`。
14. **取消只在工作项边界生效。** 一个工作项写到一半被打断会留下半份 spool，
    所以信号到达后等当前工作项写完再判；已 `Verified` 的轮次会发完再退。
15. **`diagnose` 不再拨号。** 上一版的 TCP 探测已改成只校验 `host:port` 形态：
    把数据平面可达性写进 healthcheck，会让下游一次抖动连锁重启所有采集器。

## TODO（明确没做）

- **AWS 的成本链路**（CUR / Cost Explorer）—— `cost` 对 AWS 会明确报错；
  接入前需要和 `wist-cost` 一起把契约里的 `vendor` 枚举与账单口径定版；
- 成本：18 个月回填的编排（现在是「一个范围跑一次」，外层脚本串）；COS 直传（现在只写本地目录）；
- `exposure` 组（安全组 + 规则、路由、对等连接、NAT/CCN、CLB）；
- `env_ref` 的语义（设计 §9 缺口 3）：当前按配置直传，不做账号/项目/VPC 的切分推导；
- `harvest_*` Prometheus 指标（报告已落盘，指标端点未接）；
- 报告查询接口（现在只有文件）；
- `Aborted` spool 的 TTL 清理（现在只做到「超龄不再续跑」，不删目录）；
- 并发抓取（`service.concurrency` 已解析但未生效）；
- 调度：没有跨重启的"上次定点"记录（重启后按当前墙钟算下一次）；
  `--interval` 也不跨重启延续节奏；
- 调度：没有持久化"连续失败次数"，重启后退避从第 1 级重新开始；
- AWS 的联调：真实签名/分区/STS 临时凭据（目前用固定向量 + mock 验证）；
  腾讯云的 `DescribeVpcs`/`DescribeSubnets` 字符串分页参数仍需真实账号确认；
- 单帧上限超限时的下游分片组装（设计 §9 缺口 2）。
