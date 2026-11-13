//! 分页驱动：**协议在厂商，循环与完整性证据在 core**。
//!
//! 各云的分页协议差别很大：腾讯云/阿里云是 `Limit`+`Offset`+`TotalCount`，
//! AWS 是 `NextToken`，Azure 是 `nextLink`/`skipToken`。把这些协议强行做成配置
//! 会变成一个不可测的 DSL；正确的切法是：
//!
//! - 厂商实现 [`PageFetcher::fetch_page`]（怎么发请求、怎么取下一页游标）；
//! - core 负责：逐页交给 `on_page`（边拉边落 spool）、总数前后一致、空页但还有
//!   下一页直接失败、游标重复直接失败、页数上限、以及**完整性证据**（有总数 →
//!   `Total`；无总数 → 游标耗尽 `Exhausted`）。
//!
//! 这样"没采全"不会因为某家云不返回 `TotalCount` 就变成"不校验"。

use std::time::Instant;

use async_trait::async_trait;
use serde::Serialize;

use crate::error::HarvestError;
use crate::source::{Completeness, FetchStat};

/// 防御性页数上限：云端 `TotalCount` 异常或游标不前进时不至于把进程拖死。
pub const MAX_PAGES: u64 = 10_000;

/// 分页游标。对驱动层不透明（offset / nextToken / skipToken 都可以）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageCursor(pub String);

impl PageCursor {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 一页抓取结果。
#[derive(Debug, Clone)]
pub struct PageBatch<T> {
    pub rows: Vec<T>,
    /// 下一页游标；`None` = 云端已经没有下一页（这是 `Exhausted` 的证据）。
    pub next: Option<PageCursor>,
    /// 云端声明的总数（有就带，没有就 `None`）。
    pub total: Option<u64>,
    pub request_id: Option<String>,
    /// 本页内部发生的重试次数（由厂商的 HTTP 客户端统计）。
    pub retries: u32,
}

/// 一个可分页的云 API 调用。
#[async_trait]
pub trait PageFetcher: Send + Sync {
    type Row: Serialize + Send + Sync;

    /// `cursor == None` 表示第一页。
    async fn fetch_page(
        &self,
        cursor: Option<&PageCursor>,
    ) -> Result<PageBatch<Self::Row>, HarvestError>;
}

/// 驱动分页：逐页回调（写 spool）→ 汇总统计与完整性证据。
pub async fn drive<F, S>(fetcher: &F, mut on_page: S) -> Result<FetchStat, HarvestError>
where
    F: PageFetcher,
    S: FnMut(&[F::Row]) -> Result<(), HarvestError> + Send,
{
    let started = Instant::now();
    let mut cursor: Option<PageCursor> = None;
    let mut total: Option<u64> = None;
    let mut rows = 0u64;
    let mut pages = 0u64;
    let mut retries = 0u64;
    let mut request_ids: Vec<String> = Vec::new();

    loop {
        let batch = fetcher.fetch_page(cursor.as_ref()).await?;
        retries += u64::from(batch.retries);
        if let Some(id) = batch.request_id {
            request_ids.push(id);
        }

        // 云端总数必须前后一致：中途变少说明有写入竞争，这次结果不可作为全集。
        if let Some(page_total) = batch.total {
            match total {
                None => total = Some(page_total),
                Some(previous) if previous != page_total => {
                    return Err(HarvestError::integrity(format!(
                        "分页总数前后不一致：首页 {previous}，当前页 {page_total}"
                    )));
                }
                Some(_) => {}
            }
        }

        pages += 1;
        rows += batch.rows.len() as u64;
        on_page(&batch.rows)?;

        if let Some(expected) = total
            && rows > expected
        {
            return Err(HarvestError::integrity(format!(
                "实收行数 {rows} 超过云端总数 {expected}"
            )));
        }

        let Some(next) = batch.next else {
            break;
        };
        if batch.rows.is_empty() {
            // 空页但云端说还有下一页：分页断了。宁可失败，也不能把半份数据当全集。
            return Err(HarvestError::integrity(format!(
                "分页在 {rows} 行处返回空页，但云端仍有下一页"
            )));
        }
        if cursor.as_ref() == Some(&next) {
            return Err(HarvestError::data_format(format!(
                "分页游标没有前进（重复返回 {}）",
                next.as_str()
            )));
        }
        if pages >= MAX_PAGES {
            return Err(HarvestError::data_format(format!(
                "分页超过 {MAX_PAGES} 页，疑似服务端总数/游标异常"
            )));
        }
        cursor = Some(next);
    }

    if let Some(expected) = total
        && expected != rows
    {
        return Err(HarvestError::integrity(format!(
            "分页总数不一致：云端 TotalCount={expected}，实收={rows}"
        )));
    }

    let completeness = match total {
        Some(total) => Completeness::Total { total },
        None => Completeness::Exhausted,
    };

    Ok(FetchStat {
        rows,
        pages,
        completeness: Some(completeness),
        expected_rows: total,
        retries,
        request_ids,
        duration_ms: started.elapsed().as_millis() as u64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 按脚本回放若干页（用来测驱动层，不测具体协议）。
    struct Scripted {
        pages: Mutex<VecDeque<PageBatch<u32>>>,
    }

    impl Scripted {
        fn new(pages: Vec<PageBatch<u32>>) -> Self {
            Self {
                pages: Mutex::new(pages.into()),
            }
        }
    }

    #[async_trait]
    impl PageFetcher for Scripted {
        type Row = u32;

        async fn fetch_page(
            &self,
            _cursor: Option<&PageCursor>,
        ) -> Result<PageBatch<Self::Row>, HarvestError> {
            self.pages
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| HarvestError::data_format("脚本里没有更多页了"))
        }
    }

    fn page(rows: &[u32], next: Option<&str>, total: Option<u64>) -> PageBatch<u32> {
        PageBatch {
            rows: rows.to_vec(),
            next: next.map(PageCursor::new),
            total,
            request_id: Some("req".into()),
            retries: 0,
        }
    }

    async fn collect(fetcher: &Scripted) -> Result<(FetchStat, usize), HarvestError> {
        let mut seen = 0usize;
        let stat = drive(fetcher, |rows| {
            seen += rows.len();
            Ok(())
        })
        .await?;
        Ok((stat, seen))
    }

    #[tokio::test]
    async fn total_pagination_ends_when_total_is_reached() {
        let fetcher = Scripted::new(vec![
            page(&[1, 2], Some("2"), Some(5)),
            page(&[3, 4], Some("4"), Some(5)),
            page(&[5], None, Some(5)),
        ]);
        let (stat, seen) = collect(&fetcher).await.unwrap();
        assert_eq!(stat.rows, 5);
        assert_eq!(seen, 5);
        assert_eq!(stat.pages, 3);
        assert_eq!(stat.completeness, Some(Completeness::Total { total: 5 }));
        assert_eq!(stat.request_ids.len(), 3);
    }

    #[tokio::test]
    async fn cursor_pagination_is_complete_when_exhausted() {
        // 无总数的云：游标走到尽头就是完整性证据。
        let fetcher = Scripted::new(vec![page(&[1, 2], Some("a"), None), page(&[3], None, None)]);
        let (stat, _) = collect(&fetcher).await.unwrap();
        assert_eq!(stat.rows, 3);
        assert_eq!(stat.completeness, Some(Completeness::Exhausted));
        assert_eq!(stat.expected_rows, None);
    }

    #[tokio::test]
    async fn empty_page_with_more_pages_is_an_integrity_error() {
        let fetcher = Scripted::new(vec![page(&[], Some("a"), Some(10))]);
        let err = collect(&fetcher).await.unwrap_err();
        assert_eq!(err.class, crate::ErrorClass::Integrity);
        assert!(err.message.contains("空页"), "{}", err.message);
    }

    #[tokio::test]
    async fn total_must_not_change_between_pages() {
        let fetcher = Scripted::new(vec![
            page(&[1], Some("a"), Some(2)),
            page(&[2], None, Some(3)),
        ]);
        let err = collect(&fetcher).await.unwrap_err();
        assert!(err.message.contains("前后不一致"), "{}", err.message);
    }

    #[tokio::test]
    async fn final_row_count_must_match_total() {
        let fetcher = Scripted::new(vec![page(&[1, 2], None, Some(3))]);
        let err = collect(&fetcher).await.unwrap_err();
        assert!(err.message.contains("总数不一致"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_cursor_that_does_not_advance_is_rejected() {
        let fetcher = Scripted::new(vec![
            page(&[1], Some("same"), None),
            page(&[2], Some("same"), None),
        ]);
        let err = collect(&fetcher).await.unwrap_err();
        assert!(err.message.contains("没有前进"), "{}", err.message);
    }
}
