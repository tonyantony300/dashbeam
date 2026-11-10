//! Native filesystem export: iroh-blobs collection → directory on disk.

use iroh_blobs::{
    api::{
        blobs::{ExportMode, ExportOptions, ExportProgressItem},
        Store,
    },
    format::collection::Collection,
};
use n0_future::StreamExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportConflict {
    pub original: String,
    pub resolved: String,
}

/// Export a collection into `output_dir`, resolving filename conflicts when needed.
///
/// `ExportMode::TryReference` moves each blob out of the store instead of
/// copying it. Only ask for it when the store is on the destination's volume:
/// it is then a rename, so the payload never exists twice.
pub async fn export_to_directory(
    db: &Store,
    collection: Collection,
    output_dir: &Path,
    mode: ExportMode,
) -> anyhow::Result<Vec<ExportConflict>> {
    let mut conflicts = Vec::new();

    for (name, hash) in collection.iter() {
        let desired_target = get_export_path(output_dir, name)?;
        let target = if desired_target.exists() {
            let resolved = resolve_conflict_path(&desired_target)?;
            conflicts.push(ExportConflict {
                original: desired_target.to_string_lossy().to_string(),
                resolved: resolved.to_string_lossy().to_string(),
            });
            resolved
        } else {
            desired_target
        };

        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(|e| {
                anyhow::anyhow!("failed creating export parent {}: {}", parent.display(), e)
            })?;
        }

        if mode == ExportMode::TryReference {
            match export_blob(db, *hash, &target, mode).await {
                Ok(()) => continue,
                // A failed move leaves the blob in the store (e.g. the target
                // sits behind a mount point), so a plain copy can still succeed.
                Err(cause) => {
                    tracing::warn!("could not move {name} into place, copying: {cause}")
                }
            }
        }
        export_blob(db, *hash, &target, ExportMode::Copy)
            .await
            .map_err(|cause| anyhow::anyhow!("error exporting {}: {}", name, cause))?;
    }

    Ok(conflicts)
}

async fn export_blob(
    db: &Store,
    hash: iroh_blobs::Hash,
    target: &Path,
    mode: ExportMode,
) -> Result<(), String> {
    let mut stream = db
        .export_with_opts(ExportOptions {
            hash,
            target: target.to_path_buf(),
            mode,
        })
        .stream()
        .await;

    while let Some(item) = stream.next().await {
        if let ExportProgressItem::Error(cause) = item {
            return Err(cause.to_string());
        }
    }
    Ok(())
}

fn resolve_conflict_path(path: &Path) -> anyhow::Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("path has no parent: {}", path.display()))?;

    let file_name = path
        .file_name()
        .and_then(|x| x.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid filename: {}", path.display()))?;

    let stem = path
        .file_stem()
        .and_then(|x| x.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid file stem: {}", path.display()))?;

    let extension = path.extension().and_then(|x| x.to_str());

    for index in 1..10_000u32 {
        let candidate_name = if let Some(ext) = extension {
            format!("{} ({}).{}", stem, index, ext)
        } else {
            format!("{} ({})", file_name, index)
        };
        let candidate = parent.join(candidate_name);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }

    anyhow::bail!("too many filename conflicts for {}", path.display())
}

fn get_export_path(root: &Path, name: &str) -> anyhow::Result<PathBuf> {
    let parts = name.split('/');
    let mut path = root.to_path_buf();
    for part in parts {
        validate_path_component(part)?;
        path.push(part);
    }
    Ok(path)
}

fn validate_path_component(component: &str) -> anyhow::Result<()> {
    anyhow::ensure!(!component.is_empty(), "empty path component");
    anyhow::ensure!(!component.contains('/'), "contains /");
    anyhow::ensure!(!component.contains('\\'), "contains \\");
    anyhow::ensure!(!component.contains(':'), "contains colon");
    anyhow::ensure!(component != "..", "parent directory traversal");
    anyhow::ensure!(component != ".", "current directory reference");
    anyhow::ensure!(!component.contains('\0'), "contains null byte");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_empty() {
        assert!(validate_path_component("").is_err());
    }

    #[test]
    fn validate_rejects_slash() {
        assert!(validate_path_component("a/b").is_err());
    }

    #[test]
    fn validate_rejects_backslash() {
        assert!(validate_path_component("a\\b").is_err());
    }

    #[test]
    fn validate_rejects_parent_traversal() {
        assert!(validate_path_component("..").is_err());
    }

    #[test]
    fn validate_rejects_dot() {
        assert!(validate_path_component(".").is_err());
    }

    #[test]
    fn validate_rejects_null_byte() {
        assert!(validate_path_component("a\0b").is_err());
    }

    #[test]
    fn validate_rejects_colon() {
        assert!(validate_path_component("C:foo").is_err());
    }

    #[test]
    fn validate_accepts_normal() {
        assert!(validate_path_component("file.txt").is_ok());
        assert!(validate_path_component("my-file_v2.tar.gz").is_ok());
    }

    #[test]
    fn get_export_path_blocks_drive_prefix() {
        let root = Path::new("/tmp/test");
        assert!(get_export_path(root, "C:foo").is_err());
    }

    #[test]
    fn get_export_path_blocks_traversal() {
        let root = Path::new("/tmp/test");
        assert!(get_export_path(root, "../etc/passwd").is_err());
        assert!(get_export_path(root, "subdir/../../etc/passwd").is_err());
    }

    #[test]
    fn get_export_path_blocks_backslash() {
        assert!(get_export_path(Path::new("/tmp/test"), "file\\name").is_err());
    }

    #[test]
    fn get_export_path_allows_normal() {
        let p = get_export_path(Path::new("/tmp/test"), "subdir/file.txt").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/test/subdir/file.txt"));
    }
}
