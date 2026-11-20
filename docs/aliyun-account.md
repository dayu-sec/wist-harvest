# 阿里云只读账号与权限（wist-harvest）

> 目标：给 `wist-harvest` 一个**只读 RAM 用户**，权限刚好够采资产；需要成本时再单独加账单只读。
> 适用：`wist-harvest >= 0.8.0`（`vendor = "ALIYUN"`）。

## 0. 快速清单

1. 建一个 RAM 用户，**只开 OpenAPI 调用访问**，不要控制台登录；
2. 挂 `AliyunECSReadOnlyAccess` + `AliyunVPCReadOnlyAccess`（成本再挂 `AliyunBSSReadOnlyAccess`）；
3. 把 AccessKey 写进 `.wist/sec-value.yml` 的 `ali_secretid` / `ali_secretkey`；
4. 数据源写 `vendor = "ALIYUN"`、`source_code = "aliyun-prod"`；
5. 用 `aliyun ecs DescribeInstances` 和 `wist-harvest once` 各验一次。

## 1. 采集器实际调用哪些接口

资产链路（库存）：

| 资产 | 阿里云 API | RAM action |
| --- | --- | --- |
| `CLOUD_HOST` | `ecs.DescribeInstances` | `ecs:DescribeInstances` |
| 地域解析（`regions = "auto"`） | `ecs.DescribeRegions` | `ecs:DescribeRegions` |
| `EIP` | `vpc.DescribeEipAddresses` | `vpc:DescribeEipAddresses` |
| `VPC` | `vpc.DescribeVpcs` | `vpc:DescribeVpcs` |
| `SUB_NET` | `vpc.DescribeVSwitches` | `vpc:DescribeVSwitches` |
| 账号身份（覆盖范围 / 退役判定） | `sts.GetCallerIdentity` | 不需要授权 |

成本链路（只有跑 `cost` 子命令才用）：

| 数据 | 阿里云 API | RAM action |
| --- | --- | --- |
| 账号账单 | `bssopenapi.QueryAccountBill` | `bss:QueryAccountBill` |

> `sts:GetCallerIdentity` 不需要显式授权。拿不到它不会阻断采集，但本轮快照不会带
> `coverage.account`，下游**不会用它做退役判定**。

## 2. 创建 RAM 用户

### 控制台

1. 主账号登录 → **访问控制 RAM** → 身份管理 → 用户 → **创建用户**；
2. 登录名建议按站点/客户命名，例如 `harvest-readonly-<site>`；
3. 访问方式：
   - ✅ **OpenAPI 调用访问**（创建 AccessKey）
   - ❌ 不要勾选控制台访问（采集器不需要登录控制台）
4. 创建后立即保存 `AccessKey ID` / `AccessKey Secret`（Secret 只显示一次）；
5. 在「权限管理」里授权（见下一节）。

### CLI（主账号凭据执行）

```bash
aliyun ram CreateUser --UserName harvest-readonly-<site>
aliyun ram CreateAccessKey --UserName harvest-readonly-<site>
```

`CreateAccessKey` 的输出里是 `AccessKeyId` / `AccessKeySecret`。

## 3. 授权

### 3.1 系统策略（最省事）

| 策略 | 覆盖 |
| --- | --- |
| `AliyunECSReadOnlyAccess` | ECS 实例、地域 |
| `AliyunVPCReadOnlyAccess` | EIP / VPC / 交换机 |
| `AliyunBSSReadOnlyAccess` | 账单（只有 `cost` 需要） |

```bash
aliyun ram AttachPolicyToUser --PolicyType System \
  --PolicyName AliyunECSReadOnlyAccess --UserName harvest-readonly-<site>
aliyun ram AttachPolicyToUser --PolicyType System \
  --PolicyName AliyunVPCReadOnlyAccess --UserName harvest-readonly-<site>
aliyun ram AttachPolicyToUser --PolicyType System \
  --PolicyName AliyunBSSReadOnlyAccess --UserName harvest-readonly-<site>
```

### 3.2 自定义最小权限（推荐）

只授采集器真正调用的 5 个资产接口：

```json
{
  "Version": "1",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": [
        "ecs:DescribeRegions",
        "ecs:DescribeInstances",
        "vpc:DescribeEipAddresses",
        "vpc:DescribeVpcs",
        "vpc:DescribeVSwitches"
      ],
      "Resource": "*"
    }
  ]
}
```

成本单独一条：

```json
{
  "Version": "1",
  "Statement": [
    {
      "Effect": "Allow",
      "Action": ["bss:QueryAccountBill"],
      "Resource": "*"
    }
  ]
}
```

> BSS 的 action 名建议先用 RAM 的「策略模拟器」验证一次；不想确认就直接挂系统策略
> `AliyunBSSReadOnlyAccess`。
>
> 另：成本契约（`harvest-cost-v1`）的 vendor 枚举目前由 `wist-cost` 维护，阿里云账单
> 进 `wist-cost` 前需要把它的 `Vendor` 枚举扩到 `ALIYUN`；否则 manifest 会被拒收。

### 3.3 可选的收紧项

- **限制来源 IP**（采集器出口 IP 固定时）：

  ```json
  "Condition": { "IpAddress": { "acs:SourceIp": ["203.0.113.10/32"] } }
  ```

- **不要用地域条件去卡权限**：`acs:RequestedRegion` 不是所有 ECS/VPC 接口都生效。
  地域边界请写在数据源的 `regions` 里；如果确实用了地域条件，`regions` 必须限制在
  允许的集合内，否则 `regions = "auto"` 会打到被拒的地域并以 `Forbidden.RAM` 中止整轮。

- 检查 Permission Boundary / 资源目录 SCP 没有额外拦截。

## 4. 接入 wist-harvest

### 4.1 单机 / 源码运行

`[credential]` 只写「从哪读」，不写值：

```toml
vendor = "ALIYUN"
# 账单口径（只有跑 cost 需要）。这是账号 ID，不是 AccessKey。
billing_account = "1234567890"

[credential]
access_key_id     = { env = "SEC_ALIYUN_ACCESS_KEY_ID" }
access_key_secret = { env = "SEC_ALIYUN_ACCESS_KEY_SECRET" }
# 或 0600 文件：
# access_key_id     = { file = "/etc/wist-harvest/secrets/aliyun.json", field = "access_key_id" }
# access_key_secret = { file = "/etc/wist-harvest/secrets/aliyun.json", field = "access_key_secret" }
```

完整样例见 [`examples/aliyun-prod.toml`](examples/aliyun-prod.toml)。

### 4.2 网关栈（`wist-gateway-stack`）

密钥与网关共用 `.wist/sec-value.yml`（`gops sec` 维护，`0640`、属组 = 容器 gid）：

```yaml
ali_secretid: LTAI...
ali_secretkey: ...
```

站点值（`values/<sys>/sys_value.yml` 或 `values/value.yml`）：

```yaml
HARVEST_VENDOR: ALIYUN
HARVEST_SOURCE_CODE: aliyun-prod
```

栈会渲染成：

```toml
vendor = "ALIYUN"

[credential]
access_key_id     = { file = "/secrets/sec-value.yml", field = "ali_secretid" }
access_key_secret = { file = "/secrets/sec-value.yml", field = "ali_secretkey" }
```

三条纪律：

1. **密钥不进 `.env` / compose 环境**，否则会出现在 `docker inspect` 里；
2. **启用采集时换厂商必须换 `HARVEST_SOURCE_CODE`**：它是下游的资源匹配 / 作用域隔离键，
   沿用 `tencent-cloud-prod` 去采阿里云等于静默错标数据源（`init-harvest.sh` 会拒绝）；
   未启用时允许先选厂商（源文件只渲染注释）；
3. `source_code` 变了就是**新的采集器身份**：旧客户端证书不能复用，需要重新接入
   （见 §6.4）。

## 5. 验证

### 5.1 用阿里云 CLI 验权限

```bash
# 账号 ID（不需要额外权限）
aliyun sts GetCallerIdentity

# 地域
aliyun ecs DescribeRegions

# 资产：各取 1 条，重点看有没有 Forbidden.RAM
aliyun ecs DescribeInstances --RegionId cn-hangzhou --PageSize 1
aliyun vpc DescribeEipAddresses --RegionId cn-hangzhou --PageSize 1
aliyun vpc DescribeVpcs --RegionId cn-hangzhou --PageSize 1
aliyun vpc DescribeVSwitches --RegionId cn-hangzhou --PageSize 1

# 账单（只有 cost 需要）
aliyun bssopenapi QueryAccountBill --BillingCycle 2026-10 \
  --Granularity DAILY --PageNum 1 --PageSize 1 --IsGroupByProduct false
```

### 5.2 用采集器验

```bash
# 离线自检：配置、凭据文件、目录、端点形态（不调云 API）
wist-harvest diagnose --config ... --source ... --quiet

# 真实采一轮：退出码 0 = Done；2 = Aborted（看报告）；3 = 配置/凭据/数据源初始化失败
wist-harvest once --config ... --source ...
```

网关栈里：

```bash
docker exec wist-gateway-stack-harvest /usr/local/bin/wist-harvest diagnose \
  --config /etc/wist-harvest/harvest.toml \
  --source /etc/wist-harvest/sources.d/source.toml --quiet

docker exec wist-gateway-stack-harvest /usr/local/bin/wist-harvest once \
  --config /etc/wist-harvest/harvest.toml \
  --source /etc/wist-harvest/sources.d/source.toml
```

期望 `once` 输出 `"state": "Done"`、`"published": true`。

## 6. 常见错误

### 6.1 `Forbidden.RAM`

```text
[auth] Forbidden.RAM: User not authorized to operate on the specified resource,
or this API doesn't support RAM. (action=DescribeInstances)
```

含义：这个 AccessKey 所属的 RAM 用户没有对应 action 权限。

> RAM 权限不足**不会**让容器启动失败：`harvest` 仍然会启动并保持 healthy，只是每一轮采
> 集会 `Aborted`（零帧，报告里记 `[auth]`）。启动失败优先看 `harvest-bootstrap` 日志
> （身份 / 网关 / standing），不要把两者混在一起。

排查顺序：

1. 确认 AccessKey 属于哪个 RAM 用户（RAM → 用户 → 认证管理 → AccessKey）；
2. `aliyun ram ListPoliciesForUser --UserName <user>` 看策略是否真的挂上；
3. 检查 Permission Boundary / SCP / 自定义策略里的地域或资源条件；
4. 注意 `DescribeRegions`、`GetCallerIdentity` 成功**不代表**有 `DescribeInstances` 权限。

### 6.2 `Throttling`

云侧限流。采集器会按 `ErrorClass::RateLimited` 重试；持续出现就把
`harvest.toml` 的 `[rate] default_qps` 调低。

### 6.3 `InvalidAccessKeyId` / `SignatureDoesNotMatch`

- AccessKey 写错、被禁用或已删除；
- Secret 更新后没有同步到 `.wist/sec-value.yml`；
- 主机时间偏差过大（签名时间窗），校准 NTP。

### 6.4 `harvest_worker_not_found`（换 `source_code` 后）

```text
建立 standing 返回 404：{"code":"harvest_worker_not_found",
  "message":"unknown harvest worker harvest:aliyun-prod"}
```

原因：`harvest-run/state/identity/client.crt` 还是旧 `source_code` 的证书，bootstrap
看到「身份已存在」就跳过重新接入，但网关里没有新 worker。

`wist-harvest >= 0.8.1`：bootstrap 会读证书 CN，发现与当前 `source_code` 不一致时，
先把 `identity/` 改名备份（`identity.bak-<时间>-<pid>`），再重新接入；不需要人工删文件。

`0.8.0` 及更早：手动处理（栈根）：

```bash
mv harvest-run/state/identity harvest-run/state/identity.<old-source>.bak
gops run start
```

bootstrap 会用 admin token 现签券、换新证书，并建立初始 standing。确认新证书：

```bash
openssl x509 -in harvest-run/state/identity/client.crt -noout -subject
# subject=CN=harvest:aliyun-prod
```

### 6.5 某类资产为空导致 `Aborted`

采集器默认 `allow_empty = false`：某一类资产（例如账号里确实没有 EIP）整类 0 行时，
这一轮会 `Aborted`，避免把「没采到」当成「都不存在了」。

如果账号确实没有该类资产，在 `harvest.toml` 显式放行对应类型：

```toml
allow_empty = ["EIP"]     # 或 true（会同时关掉四类的保护）
```

## 7. 安全与运维

- **一个站点一个 RAM 用户**，不要多个网关共用同一对 AccessKey；
- 不开控制台登录，只保留编程访问；
- AccessKey 建议 90 天轮换：新建第二把 → 更新 `.wist/sec-value.yml` → 跑一轮确认 →
  禁用/删除旧 Key（采集器每轮重新读凭据，不需要重启容器）；
- 开启 ActionTrail，保留 RAM 与 OpenAPI 调用审计；
- 账单是**账号级**权限：`QueryAccountBill` 查的是当前账号的账单。资源目录成员账号的
  账单如果由管理账号统一出账，当前实现没有传 `BillOwnerId`，需要额外适配；
- `billing_account` 是**账号 ID**（用于账单范围与下游映射），不是 AccessKey。
