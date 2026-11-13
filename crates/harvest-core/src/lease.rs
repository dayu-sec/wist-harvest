//! 运行期租约：单生产者。
//!
//! 误扩容（`replicas=2`）会让同一范围被双采，双方互相当成"对方消失"。
//! P0 用文件锁（`flock`）守住：拿不到锁就不采，直接退出。
//! 锁随进程退出自动释放，不会留下需要人工清理的陈旧锁文件。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

use crate::scope::{Scope, sanitize};

#[derive(Debug)]
pub struct Lease {
    file: File,
    path: PathBuf,
}

impl Lease {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        // 显式解锁，语义比"依赖 close"更清楚（close 也会释放）。
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[derive(Debug)]
pub struct LeaseError {
    pub message: String,
}

impl LeaseError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for LeaseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for LeaseError {}

impl From<std::io::Error> for LeaseError {
    fn from(err: std::io::Error) -> Self {
        Self::new(format!("租约文件 IO 错误: {err}"))
    }
}

/// 拿租约。范围粒度：`producer`。
pub fn acquire(lease_dir: &Path, scope: &Scope) -> Result<Lease, LeaseError> {
    std::fs::create_dir_all(lease_dir)?;
    let name = format!("{}.lock", sanitize(&scope.producer()));
    let path = lease_dir.join(name);
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let holder = std::fs::read_to_string(&path).unwrap_or_default();
        let holder = holder.trim();
        return Err(LeaseError::new(format!(
            "租约被占用，拒绝重复采集 {}（持有者: {}）",
            path.display(),
            if holder.is_empty() { "未知" } else { holder }
        )));
    }

    let info = format!(
        "pid={} scope={} acquired_at={}\n",
        std::process::id(),
        scope.producer(),
        crate::time::rfc3339_utc(crate::time::utc_now())
    );
    file.set_len(0)?;
    file.write_all(info.as_bytes())?;
    file.flush()?;

    Ok(Lease { file, path })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Vendor;

    fn scope() -> Scope {
        Scope::new(
            "tencent-cloud-prod",
            "inventory",
            "prod",
            Vendor::TencentCloud,
            vec!["ap-guangzhou".into()],
        )
    }

    #[test]
    fn second_acquire_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let first = acquire(dir.path(), &scope()).expect("第一次应拿到租约");
        let err = acquire(dir.path(), &scope()).expect_err("第二次必须失败");
        assert!(err.message.contains("租约被占用"), "{}", err.message);
        drop(first);
        acquire(dir.path(), &scope()).expect("释放后可再拿");
    }
}
