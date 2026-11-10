use anyhow::{bail, Context, Result};
use std::path::{Component, Path, PathBuf};

const APP_DIR_NAME: &str = "com.dashbeam.desktop";

/// Absolute form with `.`/`..` collapsed and symlinks resolved for every
/// existing component; not-yet-existing tails are appended as-is.
fn resolve_lenient(p: &Path) -> Result<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => {
                out.push(n);
                // dunce: no `\\?\` verbatim prefix on Windows, so joins and
                // messages use ordinary paths.
                if let Ok(real) = dunce::canonicalize(&out) {
                    out = real;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Ok(out)
}

fn fold_case(c: Component<'_>) -> String {
    let s = c.as_os_str().to_string_lossy();
    if cfg!(any(target_os = "macos", windows)) {
        s.to_lowercase()
    } else {
        s.into_owned()
    }
}

/// Reject `candidate` if it is, or lies inside, `app_dir`. Returns the
/// resolved candidate. Nothing is created on disk.
fn ensure_not_app_dir(candidate: &Path, app_dir: &Path) -> Result<PathBuf> {
    let cand = resolve_lenient(candidate)?;
    let app = resolve_lenient(app_dir)?;
    let c: Vec<_> = cand.components().map(fold_case).collect();
    let a: Vec<_> = app.components().map(fold_case).collect();
    if c.starts_with(&a) {
        bail!(
            "refusing to use the DashBeam app data dir: {}",
            cand.display()
        );
    }
    Ok(cand)
}

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
    let dir = match &base {
        Some(base) => ensure_not_app_dir(&dir, &base.join(APP_DIR_NAME))?,
        None => dir,
    };
    #[cfg_attr(not(unix), allow(unused_variables))]
    let existed = dir.is_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir)?.permissions().mode();
        if !existed {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod {}", dir.display()))?;
        } else if mode & 0o077 != 0 {
            tracing::warn!("data dir {} is group/world accessible", dir.display());
        }
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

/// The node's own blob-store root, so its partial downloads never share the OS
/// temp dir with the desktop app (whose launch sweep would delete them).
pub fn temp_path(data_dir: &Path) -> PathBuf {
    data_dir.join("tmp")
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
        assert_eq!(got, dunce::canonicalize(flag_dir.path()).unwrap().join("a"));
        assert!(got.is_dir());
        assert_eq!(got_env, dunce::canonicalize(env_dir.path()).unwrap());
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

    #[test]
    fn guard_rejects_dotdot_symlink_and_inside() {
        let t = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(t.path()).unwrap();
        let app = root.join("com.dashbeam.desktop");
        std::fs::create_dir(&app).unwrap();
        let other = root.join("other");
        std::fs::create_dir(&other).unwrap();
        assert!(ensure_not_app_dir(&other.join("../com.dashbeam.desktop"), &app).is_err());
        assert!(ensure_not_app_dir(&root.join("./com.dashbeam.desktop"), &app).is_err());
        assert!(ensure_not_app_dir(&app.join("sub/deeper"), &app).is_err());
        #[cfg(unix)]
        {
            let link = root.join("link");
            std::os::unix::fs::symlink(&app, &link).unwrap();
            assert!(ensure_not_app_dir(&link, &app).is_err());
            assert!(ensure_not_app_dir(&link.join("x"), &app).is_err());
        }
        #[cfg(any(target_os = "macos", windows))]
        assert!(ensure_not_app_dir(&root.join("COM.DashBeam.Desktop"), &app).is_err());
        assert!(ensure_not_app_dir(&other, &app).is_ok());
        assert!(ensure_not_app_dir(&root.join("com.dashbeam.desktop2"), &app).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn existing_dir_permissions_untouched_new_dir_is_0700() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let existing = t.path().join("e");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();
        resolve_data_dir(Some(existing.clone())).unwrap();
        let m = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(m(&existing), 0o755);
        let fresh = t.path().join("n");
        resolve_data_dir(Some(fresh.clone())).unwrap();
        assert_eq!(m(&fresh), 0o700);
    }
}
