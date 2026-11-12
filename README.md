# wist-harvest

数据平面上的一个**生产者**：到云账号拉取事实 → 归一成既有字段名 → 落本地 spool →
完整性校验 → 一个采集组一条完整快照帧写进数据平面。

```
wist-harvest ──原生快照帧──► warp-parse ──atlas.in.<family>.v1──► wist-atlas
```

设计依据：[`../wist-harvest-设计说明.md`](../wist-harvest-设计说明.md)（权威）与
[`../云资产与成本采集说明.md`](../云资产与成本采集说明.md) §2/§3。

## P0 范围

- 采集组 `inventory`，四个类型，全部来自腾讯云：

| 类型 | 云 API | 备注 |
| --- | --- | --- |
| `CLOUD_HOST` | `cvm.DescribeInstances`（2017-03-12） | 每地域 1 道分页 |
| `EIP` | `vpc.DescribeAddresses`（2017-03-12） | **每地域 2 道分页**，按 `AddressId` 合并去重 |
| `VPC` | `vpc.DescribeVpcs`（2017-03-12） | `Limit`/`Offset` 用字符串形式 |
| `SUB_NET` | `vpc.DescribeSubnets`（2017-03-12） | 同上 |

- 状态机 `Planned → Fetching → Spooled → Verified → Published → Done`，
  任一步失败进 `Aborted` 且**数据平面零帧**。
- 凭据只从 `${SEC_*}` 或 0600 文件读，任何日志/报告/错误都过统一的脱敏器。
- 不做：成本 reader（下一阶段）、安全组/路由/CLB、任何 git 操作。

## 目录

```
wist-harvest/
├── _gal/                       # gx 工作区（gx init project --path rust 生成）
├── version.txt                 # 版本唯一来源
├── Cargo.toml                  # workspace（版本 marker 在 [workspace.package]）
├── docs/examples/              # harvest.toml + sources.d 样例
└── crates/
    ├── harvest-core/           # 与厂商无关：config/scope/spool/manifest/完整性/
    │                           #   租约/限速/脱敏/状态机/调度/编排
    ├── harvest-tencent/        # TC3-HMAC-SHA256 签名 + 四个 reader + 字段归一
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

> 注意：`gx run lint` 里的 `cargo fix` 在没有版本库的目录会直接报
> `no VCS found for this package`（cargo 自身的保护）。本阶段按要求没有执行任何
> git 操作，所以门禁用的是上面三行等价命令；仓库入库后 `gx run lint` 即可使用。

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

配置文件样例见 [`docs/examples/`](docs/examples/)。凭据永远不写在配置里：

```toml
[credential]
secret_id  = { env = "SEC_TENCENT_SECRET_ID" }
secret_key = { env = "SEC_TENCENT_SECRET_KEY" }
# 或 { file = "/path/secret.json", field = "secret_key" }（文件必须 0600）
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
2. **空 ≠ 失败**：某类型 0 行时，除"范围完整 + 云端确实为空 + `service.allow_empty=true"`
   三者同时成立外，一律 `Aborted`。
3. **重放幂等**：同一 spool 重发同一 `snap_id`，帧体逐字节一致
   （`collected_at` 固定记录在 manifest 里，不是每次取当前时间）。
4. **续跑**：manifest 记录每个工作项的 `completed`，重启后只重抓失败项；
   未完成项整项重抓（`begin_item` 先截断文件）。
5. **凭据零泄漏**：`secret_id`/`secret_key` 真实值注册到全局脱敏器，
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
   `harvest-source`（`Source` 契约）。P0 只有一个厂商实现，多一个只有 trait 的
   crate 收益为零，所以 `Source`/`Sink` 契约放在 `harvest-core::source`。
   真要引入第二家厂商时再抽出去，改动只是 `pub use`。
2. **`snap_id` 里的 `:` 换成 `-`。** 公式是 `{producer}-{scope_hash}-{ts}`，而
   `producer = {source_code}:{group}`。`snap_id` 要当目录名用，所以实际落盘形态是
   `tencent-cloud-prod-inventory-<hash>-<ts>`。另外 `snap_id` 时间戳只到秒，
   同一秒内的两轮会撞目录名，因此新一轮会追加 `-1`/`-2` 后缀（`Spool::next_available_snap_id`）。
3. **`collected_at` 存在 manifest 里。** 为了"同 `snap_id` 重放逐字节一致"，
   观测时间在建 spool 时固定，续跑/重发沿用，而不是每次取 `now`。
4. **帧格式 4 字节大端长度前缀。** 设计只写了 `framing="len"`，字节序/宽度需与
   warp-parse 侧确认；这里选了 BE u32 并集中在 `harvest_warp::frame`，改一处即可。
5. **`DescribeVpcs`/`DescribeSubnets` 的 `Limit`/`Offset` 用字符串。** 这是按交接
   文档的既有实现沿用的；腾讯云官方接口签名是整型。**接真实账号前必须验一次**，
   若被拒绝，把 `PageSpec::limit_as_string` 改成 `false` 即可（一处 switch）。
6. **`Aborted` 的保留与清理。** 报告与 spool 都保留，但没有实现按 TTL 自动清理
   （设计 §5 要求"按 TTL 清理"），见 TODO。
7. **spool 用同步文件 IO。** 每页写一次、量级可控；没有改成 `spawn_blocking`。
8. **`concurrency` 只解析不生效。** P0 顺序抓取，字段留着以免下一阶段改配置结构。
9. **`allow_empty` 按类型判定。** 某类型整体 0 行即视为可疑（需要显式开关），
   而不是按整帧 0 行判断——这样"少采了一类"不会静默变成"这类资产没了"。
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

- 成本 reader（`DescribeCostDetail` 等）与 `harvest-cost-v1` 契约的分片落盘；
- `exposure` 组（安全组 + 规则、路由、对等连接、NAT/CCN、CLB）；
- `env_ref` 的语义（设计 §9 缺口 3）：当前按配置直传，不做账号/项目/VPC 的切分推导；
- `harvest_*` Prometheus 指标（报告已落盘，指标端点未接）；
- 报告查询接口（现在只有文件）；
- `Aborted` spool 的 TTL 清理；
- 并发抓取（`service.concurrency` 已解析但未生效）；
- 调度：没有跨重启的"上次定点"记录（重启后按当前墙钟算下一次）；
  `--interval` 也不跨重启延续节奏；
- 调度：没有持久化"连续失败次数"，重启后退避从第 1 级重新开始；
- 真实云账号的联调：`DescribeVpcs`/`DescribeSubnets` 的字符串分页参数、
  `framing="len"` 的帧上限与字节序；
- 单帧上限超限时的下游分片组装（设计 §9 缺口 2）。
