use anyhow::{anyhow, Context, Result};
use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

/// Exclusive per-data-dir node lock; released on drop or process death.
#[derive(Debug)]
pub struct NodeLock {
    _file: File,
}

impl NodeLock {
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        let path = crate::paths::lock_path(data_dir);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => {
                Err(anyhow!("agent already running for {}", data_dir.display()))
            }
            Err(TryLockError::Error(e)) => {
                Err(e).with_context(|| format!("lock {}", path.display()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_lock_on_same_dir_fails_with_already_running() {
        let d = tempfile::tempdir().unwrap();
        let _a = NodeLock::acquire(d.path()).unwrap();
        let err = NodeLock::acquire(d.path()).err().unwrap();
        assert!(err.to_string().contains("already running"), "{err}");
    }

    #[test]
    fn lock_released_on_drop() {
        let d = tempfile::tempdir().unwrap();
        drop(NodeLock::acquire(d.path()).unwrap());
        NodeLock::acquire(d.path()).unwrap();
    }
}
