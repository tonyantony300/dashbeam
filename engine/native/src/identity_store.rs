//! Persistent Iroh node identity (`SecretKey`) for desktop and Android.
//!
//! Storage priority:
//! 1. `IROH_SECRET` env var (explicit override; not written to disk)
//! 2. `identity.key` in the app data directory (32 raw bytes; source of truth)
//! 3. OS keychain entry (legacy desktop; migrated into `identity.key` on first load;
//!    skipped on Android where keyring has no real credential store)
//!
//! On conflict between file and keychain, the file wins and keychain is re-synced.

use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use data_encoding::HEXLOWER;
use iroh::SecretKey;

use crate::secret_store;

pub const IDENTITY_KEY_FILE: &str = "identity.key";
const SECRET_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSource {
    EnvVar,
    IdentityFile,
    KeychainMigrated,
    Generated,
}

#[derive(Debug)]
pub struct SecretLoadOutcome {
    pub secret: SecretKey,
    pub source: SecretSource,
}

pub fn identity_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join(IDENTITY_KEY_FILE)
}

pub fn load_or_create_secret(data_dir: &Path) -> Result<SecretLoadOutcome> {
    load_or_create_secret_with(data_dir, true, true)
}

/// `use_keychain == false` never touches the OS keychain; `honor_env == false`
/// ignores `IROH_SECRET`. Both off gives a purely file-backed identity.
pub fn load_or_create_secret_with(
    data_dir: &Path,
    use_keychain: bool,
    honor_env: bool,
) -> Result<SecretLoadOutcome> {
    std::fs::create_dir_all(data_dir).with_context(|| format!("create {}", data_dir.display()))?;

    if honor_env {
        if let Ok(hex) = std::env::var("IROH_SECRET") {
            let secret = parse_secret_hex(&hex)?;

            return Ok(SecretLoadOutcome {
                secret,
                source: SecretSource::EnvVar,
            });
        }
    }

    let file_path = identity_key_path(data_dir);
    let file_secret = read_identity_file(&file_path)?;
    let keychain_secret = if use_keychain {
        read_keychain_secret()?
    } else {
        None
    };

    match (file_secret, keychain_secret) {
        (Some(file), Some(keychain)) if secrets_equal(&file, &keychain) => {

            Ok(SecretLoadOutcome {
                secret: file,
                source: SecretSource::IdentityFile,
            })
        }
        (Some(file), Some(_keychain)) => {

            sync_keychain(use_keychain, &file);
            Ok(SecretLoadOutcome {
                secret: file,
                source: SecretSource::IdentityFile,
            })
        }
        (Some(file), None) => {
            sync_keychain(use_keychain, &file);

            Ok(SecretLoadOutcome {
                secret: file,
                source: SecretSource::IdentityFile,
            })
        }
        (None, Some(keychain)) => {
            write_identity_file(&file_path, &keychain)?;
            sync_keychain(use_keychain, &keychain);

            Ok(SecretLoadOutcome {
                secret: keychain,
                source: SecretSource::KeychainMigrated,
            })
        }
        (None, None) => {
            let secret = SecretKey::generate();
            write_identity_file(&file_path, &secret)?;
            sync_keychain(use_keychain, &secret);

            Ok(SecretLoadOutcome {
                secret,
                source: SecretSource::Generated,
            })
        }
    }
}

fn secrets_equal(a: &SecretKey, b: &SecretKey) -> bool {
    a.to_bytes() == b.to_bytes()
}

fn read_identity_file(path: &Path) -> Result<Option<SecretKey>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    match parse_secret_bytes(&bytes) {
        Ok(secret) => Ok(Some(secret)),
        Err(_err) => {

            Ok(None)
        }
    }
}

fn write_identity_file(path: &Path, secret: &SecretKey) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("key.tmp");
    std::fs::write(&tmp, secret.to_bytes()).with_context(|| format!("write {}", tmp.display()))?;
    restrict_permissions(&tmp)?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    restrict_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> io::Result<()> {
    Ok(())
}

fn read_keychain_secret() -> Result<Option<SecretKey>> {
    let Some(hex) = secret_store::load_secret_hex()? else {
        return Ok(None);
    };
    match parse_secret_hex(&hex) {
        Ok(secret) => Ok(Some(secret)),
        Err(_err) => {

            Ok(None)
        }
    }
}

fn sync_keychain(enabled: bool, secret: &SecretKey) {
    if enabled {
        sync_keychain_best_effort(secret);
    }
}

fn sync_keychain_best_effort(secret: &SecretKey) {
    let hex = HEXLOWER.encode(&secret.to_bytes());
    match secret_store::save_secret_hex(&hex) {
        Ok(()) => {
            match secret_store::load_secret_hex() {
                Ok(Some(stored)) if stored == hex => {}
                Ok(Some(_)) => {},
                Ok(None) => {},
                Err(_err) => {}
            }
        }
        Err(_err) => {}
    }
}

fn parse_secret_bytes(bytes: &[u8]) -> Result<SecretKey> {
    anyhow::ensure!(
        bytes.len() == SECRET_LEN,
        "identity key must be {} bytes, got {}",
        SECRET_LEN,
        bytes.len()
    );
    let mut arr = [0u8; SECRET_LEN];
    arr.copy_from_slice(bytes);
    Ok(SecretKey::from_bytes(&arr))
}

fn parse_secret_hex(hex: &str) -> Result<SecretKey> {
    let bytes = HEXLOWER
        .decode(hex.trim().as_bytes())
        .context("invalid secret hex")?;
    parse_secret_bytes(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};
    use tempfile::TempDir;

    static KEYCHAIN_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// Restores an env var when dropped.
    struct EnvGuard(&'static str, Option<String>);

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.1 {
                Some(v) => std::env::set_var(self.0, v),
                None => std::env::remove_var(self.0),
            }
        }
    }

    fn set_env_for_test(key: &'static str, val: &str) -> EnvGuard {
        let guard = EnvGuard(key, std::env::var(key).ok());
        std::env::set_var(key, val);
        guard
    }

    fn lock_keychain_tests() -> MutexGuard<'static, ()> {
        KEYCHAIN_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn identity_file_roundtrip() {
        let dir = TempDir::new().expect("tempdir");
        let secret = SecretKey::generate();
        let path = identity_key_path(dir.path());
        write_identity_file(&path, &secret).expect("write");
        let loaded = read_identity_file(&path)
            .expect("read")
            .expect("some");
        assert!(secrets_equal(&secret, &loaded));
    }

    #[test]
    fn generated_secret_is_stable_across_restarts() {
        let _guard = lock_keychain_tests();
        let dir = TempDir::new().expect("tempdir");
        let first = load_or_create_secret(dir.path()).expect("first load");
        assert_eq!(first.source, SecretSource::Generated);
        let second = load_or_create_secret(dir.path()).expect("second load");
        assert_eq!(second.source, SecretSource::IdentityFile);
        assert!(secrets_equal(&first.secret, &second.secret));
    }

    #[test]
    fn file_wins_over_different_keychain() {
        let _guard = lock_keychain_tests();
        let _ = secret_store::delete_secret();
        let dir = TempDir::new().expect("tempdir");
        let file_secret = SecretKey::generate();
        write_identity_file(&identity_key_path(dir.path()), &file_secret).expect("write file");

        let keychain_secret = SecretKey::generate();
        let hex = HEXLOWER.encode(&keychain_secret.to_bytes());
        secret_store::save_secret_hex(&hex).expect("write keychain");

        let loaded = load_or_create_secret(dir.path()).expect("load");
        assert_eq!(loaded.source, SecretSource::IdentityFile);
        assert!(secrets_equal(&file_secret, &loaded.secret));

        let _ = secret_store::delete_secret();
    }

    #[test]
    fn keychain_migrates_when_file_missing() {
        let _guard = lock_keychain_tests();
        let _ = secret_store::delete_secret();
        let keychain_secret = SecretKey::generate();
        let hex = HEXLOWER.encode(&keychain_secret.to_bytes());
        secret_store::save_secret_hex(&hex).expect("write keychain");
        let roundtrip = secret_store::load_secret_hex().expect("read keychain");
        if roundtrip.as_deref() != Some(hex.as_str()) {
            eprintln!("skipping keychain_migrates_when_file_missing: OS keychain roundtrip unavailable");
            let _ = secret_store::delete_secret();
            return;
        }

        let dir = TempDir::new().expect("tempdir");
        let loaded = load_or_create_secret(dir.path()).expect("load");
        assert_eq!(loaded.source, SecretSource::KeychainMigrated);
        assert!(secrets_equal(&keychain_secret, &loaded.secret));
        assert!(identity_key_path(dir.path()).exists());

        let _ = secret_store::delete_secret();
    }

    #[test]
    fn keychain_free_load_never_touches_keychain_and_persists() {
        let dir = TempDir::new().expect("tempdir");
        secret_store::reset_call_count_for_tests();
        let a = load_or_create_secret_with(dir.path(), false, false).unwrap();
        let b = load_or_create_secret_with(dir.path(), false, false).unwrap();
        assert_eq!(a.secret.to_bytes(), b.secret.to_bytes());
        assert_eq!(a.source, SecretSource::Generated);
        assert_eq!(b.source, SecretSource::IdentityFile);
        assert_eq!(secret_store::call_count_for_tests(), 0);
    }

    #[test]
    fn keychain_free_load_ignores_iroh_secret_env() {
        let _guard = lock_keychain_tests();
        let dir = TempDir::new().expect("tempdir");
        let env_key = SecretKey::generate();
        let _env = set_env_for_test("IROH_SECRET", &HEXLOWER.encode(&env_key.to_bytes()));
        let out = load_or_create_secret_with(dir.path(), false, false).unwrap();
        assert_ne!(out.secret.to_bytes(), env_key.to_bytes());
        assert_ne!(out.source, SecretSource::EnvVar);
    }
}
