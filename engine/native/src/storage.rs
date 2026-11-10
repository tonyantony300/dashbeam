//! Native filesystem blob store creation.

use crate::types::AutoCleanupDir;
use anyhow::Context;
use data_encoding::HEXLOWER;
use iroh_blobs::store::fs::FsStore;
use rand::RngExt;
use std::path::{Path, PathBuf};

/// Root for every blob store this crate creates. Set once by the shell
/// (Android points it at the app cache dir, where `std::env::temp_dir()` is
/// not writable before Android 13). Left unset, [`temp_dir`] falls back to
/// `std::env::temp_dir()`, so tests and non-Tauri consumers keep working.
pub static TEMP_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Directory name prefix of every send-side blob store.
pub const SEND_DIR_PREFIX: &str = ".dashbeam-send-";
/// Directory name prefix of every partial-receive blob store.
pub const RECV_DIR_PREFIX: &str = ".dashbeam-recv-";
/// Prefixes older builds used. Never created any more; the shell still sweeps
/// them at launch so an upgrade leaves nothing behind in the temp dir.
pub const LEGACY_DIR_PREFIXES: [&str; 2] = [".sendme-send-", ".sendme-recv-"];

pub fn new_send_blobs_dir() -> PathBuf {
    let suffix = rand::rng().random::<[u8; 16]>();
    temp_dir().join(format!("{SEND_DIR_PREFIX}{}", HEXLOWER.encode(&suffix)))
}

pub async fn create_send_store(dir: &Path) -> anyhow::Result<FsStore> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("failed to create send store dir {}", dir.display()))?;
    FsStore::load(dir)
        .await
        .with_context(|| format!("failed to load send store at {}", dir.display()))
}

/// How long opening a partial-receive store may take before it's set aside.
const RECV_STORE_LOAD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

pub async fn create_recv_store(hash_hex: &str) -> anyhow::Result<(FsStore, PathBuf)> {
    let dir_name = format!("{RECV_DIR_PREFIX}{hash_hex}");
    let path = temp_dir().join(dir_name);
    match tokio::time::timeout(RECV_STORE_LOAD_TIMEOUT, FsStore::load(&path)).await {
        Ok(store) => {
            let store = store
                .with_context(|| format!("failed to load recv store at {}", path.display()))?;
            Ok((store, path))
        }
        // A store left by a process that died mid-download can hang `load`
        // forever, wedging every later download of the same content. Move it
        // aside (the prefix keeps it in the launch sweep) and start fresh.
        Err(_) => {
            let suffix = HEXLOWER.encode(&rand::rng().random::<[u8; 4]>());
            let aside = temp_dir().join(format!("{RECV_DIR_PREFIX}{hash_hex}-unreadable-{suffix}"));
            std::fs::rename(&path, &aside).with_context(|| {
                format!(
                    "partial download store {} did not open; remove it and retry",
                    path.display()
                )
            })?;
            tracing::warn!(
                store = %path.display(),
                moved_to = %aside.display(),
                "partial download store did not open in time; starting fresh"
            );
            let store = tokio::time::timeout(RECV_STORE_LOAD_TIMEOUT, FsStore::load(&path))
                .await
                .context("recv store load timed out")?
                .with_context(|| format!("failed to load recv store at {}", path.display()))?;
            Ok((store, path))
        }
    }
}

pub fn recv_cleanup_guard(path: PathBuf) -> AutoCleanupDir {
    AutoCleanupDir::new(path)
}

pub fn temp_dir() -> PathBuf {
    TEMP_DIR.get().cloned().unwrap_or_else(std::env::temp_dir)
}
