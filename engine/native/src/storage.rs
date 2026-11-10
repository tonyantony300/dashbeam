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
    open_store(dir)
        .await
        .with_context(|| format!("failed to load send store at {}", dir.display()))
}

/// Whether partial receives are staged inside the destination folder, so the
/// bytes never touch the system temp drive and the export is a rename. Android
/// receives into app-private staging and exports that whole tree afterwards, so
/// a store placed there would be exported along with the files.
pub fn stages_in_destination() -> bool {
    !cfg!(target_os = "android")
}

/// The partial-receive store for `hash_hex` under the temp dir: the only
/// location on Android, the fallback everywhere else.
pub fn temp_recv_store_dir(hash_hex: &str) -> PathBuf {
    temp_dir().join(format!("{RECV_DIR_PREFIX}{hash_hex}"))
}

/// Where a receive of `hash_hex` bound for `output_dir` is staged.
pub fn recv_store_dir(hash_hex: &str, output_dir: &Path) -> PathBuf {
    if stages_in_destination() {
        output_dir.join(format!("{RECV_DIR_PREFIX}{hash_hex}"))
    } else {
        temp_recv_store_dir(hash_hex)
    }
}

pub struct RecvStore {
    pub store: FsStore,
    pub path: PathBuf,
    /// True when the store sits in the destination folder, i.e. on the same
    /// volume as the exported files.
    pub in_destination: bool,
}

/// Opens the store a receive downloads into. Prefers the destination folder and
/// falls back to the temp dir when a store cannot live there (some network
/// shares and removable filesystems cannot host the database).
pub async fn create_recv_store(hash_hex: &str, output_dir: &Path) -> anyhow::Result<RecvStore> {
    let fallback = temp_recv_store_dir(hash_hex);
    let staged = recv_store_dir(hash_hex, output_dir);
    if staged != fallback {
        let existed = staged.exists();
        match open_store(&staged).await {
            Ok(store) => {
                hide_dir(&staged);
                return Ok(RecvStore {
                    store,
                    path: staged,
                    in_destination: true,
                });
            }
            Err(e) => {
                tracing::warn!(
                    "cannot stage receive in {}, using the temp dir: {e:#}",
                    staged.display()
                );
                if !existed {
                    let _ = tokio::fs::remove_dir_all(&staged).await;
                }
            }
        }
    }
    let store = open_store(&fallback)
        .await
        .with_context(|| format!("failed to load recv store at {}", fallback.display()))?;
    Ok(RecvStore {
        store,
        path: fallback,
        in_destination: false,
    })
}

/// How long a store may take to open before it is treated as failed.
const STORE_OPEN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Opens a blob store in `dir`, never hanging.
///
/// What the store needs (a writable directory and file locking) is probed with
/// plain file operations first, so an unusable folder fails fast with a clear
/// reason. The open itself is bounded as a backstop: iroh-blobs before 0.104
/// deadlocked instead of returning an error when its setup failed.
async fn open_store(dir: &Path) -> anyhow::Result<FsStore> {
    let probe_dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || probe_store_dir(&probe_dir))
        .await
        .context("store probe panicked")??;
    match tokio::time::timeout(STORE_OPEN_TIMEOUT, FsStore::load(dir)).await {
        Ok(store) => Ok(store?),
        Err(_) => anyhow::bail!("timed out opening the blob store"),
    }
}

fn probe_store_dir(dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir).context("cannot create the store dir")?;
    let probe = dir.join(".probe");
    let result = (|| {
        let file = std::fs::File::create(&probe).context("store dir is not writable")?;
        file.try_lock()
            .map_err(|e| anyhow::anyhow!("file locking unavailable: {e}"))?;
        file.unlock().context("file locking unavailable")?;
        anyhow::Ok(())
    })();
    let _ = std::fs::remove_file(&probe);
    result
}

/// Dot-prefixed names are not hidden on Windows; set the attribute so the
/// staging folder stays out of the user's way in Explorer.
#[cfg(windows)]
fn hide_dir(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: `wide` is a NUL-terminated UTF-16 path that outlives the call.
    if unsafe { SetFileAttributesW(wide.as_ptr(), FILE_ATTRIBUTE_HIDDEN) } == 0 {
        tracing::debug!("could not hide {}", path.display());
    }
}

#[cfg(not(windows))]
fn hide_dir(_path: &Path) {}

pub fn recv_cleanup_guard(path: PathBuf) -> AutoCleanupDir {
    AutoCleanupDir::new(path)
}

pub fn temp_dir() -> PathBuf {
    TEMP_DIR.get().cloned().unwrap_or_else(std::env::temp_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression: this used to hang forever inside iroh-blobs.
    #[tokio::test(flavor = "multi_thread")]
    async fn opening_a_store_where_a_file_sits_fails_instead_of_hanging() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let blocked = dir.path().join("store");
        std::fs::write(&blocked, b"not a directory").expect("write");

        let opened = tokio::time::timeout(std::time::Duration::from_secs(10), open_store(&blocked))
            .await
            .expect("open_store must return, not hang");

        assert!(opened.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_opens_in_a_usable_directory() {
        let dir = tempfile::TempDir::new().expect("tempdir");
        let store_dir = dir.path().join("store");

        let store = open_store(&store_dir).await.expect("open");

        assert!(!store_dir.join(".probe").exists(), "the probe cleans up");
        store.shutdown().await.expect("shutdown");
    }
}
