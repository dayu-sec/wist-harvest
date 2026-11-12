# Changelog

## Unreleased

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
