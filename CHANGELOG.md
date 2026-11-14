# Changelog

本文件记录 `wist-harvest` 的所有重要变更。格式遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/)，
版本号遵循[语义化版本](https://semver.org/lang/zh-CN/)。

## [0.4.0-alpha] - 2026-11-13

**采集开关可以自拉了：`[control]` 轮询中心配置（不经 gwlinkd）。**

「开不开」归中心（控制逻辑在中心），但执行不必经过本机代理 —— 采集器定时问一句就行，
gwlinkd 不需要认识 harvest。开关**不需要重启容器**，下一拍即生效。

```toml
[control]
url = "http://gateway:3001/internal/harvest"   # 空 = 不轮询（用 [service] enabled）
poll_interval_ms = 30000                       # 下限 1000

[control.credential]
bearer = { file = "/secrets/sec-value.yml", field = "harvest_control_token" }
```

契约 v1：`GET <url>` + `Authorization: Bearer <token>` → `200 {"enabled": true}`（`revision` 可选）。

失败语义刻意选的是 **fail-closed + 沿用上一拍**：

- 配了 `[control]` 时**首拍之前不采** —— 否则容器刚起来那一轮可能抢在轮询之前跑掉，
  等于「中心说关、实际采了一次」；
- 首拍失败（中心不可达 / 500 / 契约不符）→ 不采，日志一条 warn；
- 拿到过之后中心抖动 → 沿用上一拍的值，不因为一次失败就停采或放开；
- 本地 `[service] enabled = false` 仍然是**硬关**，远端说 true 也不采。

切换日志只在**状态翻转**时打一次（不逐轮刷）。`once` / `cost` 不参与该开关（手工动作）。

## [0.3.0-alpha] - 2026-11-13

**新增运行时开关 `[service] enabled`（默认 true）。**

「装不装」和「开不开」分开：容器常驻，采集是否调度由配置里的 `enabled` 决定。管理面关掉
采集时不必销毁容器、也不必重新部署 —— 改配置后重启即恢复。

- `enabled = false`：进程保持常驻、**不调度**（连 `--now` 也不跑），等退出信号；
- `diagnose`（healthcheck）在关闭态**只探活**，不再检查数据源 / 凭据 / 落点 ——
  关掉采集的站点本来就没有凭据，否则容器会以 unhealthy 无限重启；
- 默认 `true`：老配置文件没有这个键时行为不变。

**回退注意**：`[service]` 是 `deny_unknown_fields`，0.2.0 的二进制读到 `enabled` 会直接
解析失败。回退镜像时要同时回退渲染出来的 `harvest.toml`（或删掉这一行）。

## [0.2.0-alpha] - 2026-11-13

**采集器不再认识客户 / 租户。**

一个网关（数据面）只服务一个客户，客户身份由**发送通道**在入库时打上（wparse 的全局 tag
`tenant_id`，来自 gwlinkd 写入的 `WIST_TENANT_ID`），OML 用它拼 `cust_id` 投给 atlas。
采集器不需要、也不应该知道它 —— 旧设计把客户写进采集配置，等于把「一个客户一套采集器」
焊在配置里，而且换绑时在途快照会被打上新客户。

### 不兼容

- **`sources.d/<source_code>.toml` 里的 `tenant_id` 删掉**。`deny_unknown_fields` 下留着它
  会直接解析失败；启用判据改由栈侧的 `HARVEST_ENABLED` 决定（不再拿「租户非空」当开关）。
- **spool 目录**：`<spool>/<tenant>/<producer>/<snap_id>/` → `<spool>/<producer>/<snap_id>/`；
  租约文件同理去掉租户段。
- **快照体少一个 `tenant_id`**（`wist.harvest.snapshot.v1` 内容变化）：数据面的 WPL/OML 与
  atlas 的 inventory 适配器都**不读**它，故无消费方受影响。
- **成本产物**：`cost/{tenant}/{vendor}/…` → `cost/{vendor}/…`，manifest 的 `range` 少一个
  `tenant`；账号 → 客户 的映射下沉到 `wist-cost`（用 `billing_account_uin`）。

运行报告与 `--dry-run` 输出也不再带 `tenant_id`（保留 `source_code` / `vendor` / `producer`）。

## [0.1.1-alpha] - 2026-11-13

除版本号外**没有行为变化**。这次发布是为了把上一版没走到的制品路径走一遍：
构建与推送镜像用的 GitHub Actions 已从 Node 20 迁到 Node 24
（`v0.1.0-alpha` 发布时这三个 action 都报了弃用告警）。镜像里的 `wist-harvest` 自身行为不变。

用着 `v0.1.0-alpha` 的站点**不必**跟升级；下一次有功能变更时一起走就行。

## [0.1.0-alpha] - 2026-11-13

首个实际发布的版本。骨架写完时只写了 changelog 标题、没打 tag，所以这一次把骨架、
多云（腾讯云 + AWS）、成本链路与正确性修复一并发布。

### 制品

- 首个可部署制品：容器镜像 `ghcr.io/dayu-sec/wist-harvest`（国内镜像源
  `dy-sec.tencentcloudcr.com/cloud/wist-harvest`，linux amd64 / arm64），
  以及同 tag 的二进制 tarball（GitHub Release）。
- 镜像以 **10001:10001** 运行（与 `wist-field-stack` 同身份）、**不开任何入站端口**、
  不带 shell；健康检查用 `wist-harvest diagnose`（离线、不输出凭据），由栈的编排定义。

### 多云（P0 解耦 + P1 第二家云）

- **`vendor` 成为必填配置**：`sources.d` 里显式写 `vendor = "TENCENT_CLOUD" | "AWS"`；
  漏写/写错一律启动失败，不再“默认按腾讯云跑”。
- **厂商注册表**：`harvest-app::vendor` 按 `vendor` 构建 `Box<dyn Source>` / `Box<dyn CostSource>`；
  `dry-run` / `diagnose` 走不联网的 `plan_from_config`。app 不再直接依赖某个云的类型。
- **凭据表按厂商定义形状**：`[credential]` 是键值表（腾讯云 `secret_id`/`secret_key`，
  AWS `access_key_id`/`secret_access_key`/`session_token`），core 只负责「从哪读」与脱敏；
  厂商专属参数放 `[vendor_options]`，由对应 crate 用 `deny_unknown_fields` 强类型解析。
- **分页驱动下沉到 core**（`harvest-core::page`）：`PageFetcher` + 驱动负责逐页落 spool、
  总数前后一致、空页但还有下一页、游标不前进、页数上限；腾讯云的 `Limit`/`Offset`
  与 AWS 的 `NextToken` 各自实现协议。
- **完整性证据显式化**：`Completeness::{Total, Exhausted}` 写进 manifest；
  **没有总数的云（AWS）靠游标耗尽作为证据**，不存在“没 TotalCount 就不校验”的路径。
- **新增 `harvest-aws`**：SigV4 签名（独立 Python 固定向量）、EC2 Query XML reader
  （instances/addresses/vpcs/subnets）、STS 身份与 `DescribeRegions`（排除 `not-opted-in`）、
  `[vendor_options].partition`（`aws`/`aws-cn`/`aws-us-gov`）。AWS 的 EIP 只有一路。
- AWS 账单采集**未实现**：`cost` 对 AWS 明确报错，不静默按腾讯云跑。
- 配置字段 `billing_account_uin` 泛化为 `billing_account`（保留旧名作为 alias）。

### 正确性（不完整不发 / 不误删）

- **覆盖范围按类型收敛**：`regions="auto"` 但关掉了某一类资产时，`coverage.scope`
  自动从 `account` 降为 `partial`，空数组不再被当成「这类资产都没了」的退役证据。
  收敛逻辑放在 `Coverage::for_planned_types`（core 层），任何 `Source` 都绕不过。
- **计划变更不再死循环**：改了 `[assets]` 开关后旧 spool 会被标记 `Abandoned` 并开新
  `snap_id`，而不是每轮撞在同一个校验错误上、永不发布。新增 `RunState::Abandoned`。
- **续跑加年龄上限**：`service.max_resume_age_ms`（默认 24h）。永久性局部失败不会再把
  一份旧 spool 无限续下去（那会让采集器永不产出新快照且 `collected_at` 停在过去）。
- **组装前核对文件本身**：实际行数必须与 manifest 一致；spool 文件缺失/不匹配一律
  `Aborted`，不再静默当成 0 行发出去。
- **`allow_empty` 可按类型**：`allow_empty = ["EIP"]`，不必为养一个空类型把其余类型的
  「空 ≠ 失败」保护一起关掉。

### 成本链路

- **分片先暂存后入位**：抓取期间只写 `.staging-<batch_id>/`，完整成功后才 `rename`
  进最终对象键，manifest 最后原子替换。抓取失败不再污染已发布批次的字节
  （此前会覆写最终键，导致旧 manifest 的 sha256 与实际文件对不上、被 `wist-cost` 整批拒收）。
- 同一范围重发时清理新 manifest 未列出的旧分片，不留孤儿。
- **金额解析失败报错**：`DecimalSum::add` 不再把读不出来的金额静默归零（`"0"` 是
  「确认为零」，不能拿来兜底）；`parse_scaled` 拒绝 `+1.5` / `1e3` / 千分位等形态。
- `cost` 默认账期按 **Asia/Shanghai** 算：UTC 在月初 00:00–08:00 会多退一个月。
- manifest 写入 `request_ids`；manifest 本身改为临时文件 + `rename` 原子替换。

### 其他

- TLS 握手也纳入 `connect_timeout`，对端 TCP 通但握手卡住时不再挂死整轮。
- spool 目录名安全化（`region` / `lane` 拼路径前统一 `sanitize`）。
- 启动失败的报告改用 `start-<ts>.json`，不再反复覆盖同一份 `-.json`。
- 修正文档漂移：帧格式（ASCII 十进制长度）、`--account` 不存在的描述、
  `Interval` 语义、WPL 的 `chars:body`（无 NUL）。

### 运行（`run` / `diagnose`）

- 新增 `run` 常驻调子命令：`--run-at`（默认 00:30 Asia/Shanghai）或 `--interval`，
  叠加 `--jitter`（默认 5 分钟）错开多站点；单轮并发（上一轮未结束则跳过并 warn）；
  失败按指数退避重试且**不推迟下一次定点**；SIGTERM/SIGINT 优雅退出（退出码 0）。
- `diagnose` 改为容器 healthcheck 语义：离线、非交互、分项结果，新增 `--quiet`；
  数据平面端点只校验形态不做拨号。凭据只报来源，输出前统一脱敏。
- 抓取轮次支持取消：信号到达时在工作项边界中止，保持零帧。

### 骨架（P0，首版里一并带上）

P0 骨架：`inventory` 采集组端到端跑通。

- 腾讯云数据源：TC3-HMAC-SHA256 签名，云主机 / 公网 IP / VPC / 子网四个 reader，
  字段沿用既有命名与兼容常量（`ip`/`vpcid`/`subvpcid`、`version:"1"`、
  `risk_level:"MEDIUM"`、`enable:1`、`network_type:""`、VPC 根 `-1`）。
- 公网 IP 每地域两路分页（`address-type=WanIP` 与不过滤），按 `AddressId` 合并去重。
- `tags` 用显式三态承载「字段缺席 / null / []」，序列化保持原样。
- 采集流程：spool 落盘 + manifest，状态机 `Planned → … → Done`，
  任一步失败进 `Aborted` 且数据平面零帧；进程重启按 manifest 续跑。
- 限速按 action 维度的令牌桶；错误分类重试（限流/5xx/网络可重试，
  鉴权/参数/响应超限不重试），HTTP 200 里的 `Response.Error` 同样算失败。
- 数据平面 sink：TLS（可选 mTLS）+ 长度前缀帧，ACK 超时按未确认重发同一 `snap_id`。
- 凭据只从 `${SEC_*}` 或 0600 文件读，日志/报告/错误统一脱敏。
- CLI：`once` / `dry-run` / `diagnose`。
