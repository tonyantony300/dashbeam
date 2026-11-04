use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

const APP_DIR_NAME: &str = "com.dashbeam.desktop";

/// Resolve and create the agent data dir: flag > `DASHBEAM_AGENT_DIR` > default.
/// Refuses the desktop app's own dir so the two never share identity.
pub fn resolve_data_dir(flag: Option<PathBuf>) -> Result<PathBuf> {
    let base = dirs::data_dir();
    let dir = match flag {
        Some(p) => p,
        None => match std::env::var_os("DASHBEAM_AGENT_DIR").filter(|v| !v.is_empty()) {
            Some(v) => PathBuf::from(v),
            None => base
                .clone()
                .context("no OS data dir; pass --data-dir")?
                .join("dashbeam-agent"),
        },
    };
    if let Some(base) = &base {
        if dir == base.join(APP_DIR_NAME) {
            bail!(
                "refusing to use the DashBeam app data dir: {}",
                dir.display()
            );
        }
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod {}", dir.display()))?;
    }
    Ok(dir)
}

pub fn socket_path(data_dir: &Path) -> PathBuf {
    data_dir.join("node.sock")
}

pub fn lock_path(data_dir: &Path) -> PathBuf {
    data_dir.join("node.lock")
}

pub fn log_path(data_dir: &Path) -> PathBuf {
    data_dir.join("node.log")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn resolve_data_dir_prefers_flag_over_env() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("DASHBEAM_AGENT_DIR");
        let env_dir = tempfile::tempdir().unwrap();
        let flag_dir = tempfile::tempdir().unwrap();
        std::env::set_var("DASHBEAM_AGENT_DIR", env_dir.path());
        let got = resolve_data_dir(Some(flag_dir.path().join("a"))).unwrap();
        let got_env = resolve_data_dir(None).unwrap();
        match prev {
            Some(v) => std::env::set_var("DASHBEAM_AGENT_DIR", v),
            None => std::env::remove_var("DASHBEAM_AGENT_DIR"),
        }
        assert_eq!(got, flag_dir.path().join("a"));
        assert!(got.is_dir());
        assert_eq!(got_env, env_dir.path());
    }

    #[test]
    fn resolve_data_dir_rejects_app_dir() {
        let app: PathBuf = dirs::data_dir().unwrap().join("com.dashbeam.desktop");
        assert!(resolve_data_dir(Some(app)).is_err());
    }

    #[test]
    fn helper_paths() {
        let d = PathBuf::from("/x");
        assert_eq!(socket_path(&d), PathBuf::from("/x/node.sock"));
        assert_eq!(lock_path(&d), PathBuf::from("/x/node.lock"));
        assert_eq!(log_path(&d), PathBuf::from("/x/node.log"));
    }
}
