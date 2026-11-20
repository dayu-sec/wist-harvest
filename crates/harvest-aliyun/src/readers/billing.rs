//! 账单：`QueryAccountBill`（`business.aliyuncs.com`，`2017-12-14`，BSS OpenAPI）。
//!
//! 与资产读取器的三点不同：
//!
//! 1. **不分地域**：账单是账号级全局接口，不带 `RegionId`；
//! 2. 分页是 `PageNum` / `PageSize`（**1 起**），总数在 `Data.TotalCount`；
//! 3. 金额字段在行顶层（不是组件数组），但仍是十进制字符串，走 [`DecimalSum`]，
//!    **不碰浮点**；未知（字段缺失/空串）与 `"0"` 严格区分。
//!
//! 用 `Granularity=DAILY` 拉逐日明细：月口径与日口径都从同一批数据派生，
//! 避免同一个月用两种粒度采出两套口径不同的行。

use harvest_core::cost::{
    COST_DETAIL_SCHEMA, CostBasis, CostDetail, CostFetchStat, CostRange, DecimalSum, resource_key,
};
use harvest_core::error::{ErrorClass, HarvestError};
use serde_json::{Map, Value};

use crate::client::{AliyunClient, ApiAction, request_id};
use crate::value::{as_i64, as_str, inner_items};

pub const QUERY_ACCOUNT_BILL: ApiAction = ApiAction {
    name: "QueryAccountBill",
    host: "business.aliyuncs.com",
    version: "2017-12-14",
};

/// `PageSize` 上限。写死并断言，避免有人「顺手调大」后被云拒绝。
pub const PAGE_LIMIT: u64 = 300;
pub const DATA_KEY: &str = "Data";
pub const ITEMS_OUTER_KEY: &str = "Items";
pub const ITEMS_INNER_KEY: &str = "Item";

/// 抓一个范围的所有明细，逐行回调（有界内存）。
///
/// `on_row` 必须自己决定要不要（比如按 `BillingDate` 过滤日范围）；这里只负责
/// 「把整月完整分页拉一遍，并把每一行交给调用方」。
pub async fn fetch_range<F>(
    client: &AliyunClient,
    range: &CostRange,
    mut on_row: F,
) -> Result<CostFetchStat, HarvestError>
where
    F: FnMut(&Map<String, Value>) -> Result<(), HarvestError>,
{
    let month = range.month_label();
    let mut stat = CostFetchStat::default();
    let mut page: u64 = 1;
    let mut total: Option<u64> = None;

    loop {
        let params = [
            ("BillingCycle", month.clone()),
            ("Granularity", "DAILY".to_string()),
            ("IsGroupByProduct", "false".to_string()),
            ("PageNum", page.to_string()),
            ("PageSize", PAGE_LIMIT.to_string()),
        ];
        let outcome = client.call(&QUERY_ACCOUNT_BILL, None, &params).await?;
        stat.retries += u64::from(outcome.retries);
        if let Some(id) = request_id(&outcome.body) {
            stat.request_ids.push(id);
        }

        let data = outcome
            .body
            .get(DATA_KEY)
            .and_then(Value::as_object)
            .ok_or_else(|| {
                HarvestError::data_format(format!(
                    "{} 响应缺少 {DATA_KEY}",
                    QUERY_ACCOUNT_BILL.name
                ))
                .with_action(QUERY_ACCOUNT_BILL.name)
            })?;
        let page_total = as_i64(data.get("TotalCount")).ok_or_else(|| {
            HarvestError::data_format(format!(
                "{} 响应缺少 {DATA_KEY}.TotalCount",
                QUERY_ACCOUNT_BILL.name
            ))
            .with_action(QUERY_ACCOUNT_BILL.name)
        })? as u64;
        match total {
            None => total = Some(page_total),
            Some(previous) if previous != page_total => {
                return Err(HarvestError::integrity(format!(
                    "{} 分页总数前后不一致: 首页 {previous}，当前页 {page_total}",
                    QUERY_ACCOUNT_BILL.name
                ))
                .with_action(QUERY_ACCOUNT_BILL.name));
            }
            Some(_) => {}
        }

        let items = match inner_items(data, ITEMS_OUTER_KEY, ITEMS_INNER_KEY) {
            Some(items) => items,
            None if page_total == 0 => Vec::new(),
            None => {
                return Err(HarvestError::data_format(format!(
                    "{} 响应缺少 {DATA_KEY}.{ITEMS_OUTER_KEY}.{ITEMS_INNER_KEY}",
                    QUERY_ACCOUNT_BILL.name
                ))
                .with_action(QUERY_ACCOUNT_BILL.name));
            }
        };

        stat.pages += 1;
        for item in &items {
            let obj = item.as_object().ok_or_else(|| {
                HarvestError::data_format(format!("{ITEMS_INNER_KEY} 的元素不是对象"))
                    .with_action(QUERY_ACCOUNT_BILL.name)
            })?;
            stat.rows += 1;
            on_row(obj)?;
        }

        let fetched = (page - 1) * PAGE_LIMIT + items.len() as u64;
        // 空页 + 还有余额 = 分页断了，必须当失败（少一天账单留下的是永久空洞）。
        if items.is_empty() && fetched < page_total {
            return Err(HarvestError::integrity(format!(
                "{} 在 page {page} / total {page_total} 处返回空页",
                QUERY_ACCOUNT_BILL.name
            )));
        }
        // `PageNum` 分页按固定 PageSize 翻页：非末页返回短页时，下一页会**跳过**
        // 短掉的那段（例如第 1 页只给 10 条却按 300 条翻到第 2 页）。宁可中止，
        // 也不能少账；月账单的缺口是永久性的。
        if (items.len() as u64) < PAGE_LIMIT && fetched < page_total {
            return Err(HarvestError::integrity(format!(
                "{} 在 page {page} 返回 {} 条（PageSize {PAGE_LIMIT}）但 total 还有 {page_total}：\
                 短页会导致后续分页跳行",
                QUERY_ACCOUNT_BILL.name,
                items.len()
            )));
        }
        if fetched >= page_total {
            break;
        }
        page += 1;
    }

    // 最终行数必须与云端总数一致：多出来的行（`TotalCount` 少报）同样不能发布。
    if let Some(total) = total
        && stat.rows != total
    {
        return Err(HarvestError::integrity(format!(
            "{} 账单总数不一致：云端 TotalCount={total}，实收={}",
            QUERY_ACCOUNT_BILL.name, stat.rows
        ))
        .with_action(QUERY_ACCOUNT_BILL.name));
    }

    Ok(stat)
}

/// 把一条账单明细映射成契约的 `detail` 行。
///
/// 归日（契约 §3）：`CONSUMPTION` 取查询日；`CHARGE` 取**账单归属日** `BillingDate`。
/// 阿里云的 `QueryAccountBill`（`Granularity=DAILY`）把归属日放在 `BillingDate` 上，
/// 没有 `PayTime` 参与归日 —— 与腾讯云「不能用 PayTime 归日」的结论一致。
pub fn map_detail(
    obj: &Map<String, Value>,
    range: &CostRange,
    currency: &str,
) -> Result<Option<CostDetail>, HarvestError> {
    let bill_day = {
        let raw = as_str(obj.get("BillingDate"));
        raw.get(..10).unwrap_or_default().to_string()
    };

    if range.cost_basis == CostBasis::Consumption
        && bill_day != range.period_start.format("%Y-%m-%d").to_string()
    {
        return Ok(None);
    }

    let cost_date = match range.cost_basis {
        CostBasis::Consumption => range.period_start.format("%Y-%m-%d").to_string(),
        CostBasis::Charge => {
            if bill_day.is_empty() {
                return Err(HarvestError::new(
                    ErrorClass::DataFormat,
                    format!(
                        "CHARGE 范围 {} 的明细缺 BillingDate（InstanceID={}）：无可归日",
                        range.month_label(),
                        as_str(obj.get("InstanceID"))
                    ),
                ));
            }
            bill_day.clone()
        }
    };

    let bill_month = {
        let month = bill_day.get(..7).unwrap_or_default().to_string();
        if month.is_empty() {
            range.month_label()
        } else {
            month
        }
    };

    let business_code = as_str(obj.get("ProductCode"));
    let resource_id = {
        let id = as_str(obj.get("InstanceID"));
        if id.is_empty() { None } else { Some(id) }
    };

    let mut real = DecimalSum::default();
    real.add(opt(obj, "PretaxAmount").as_deref())?;
    let mut cash = DecimalSum::default();
    cash.add(opt(obj, "PaymentAmount").as_deref())?;
    // 阿里云：`DeductedByCashCoupons` 是代金券，`DeductedByCoupons` 是优惠券。
    let mut voucher = DecimalSum::default();
    voucher.add(opt(obj, "DeductedByCashCoupons").as_deref())?;
    let mut incentive = DecimalSum::default();
    incentive.add(opt(obj, "DeductedByCoupons").as_deref())?;

    let account = non_empty(obj, "BillAccountID");

    Ok(Some(CostDetail {
        schema: COST_DETAIL_SCHEMA.to_string(),
        cost_date,
        bill_month,
        business_code: business_code.clone(),
        business_code_name: non_empty(obj, "ProductName"),
        product_code: non_empty(obj, "ProductDetailCode"),
        product_code_name: non_empty(obj, "ProductDetail"),
        resource_id: resource_id.clone(),
        resource_key: resource_key(&business_code, resource_id.as_deref()),
        resource_name: None,
        payer_uin: account.clone(),
        owner_uin: account,
        pay_mode_name: non_empty(obj, "SubscriptionType"),
        action_type_name: non_empty(obj, "BillingItem"),
        // 阿里云逐日账单没有付款时刻；归日只看 BillingDate。
        pay_time: None,
        fee_begin_time: None,
        fee_end_time: None,
        // 该接口没有稳定的明细主键，不强造一个：下游按 (范围, 行内容) 去重。
        provider_detail_id: None,
        bill_id: non_empty(obj, "BillID"),
        order_id: None,
        currency: {
            let row_currency = as_str(obj.get("Currency"));
            if row_currency.is_empty() {
                currency.to_string()
            } else {
                row_currency
            }
        },
        real_cost: real.finish(),
        cash_amount: cash.finish(),
        voucher_amount: voucher.finish(),
        incentive_amount: incentive.finish(),
        // 储值卡抵扣没有对应的契约字段，只保留在 `raw` 里。
        transfer_amount: None,
        components: Vec::new(),
        raw: Value::Object(obj.clone()),
    }))
}

fn non_empty(obj: &Map<String, Value>, key: &str) -> Option<String> {
    let value = as_str(obj.get(key));
    if value.is_empty() { None } else { Some(value) }
}

/// 金额：空串按「未知」处理（`None`），不要变成 0 —— 契约里两者不是一回事。
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
    use harvest_core::model::Vendor;
    use serde_json::json;

    fn range(basis: CostBasis, start: &str, end: &str) -> CostRange {
        CostRange {
            source_code: "aliyun-prod".into(),
            vendor: Vendor::Aliyun,
            billing_account_uin: "1234567890".into(),
            cost_basis: basis,
            period_start: NaiveDate::parse_from_str(start, "%Y-%m-%d").unwrap(),
            period_end: NaiveDate::parse_from_str(end, "%Y-%m-%d").unwrap(),
        }
    }

    fn row() -> Map<String, Value> {
        json!({
            "BillAccountID": "1234567890",
            "SubscriptionType": "PayAsYouGo",
            "ProductCode": "ecs",
            "ProductName": "云服务器ECS",
            "ProductDetail": "云服务器ECS-按量付费",
            "ProductDetailCode": "ecs",
            "BillingItem": "云服务器ECS按量付费",
            "InstanceID": "i-example01",
            "BillingDate": "2026-10-15",
            "PretaxAmount": "12.345678",
            "PaymentAmount": "10.000000",
            "DeductedByCashCoupons": "1.000000",
            "DeductedByCoupons": "1.345678",
            "DeductedByPrepaidCard": "0",
            "Currency": "CNY",
            "BillID": "bill-001"
        })
        .as_object()
        .cloned()
        .unwrap()
    }

    #[test]
    fn a_bill_row_maps_to_the_contract_shape() {
        let r = range(CostBasis::Consumption, "2026-10-15", "2026-10-15");
        let detail = map_detail(&row(), &r, "CNY").unwrap().unwrap();
        assert_eq!(detail.cost_date, "2026-10-15");
        assert_eq!(detail.bill_month, "2026-10");
        assert_eq!(detail.business_code, "ecs");
        assert_eq!(detail.resource_key, "ecs:i-example01");
        assert_eq!(detail.payer_uin.as_deref(), Some("1234567890"));
        assert_eq!(detail.real_cost.as_deref(), Some("12.345678"));
        assert_eq!(detail.cash_amount.as_deref(), Some("10"));
        assert_eq!(detail.voucher_amount.as_deref(), Some("1"));
        assert_eq!(detail.incentive_amount.as_deref(), Some("1.345678"));
        assert_eq!(detail.provider_detail_id, None);
        assert_eq!(detail.currency, "CNY");
        assert_eq!(detail.raw["InstanceID"], "i-example01");
    }

    #[test]
    fn a_daily_range_keeps_only_that_day() {
        let r = range(CostBasis::Consumption, "2026-10-16", "2026-10-16");
        assert!(map_detail(&row(), &r, "CNY").unwrap().is_none());
    }

    #[test]
    fn a_charge_row_uses_its_billing_date() {
        let r = range(CostBasis::Charge, "2026-10-01", "2026-10-31");
        let detail = map_detail(&row(), &r, "CNY").unwrap().unwrap();
        assert_eq!(detail.cost_date, "2026-10-15");
    }

    #[test]
    fn a_charge_row_without_a_billing_date_fails_the_range() {
        let mut obj = row();
        obj.remove("BillingDate");
        let r = range(CostBasis::Charge, "2026-10-01", "2026-10-31");
        let err = map_detail(&obj, &r, "CNY").unwrap_err();
        assert!(err.message.contains("BillingDate"), "{}", err.message);
    }

    #[test]
    fn a_row_without_resource_id_gets_a_stable_key() {
        let mut obj = row();
        obj.remove("InstanceID");
        let r = range(CostBasis::Consumption, "2026-10-15", "2026-10-15");
        let detail = map_detail(&obj, &r, "CNY").unwrap().unwrap();
        assert_eq!(detail.resource_key, "ecs:UNALLOCATED");
        assert_eq!(detail.resource_id, None);
    }

    #[test]
    fn an_unparseable_amount_fails_the_row() {
        let mut obj = row();
        obj.insert("PretaxAmount".into(), json!("1,234.00"));
        let r = range(CostBasis::Consumption, "2026-10-15", "2026-10-15");
        let err = map_detail(&obj, &r, "CNY").unwrap_err();
        assert_eq!(err.class, ErrorClass::DataFormat);
    }

    #[test]
    fn unknown_amounts_stay_null() {
        let mut obj = row();
        obj.remove("PretaxAmount");
        obj.remove("PaymentAmount");
        let r = range(CostBasis::Consumption, "2026-10-15", "2026-10-15");
        let detail = map_detail(&obj, &r, "CNY").unwrap().unwrap();
        assert_eq!(detail.real_cost, None);
        assert_eq!(detail.cash_amount, None);
    }

    #[test]
    fn page_limit_stays_within_what_the_api_accepts() {
        const { assert!(PAGE_LIMIT <= 300) };
    }
}
