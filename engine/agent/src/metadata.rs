//! Validates the paths an agent asks to send and describes them for the invite.

use anyhow::{bail, Context, Result};
use engine::FileMetadata;
use std::path::{Path, PathBuf};

/// Paths checked and measured before anything is shared.
#[derive(Debug, Clone)]
pub struct Prepared {
    /// Absolute, in the caller's order.
    pub paths: Vec<PathBuf>,
    /// Top-level names, one per path.
    pub file_names: Vec<String>,
    /// Regular files, counting those inside directories.
    pub file_count: u32,
    pub total_size: u64,
    pub metadata: FileMetadata,
}

/// Relative paths join `cwd` (the caller's, not the daemon's). Every path must
/// exist; directories are walked for the file count and size.
pub fn prepare_paths(paths: &[PathBuf], cwd: &Path) -> Result<Prepared> {
    if paths.is_empty() {
        bail!("no paths to send");
    }
    let mut out = Vec::with_capacity(paths.len());
    let mut file_names = Vec::with_capacity(paths.len());
    let (mut file_count, mut total_size) = (0u64, 0u64);
    for p in paths {
        let abs = std::path::absolute(cwd.join(p))
            .with_context(|| format!("resolve path {}", p.display()))?;
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                bail!("path not found: {}", abs.display())
            }
            Err(e) => return Err(e).with_context(|| format!("read {}", abs.display())),
        };
        if meta.is_dir() {
            let (n, bytes) = walk(&abs)?;
            file_count += n;
            total_size += bytes;
        } else {
            file_count += 1;
            total_size += meta.len();
        }
        file_names.push(
            abs.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| abs.display().to_string()),
        );
        out.push(abs);
    }
    let file_name = match file_names.as_slice() {
        [one] => one.clone(),
        many => format!("{} items", many.len()),
    };
    let metadata = FileMetadata {
        file_name,
        item_count: u32::try_from(out.len()).unwrap_or(u32::MAX),
        size: total_size,
        thumbnail: None,
        mime_type: None,
        items: None,
    };
    Ok(Prepared {
        paths: out,
        file_names,
        file_count: u32::try_from(file_count).unwrap_or(u32::MAX),
        total_size,
        metadata,
    })
}

/// `(regular files, bytes)` under `dir`. Symlinked dirs are not followed (no cycles).
fn walk(dir: &Path) -> Result<(u64, u64)> {
    let (mut count, mut bytes) = (0, 0);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).with_context(|| format!("read {}", d.display()))? {
            let entry = entry.with_context(|| format!("read {}", d.display()))?;
            let ty = entry.file_type()?;
            if ty.is_dir() {
                stack.push(entry.path());
            } else if ty.is_file() || ty.is_symlink() {
                match std::fs::metadata(entry.path()) {
                    Ok(m) if m.is_file() => {
                        count += 1;
                        bytes += m.len();
                    }
                    _ => {}
                }
            }
        }
    }
    Ok((count, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_path_errors_with_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.txt");
        let err = prepare_paths(std::slice::from_ref(&missing), dir.path()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("path not found"), "{msg}");
        assert!(msg.contains(&missing.display().to_string()), "{msg}");
    }

    #[test]
    fn relative_path_resolves_against_cwd() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hello").unwrap();
        let p = prepare_paths(&[PathBuf::from("a.txt")], dir.path()).unwrap();
        assert_eq!(p.paths, vec![dir.path().join("a.txt")]);
        assert!(p.paths[0].is_absolute());
        assert_eq!(p.file_names, vec!["a.txt".to_string()]);
        assert_eq!((p.file_count, p.total_size), (1, 5));
        assert_eq!(p.metadata.file_name, "a.txt");
        assert_eq!((p.metadata.item_count, p.metadata.size), (1, 5));

        // Relative to the caller's cwd, not the process's.
        let other = tempfile::tempdir().unwrap();
        let err = prepare_paths(&[PathBuf::from("a.txt")], other.path()).unwrap_err();
        assert!(format!("{err:#}").contains(&other.path().join("a.txt").display().to_string()));
    }

    #[test]
    fn directory_counts_files_and_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("docs");
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::write(root.join("one.txt"), b"1234").unwrap();
        std::fs::write(root.join("sub/two.txt"), b"123456").unwrap();
        std::fs::write(root.join("sub/deeper/three.bin"), [0u8; 10]).unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();
        let extra = dir.path().join("extra.txt");
        std::fs::write(&extra, b"xy").unwrap();

        let p = prepare_paths(std::slice::from_ref(&root), dir.path()).unwrap();
        assert_eq!((p.file_count, p.total_size), (3, 20));
        assert_eq!(p.file_names, vec!["docs".to_string()]);
        assert_eq!(p.metadata.file_name, "docs");
        assert_eq!((p.metadata.item_count, p.metadata.size), (1, 20));

        let p = prepare_paths(&[root, extra], dir.path()).unwrap();
        assert_eq!((p.file_count, p.total_size), (4, 22));
        assert_eq!(p.file_names, vec!["docs".to_string(), "extra.txt".into()]);
        assert_eq!(p.metadata.file_name, "2 items");
        assert_eq!((p.metadata.item_count, p.metadata.size), (2, 22));
        assert!(p.metadata.thumbnail.is_none() && p.metadata.items.is_none());
    }

    #[test]
    fn empty_path_list_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prepare_paths(&[], dir.path()).is_err());
    }
}
