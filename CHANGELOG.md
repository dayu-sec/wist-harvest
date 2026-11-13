# Changelog

## Unreleased

### 交付（CI 与运行镜像）

- **补上发布流水线**（与 `wist-gateway` 同一套模板，只改产物名）：
  - `.github/workflows/build-and-test.yml`：`cargo fmt --check` → `build --release` → `test --release`
    （ubuntu-24.04 + macos-15），另起 coverage job 传 Codecov；
  - `.github/workflows/release.yml`：推 `v*.*.*` tag 时按平台**原生**编译三个目标
    （linux amd64/arm64、macos arm64）→ tarball；按架构各推一个单平台镜像
    （ghcr + 腾讯云 TCR）→ 合并 manifest；发 GitHub Release；可选推 COS
    （带 `x-cos-meta-artifact-sha256`）；
  - `.github/dependabot.yml`（cargo + github-actions）。
- **新增 `docker/Dockerfile`**：`distroless/cc-debian13` + 预编译二进制，**整个文件没有一个 `RUN`**
  （否则 arm64 那侧要在 QEMU 下跑 apt-get）。与 gateway 的差别只有三处，都是为了「采集器」：
  运行身份 **10001:10001**（与 `wist-field-stack` 的 compose 同值）、
  **不 EXPOSE**（没有入站面）、**不内置 HEALTHCHECK**（`diagnose` 必须带栈给的 `--config`/`--source`）。
- 与 gateway 的有意偏离：没有跨仓检出（用例全用仓内 fixture，拉别仓只会被别仓的分支染红）；
  Codecov 上传失败不阻断构建（新仓配 `CODECOV_TOKEN` 之前不该因“覆盖率没传上去”变红）。

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
- 修掉 clippy `-D warnings` 的测试代码告警，门禁恢复绿色。

- 新增 `run` 常驻调子命令：`--run-at`（默认 00:30 Asia/Shanghai）或 `--interval`，
  叠加 `--jitter`（默认 5 分钟）错开多站点；单轮并发（上一轮未结束则跳过并 warn）；
  失败按指数退避重试且**不推迟下一次定点**；SIGTERM/SIGINT 优雅退出（退出码 0）。
- `diagnose` 改为容器 healthcheck 语义：离线、非交互、分项结果，新增 `--quiet`；
  数据平面端点只校验形态不做拨号。凭据只报来源，输出前统一脱敏。
- `harvest-app` 拆出 lib，四个子命令的逻辑都可在集成测试里直接断言。
- 抓取轮次支持取消（`RunOptions.cancel`）：信号到达时在工作项边界中止，保持零帧。

## 0.1.0 — 2026-11-12

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
