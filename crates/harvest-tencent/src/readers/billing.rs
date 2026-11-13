//! 账单：`DescribeBillDetail`（`billing.tencentcloudapi.com`，`2018-07-09`）。
//!
//! 与资产读取器的三点不同：
//!
//! 1. **不分地域**：账单是账号级全局接口，不带 `X-TC-Region`；
//! 2. **单页上限 300**（1000 会被 `InvalidParameterValue #Limit` 拒掉，实测）；
//! 3. 响应用 `Total` / `DetailSet`，不是 `TotalCount` / `XxxSet` —— 复用资产那套
//!    分页驱动会一路拼错，所以这里单独走一份循环。
//!
//! 一条明细是**一次扣费事件**（`ActionType` 如 `postpay_deduct_h`），金额散在
//! `ComponentSet[]` 里：顶层没有金额字段。所以标准化时按组件求和，全程走十进制字符串
//! （`DecimalSum`），不碰浮点。

use harvest_core::cost::{
    COST_DETAIL_SCHEMA, CostBasis, CostDetail, CostRange, DecimalSum, resource_key,
};
use harvest_core::error::{ErrorClass, HarvestError};
use serde_json::{Map, Value, json};

use crate::client::{ApiAction, TencentClient, request_id};
use crate::value::as_str;

pub const DESCRIBE_BILL_DETAIL: ApiAction = ApiAction {
    name: "DescribeBillDetail",
    host: "billing.tencentcloudapi.com",
    version: "2018-07-09",
    service: "billing",
};

/// `DetailSet`。单页硬上限 300，写死并断言，避免有人「顺手调大」后被云拒绝。
pub const PAGE_LIMIT: u64 = 300;
pub const SET_KEY: &str = "DetailSet";
pub const TOTAL_KEY: &str = "Total";

/// 一个范围抓下来多少行 / 多少页 / 多少重试。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BillFetchStat {
    pub rows: u64,
    pub pages: u64,
    pub retries: u64,
    /// 云 API `RequestId`（写入 manifest 供排障，不含凭据）。
    pub request_ids: Vec<String>,
}

/// 抓一个范围的所有明细，逐行回调（有界内存）。
///
/// `on_row` 必须自己决定要不要（比如按 `BillDay` 过滤日范围）；这里只负责
/// 「把整月完整分页拉一遍，并把每一行交给调用方」。
pub async fn fetch_range<F>(
    client: &TencentClient,
    range: &CostRange,
    mut on_row: F,
) -> Result<BillFetchStat, HarvestError>
where
    F: FnMut(&Map<String, Value>) -> Result<(), HarvestError>,
{
    let month = range.month_label();
    let mut stat = BillFetchStat::default();
    let mut offset: u64 = 0;
    let mut total: Option<u64> = None;

    loop {
        let body = json!({
            "Month": month,
            // `byDay` 给的是逐日粒度的扣费明细，日/月两种口径都从它派生，
            // 免得同一批数据用两种粒度采出两套口径不同的行。
            "PeriodType": "byDay",
            "Offset": offset,
            "Limit": PAGE_LIMIT,
            "NeedRecordNum": 1,
        });
        let outcome = client.call(&DESCRIBE_BILL_DETAIL, None, &body).await?;
        stat.retries += u64::from(outcome.retries);
        if let Some(id) = request_id(&outcome.body) {
            stat.request_ids.push(id);
        }

        let page_total = outcome
            .body
            .get(TOTAL_KEY)
            .and_then(Value::as_i64)
            .ok_or_else(|| {
                HarvestError::data_format(format!(
                    "{} 响应缺少 {TOTAL_KEY}",
                    DESCRIBE_BILL_DETAIL.name
                ))
                .with_action(DESCRIBE_BILL_DETAIL.name)
            })? as u64;
        match total {
            None => total = Some(page_total),
            Some(previous) if previous != page_total => {
                return Err(HarvestError::integrity(format!(
                    "{} 分页总数前后不一致: 首页 {previous}，当前页 {page_total}",
                    DESCRIBE_BILL_DETAIL.name
                ))
                .with_action(DESCRIBE_BILL_DETAIL.name));
            }
            Some(_) => {}
        }

        let items = outcome
            .body
            .get(SET_KEY)
            .and_then(Value::as_array)
            .ok_or_else(|| {
                HarvestError::data_format(format!(
                    "{} 响应缺少数组 {SET_KEY}",
                    DESCRIBE_BILL_DETAIL.name
                ))
            })?;

        stat.pages += 1;
        for item in items {
            let obj = item
                .as_object()
                .ok_or_else(|| HarvestError::data_format(format!("{SET_KEY} 的元素不是对象")))?;
            stat.rows += 1;
            on_row(obj)?;
        }

        offset += items.len() as u64;
        // 空页 + 还有余额 = 分页断了，必须当失败（少一天账单留下的是永久空洞）。
        if items.is_empty() && offset < page_total {
            return Err(HarvestError::integrity(format!(
                "{} 在 offset {offset} / total {page_total} 处返回空页",
                DESCRIBE_BILL_DETAIL.name
            )));
        }
        if offset >= page_total {
            break;
        }
    }

    Ok(stat)
}

/// 把一条账单明细映射成契约的 `detail` 行。
///
/// 归日（契约 §3）：`CONSUMPTION` 取查询日；`CHARGE` 取**账单归属日** `BillDay`。
///
/// 为什么不是 `PayTime`：月底结算会跨零点（实测：`BillDay=2026-10-31` 的行
/// `PayTime=2026-11-01 00:05`）。按付款时刻归日会把 10 月账单的 196 行推到 11 月 ——
/// 而 11 月的查询根本不包含它们，两边的钱都对不上。`pay_time` 作为属性保留在行里。
pub fn map_detail(
    obj: &Map<String, Value>,
    range: &CostRange,
    currency: &str,
) -> Result<Option<CostDetail>, HarvestError> {
    let bill_day = as_str(obj.get("BillDay"));
    // 日范围：整月拉下来后按 `BillDay` 过滤（`byDay` 的粒度就是日）。
    if range.cost_basis == CostBasis::Consumption {
        let day = bill_day.get(..10).unwrap_or_default();
        if day != range.period_start.format("%Y-%m-%d").to_string() {
            return Ok(None);
        }
    }

    let cost_date = match range.cost_basis {
        CostBasis::Consumption => range.period_start.format("%Y-%m-%d").to_string(),
        CostBasis::Charge => {
            // `BillDay` 是账单自己的归属日；`FeeBeginTime` 兜底；都没有就无法归日。
            let attribution = ["BillDay", "FeeBeginTime"].into_iter().find_map(|key| {
                let value = as_str(obj.get(key));
                let day = value.get(..10).unwrap_or_default().to_string();
                if day.is_empty() { None } else { Some(day) }
            });
            attribution.ok_or_else(|| {
                HarvestError::new(
                    ErrorClass::DataFormat,
                    format!(
                        "CHARGE 范围 {} 的明细缺 BillDay / FeeBeginTime（Id={}）：无可归日",
                        range.month_label(),
                        as_str(obj.get("Id"))
                    ),
                )
            })?
        }
    };

    let bill_month = {
        let raw = as_str(obj.get("BillMonth"));
        let month = raw.get(..7).unwrap_or_default().to_string();
        if month.is_empty() {
            range.month_label()
        } else {
            month
        }
    };

    let business_code = as_str(obj.get("BusinessCode"));
    let resource_id = {
        let id = as_str(obj.get("ResourceId"));
        if id.is_empty() { None } else { Some(id) }
    };

    let components: Vec<Value> = obj
        .get("ComponentSet")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // 组件金额求和；顶层没有金额字段（实测 `DescribeBillDetail` 就是这么给的）。
    let mut real = DecimalSum::default();
    let mut cash = DecimalSum::default();
    let mut voucher = DecimalSum::default();
    let mut incentive = DecimalSum::default();
    let mut transfer = DecimalSum::default();
    let mut currency_from_row = String::new();
    for component in &components {
        let Some(component) = component.as_object() else {
            continue;
        };
        real.add(opt(component, "RealCost").as_deref())?;
        cash.add(opt(component, "CashPayAmount").as_deref())?;
        voucher.add(opt(component, "VoucherPayAmount").as_deref())?;
        incentive.add(opt(component, "IncentivePayAmount").as_deref())?;
        transfer.add(opt(component, "TransferPayAmount").as_deref())?;
        if currency_from_row.is_empty() {
            currency_from_row = as_str(component.get("Currency"));
        }
    }

    Ok(Some(CostDetail {
        schema: COST_DETAIL_SCHEMA.to_string(),
        cost_date,
        bill_month,
        business_code: business_code.clone(),
        business_code_name: non_empty(obj, "BusinessCodeName"),
        product_code: non_empty(obj, "ProductCode"),
        product_code_name: non_empty(obj, "ProductCodeName"),
        resource_id: resource_id.clone(),
        resource_key: resource_key(&business_code, resource_id.as_deref()),
        resource_name: non_empty(obj, "ResourceName"),
        payer_uin: non_empty(obj, "PayerUin"),
        owner_uin: non_empty(obj, "OwnerUin"),
        pay_mode_name: non_empty(obj, "PayModeName"),
        action_type_name: non_empty(obj, "ActionTypeName"),
        pay_time: non_empty(obj, "PayTime"),
        fee_begin_time: non_empty(obj, "FeeBeginTime"),
        fee_end_time: non_empty(obj, "FeeEndTime"),
        provider_detail_id: non_empty(obj, "Id"),
        bill_id: non_empty(obj, "BillId"),
        order_id: non_empty(obj, "OrderId"),
        // 组件上的 `Currency` 在国内账号上是空串，所以以配置为准、行内值优先。
        currency: if currency_from_row.is_empty() {
            currency.to_string()
        } else {
            currency_from_row
        },
        real_cost: real.finish(),
        cash_amount: cash.finish(),
        voucher_amount: voucher.finish(),
        incentive_amount: incentive.finish(),
        transfer_amount: transfer.finish(),
        components,
        raw: Value::Object(obj.clone()),
    }))
}

fn non_empty(obj: &Map<String, Value>, key: &str) -> Option<String> {
    let value = as_str(obj.get(key));
    if value.is_empty() { None } else { Some(value) }
}

/// 组件金额：空串按「未知」处理（`None`），不要变成 0 —— 契约里两者不是一回事。
fn opt(obj: &Map<String, Value>, key: &str) -> Option<String> {
    let value = as_str(obj.get(key));
    if value.trim().is_empty() {
        None
    } else {
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use harvest_core::cost::CostBasis;
    use harvest_core::model::Vendor;

    fn range(basis: CostBasis, start: (i32, u32, u32), end: (i32, u32, u32)) -> CostRange {
        CostRange {
            tenant: "DEV-X4".into(),
            source_code: "tencent-cloud-prod".into(),
            vendor: Vendor::TencentCloud,
            billing_account_uin: "100041355715".into(),
            cost_basis: basis,
            period_start: NaiveDate::from_ymd_opt(start.0, start.1, start.2).unwrap(),
            period_end: NaiveDate::from_ymd_opt(end.0, end.1, end.2).unwrap(),
        }
    }

    /// 真实响应的一条（字段名与取值形状照抄线上）。
    fn row() -> Map<String, Value> {
        json!({
            "Id": "2105344359775739904",
            "PayerUin": "100041355715",
            "OwnerUin": "100041355715",
            "BusinessCodeName": "容器镜像服务 TCR",
            "BusinessCode": "p_tcr",
            "ProductCodeName": "容器镜像服务企业版",
            "ProductCode": "sp_tcr_enterprise",
            "PayModeName": "按量计费",
            "ResourceId": "tcr-bxyfnocf",
            "ResourceName": "",
            "ActionTypeName": "按量计费小时结",
            "OrderId": "20261001715220826682472",
            "BillId": "20261001715220826682582",
            "PayTime": "2026-10-01 01:03:37",
            "FeeBeginTime": "2026-10-01 00:00:00",
            "FeeEndTime": "2026-10-01 00:59:59",
            "BillDay": "2026-10-01 00:00:00",
            "BillMonth": "2026-10-01 00:00:00",
            "ComponentSet": [{
                "ComponentCode": "v_tcr_enterprise_spec",
                "RealCost": "0.97000000",
                "CashPayAmount": "0.97000000",
                "VoucherPayAmount": "0",
                "IncentivePayAmount": "0",
                "TransferPayAmount": "0",
                "Currency": ""
            }]
        })
        .as_object()
        .unwrap()
        .clone()
    }

    #[test]
    fn a_bill_row_maps_to_the_contract_shape() {
        let row = row();
        let detail = map_detail(
            &row,
            &range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31)),
            "CNY",
        )
        .unwrap()
        .expect("整月范围不该被过滤掉");

        assert_eq!(detail.schema, COST_DETAIL_SCHEMA);
        assert_eq!(
            detail.cost_date, "2026-10-01",
            "CHARGE 取 BillDay（账单归属日）"
        );
        assert_eq!(detail.bill_month, "2026-10");
        assert_eq!(detail.business_code, "p_tcr");
        assert_eq!(detail.resource_key, "p_tcr:tcr-bxyfnocf");
        assert_eq!(
            detail.provider_detail_id.as_deref(),
            Some("2105344359775739904")
        );
        // 金额是十进制字符串，并且是按组件求和来的。
        assert_eq!(detail.real_cost.as_deref(), Some("0.97"));
        assert_eq!(detail.cash_amount.as_deref(), Some("0.97"));
        assert_eq!(
            detail.voucher_amount.as_deref(),
            Some("0"),
            "\"0\" 与 null 必须分开"
        );
        // 组件 Currency 是空串时回落配置值。
        assert_eq!(detail.currency, "CNY");
        assert_eq!(detail.components.len(), 1, "组件原文不丢");
        assert!(detail.raw.get("Id").is_some(), "raw 留整条原文");
        // 空字符串的可选字段输出 None，不是 Some("")。
        assert!(detail.resource_name.is_none());
    }

    #[test]
    fn a_daily_range_keeps_only_that_day() {
        let day = range(CostBasis::Consumption, (2026, 10, 2), (2026, 10, 2));
        assert!(
            map_detail(&row(), &day, "CNY").unwrap().is_none(),
            "10-01 的行不属于 10-02 的范围"
        );
        let other = range(CostBasis::Consumption, (2026, 10, 1), (2026, 10, 1));
        let detail = map_detail(&row(), &other, "CNY").unwrap().unwrap();
        assert_eq!(detail.cost_date, "2026-10-01", "CONSUMPTION 取查询日");
    }

    #[test]
    fn a_settlement_that_crosses_midnight_stays_in_its_bill_month() {
        // 实测：月底结算会跨零点（BillDay 10-31，PayTime 11-01 00:05）。
        // 按付款时刻归日会把 10 月账单推到 11 月，而 11 月的查询不包含这些行 ——
        // 两边的钱都对不上，且范围校验会整批拒收。
        let mut row = row();
        row.insert("BillDay".into(), json!("2026-10-31 00:00:00"));
        row.insert("PayTime".into(), json!("2026-11-01 00:05:02"));
        let detail = map_detail(
            &row,
            &range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31)),
            "CNY",
        )
        .unwrap()
        .unwrap();
        assert_eq!(detail.cost_date, "2026-10-31", "按账单归属日，不按付款时刻");
        assert_eq!(
            detail.pay_time.as_deref(),
            Some("2026-11-01 00:05:02"),
            "付款时刻仍是事实，只是不用来归日"
        );
    }

    #[test]
    fn a_charge_row_without_any_attribution_date_fails_the_range() {
        let mut row = row();
        row.remove("BillDay");
        row.remove("FeeBeginTime");
        let err = map_detail(
            &row,
            &range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31)),
            "CNY",
        )
        .expect_err("没有归属日不能猜");
        assert!(format!("{err}").contains("BillDay"), "实际：{err}");
    }

    #[test]
    fn a_row_without_resource_id_gets_a_stable_key() {
        let mut row = row();
        row.remove("ResourceId");
        let detail = map_detail(
            &row,
            &range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31)),
            "CNY",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            detail.resource_key, "p_tcr:UNALLOCATED",
            "未归集费用必须有稳定键，否则下游无处可挂"
        );
    }

    #[test]
    fn an_unparseable_component_amount_fails_the_row() {
        // 金额读不出来必须报错；静默归零会把「未知」写成「确认为零」。
        let mut bad = row();
        bad.get_mut("ComponentSet").unwrap().as_array_mut().unwrap()[0]["RealCost"] = json!("1e5");
        let err = map_detail(
            &bad,
            &range(CostBasis::Charge, (2026, 10, 1), (2026, 10, 31)),
            "CNY",
        )
        .expect_err("金额解析失败不能静默");
        assert!(format!("{err}").contains("十进制"), "实际：{err}");
    }

    #[test]
    fn page_limit_stays_within_what_the_api_accepts() {
        // 实测：Limit=1000 会被 InvalidParameterValue #Limit 拒掉，300 可以。
        assert_eq!(PAGE_LIMIT, 300);
    }
}
