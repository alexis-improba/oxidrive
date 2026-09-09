//! Dispatches changed files to the correct text extractor.

use std::path::{Path, PathBuf};

use crate::error::OxidriveError;
use crate::index::csv_extract;
use crate::index::docx;
use crate::index::pdf;
use crate::index::pptx;
use crate::index::xlsx;
use crate::types::RelativePath;
use crate::utils::fs::atomic_write;

const MAX_INDEX_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TEXT_PASSTHROUGH_BYTES: usize = 8 * 1024 * 1024;

fn index_markdown_path(index_dir: &Path, rel: &RelativePath) -> PathBuf {
    let rel_path = Path::new(rel.as_str());
    let mut dest = match rel_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => index_dir.join(parent),
        _ => index_dir.to_path_buf(),
    };
    let leaf = rel_path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| rel.as_str().to_string());
    dest.push(format!("{leaf}.md"));
    dest
}

/// Rebuilds Markdown sidecars under `index_dir` for every supported file in `changed_files`.
///
/// Output paths mirror the relative layout under `sync_dir`, appending `.md` to the full leaf name.
pub async fn update_index(
    changed_files: &[RelativePath],
    sync_dir: &Path,
    index_dir: &Path,
) -> Result<usize, OxidriveError> {
    tokio::fs::create_dir_all(index_dir)
        .await
        .map_err(|e| OxidriveError::other(format!("create index dir: {e}")))?;

    let mut count = 0usize;
    for rel in changed_files {
        if !rel.is_safe_non_empty() {
            tracing::warn!(path = %rel, "skipping unsafe path for the search index");
            continue;
        }
        if rel.as_str().starts_with(".oxidrive/") || rel.as_str().starts_with(".index/") {
            tracing::debug!(path = %rel, "skipping internal path for index update");
            continue;
        }
        let src = sync_dir.join(rel.as_str());
        let dest = index_markdown_path(index_dir, rel);

        if !src.exists() {
            if tokio::fs::metadata(&dest).await.is_ok() {
                tokio::fs::remove_file(&dest).await.map_err(|e| {
                    OxidriveError::other(format!("remove index {}: {e}", dest.display()))
                })?;
                count += 1;
            }
            continue;
        }
        let src_meta = std::fs::metadata(&src)
            .map_err(|e| OxidriveError::other(format!("Failed to stat {}: {e}", src.display())))?;
        if src_meta.len() > MAX_INDEX_SOURCE_BYTES {
            let markdown = format!(
                "# {}\n\n- **Indexation ignorée** : fichier trop volumineux\n- **Taille** : {} octets\n- **Limite** : {} octets\n",
                src.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default(),
                src_meta.len(),
                MAX_INDEX_SOURCE_BYTES
            );
            if let Some(parent) = dest.parent() {
                tokio::fs::create_dir_all(parent).await.map_err(|e| {
                    OxidriveError::other(format!("mkdir {}: {e}", parent.display()))
                })?;
            }
            atomic_write(&dest, markdown.as_bytes()).await?;
            count += 1;
            continue;
        }

        let ext = src
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        let markdown = match ext.as_str() {
            "docx" => docx::docx_to_markdown(&src)?,
            "xlsx" => xlsx::xlsx_to_markdown(&src)?,
            "pptx" => pptx::pptx_to_markdown(&src)?,
            "pdf" => pdf::pdf_to_markdown(&src)?,
            "csv" => csv_extract::csv_to_markdown(&src)?,
            "txt" | "md" | "markdown" | "rst" | "text" => {
                let bytes = std::fs::read(&src).map_err(|e| {
                    OxidriveError::other(format!("Failed to read {}: {e}", src.display()))
                })?;
                if bytes.len() > MAX_TEXT_PASSTHROUGH_BYTES {
                    format!(
                        "# {}\n\n- **Indexation texte tronquée** : fichier trop volumineux\n- **Taille** : {} octets\n- **Limite lecture texte** : {} octets\n",
                        src.file_name()
                            .map(|n| n.to_string_lossy().to_string())
                            .unwrap_or_default(),
                        bytes.len(),
                        MAX_TEXT_PASSTHROUGH_BYTES
                    )
                } else {
                    String::from_utf8_lossy(&bytes).to_string()
                }
            }
            _ => {
                let name = src
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                format!(
                    "# {name}\n\n- **Extension** : .{ext}\n- **Taille** : {} octets\n- **Modifié** : {:?}\n",
                    src_meta.len(),
                    src_meta.modified().ok(),
                )
            }
        };

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| OxidriveError::other(format!("mkdir {}: {e}", parent.display())))?;
        }
        atomic_write(&dest, markdown.as_bytes()).await?;
        count += 1;
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn text_passthrough_writes_content() {
        let sync = tempdir().expect("tempdir");
        let index = tempdir().expect("tempdir");
        std::fs::write(sync.path().join("note.txt"), "hello index").expect("write");
        let changed = [RelativePath::from("note.txt")];
        let n = update_index(&changed, sync.path(), index.path())
            .await
            .expect("update");
        assert_eq!(n, 1);
        let md = std::fs::read_to_string(index.path().join("note.txt.md")).expect("read md");
        assert_eq!(md, "hello index");
    }

    #[tokio::test]
    async fn binary_unknown_writes_metadata() {
        let sync = tempdir().expect("tempdir");
        let index = tempdir().expect("tempdir");
        std::fs::write(sync.path().join("pic.png"), [0u8, 1, 2]).expect("write");
        let changed = [RelativePath::from("pic.png")];
        let n = update_index(&changed, sync.path(), index.path())
            .await
            .expect("update");
        assert_eq!(n, 1);
        let md = std::fs::read_to_string(index.path().join("pic.png.md")).expect("read md");
        assert!(md.contains("# pic.png"));
        assert!(md.contains("**Extension**"));
        assert!(md.contains(".png"));
        assert!(md.contains("3 octets"));
    }

    #[tokio::test]
    async fn missing_source_removes_index_sidecar() {
        let sync = tempdir().expect("tempdir");
        let index = tempdir().expect("tempdir");
        let sidecar = index.path().join("gone.txt.md");
        std::fs::write(&sidecar, "stale").expect("write sidecar");
        let changed = [RelativePath::from("gone.txt")];
        let n = update_index(&changed, sync.path(), index.path())
            .await
            .expect("update");
        assert_eq!(n, 1);
        assert!(!sidecar.exists());
    }

    #[tokio::test]
    async fn internal_oxidrive_paths_are_ignored() {
        let sync = tempdir().expect("tempdir");
        let index = tempdir().expect("tempdir");
        std::fs::create_dir_all(sync.path().join(".oxidrive")).expect("mkdir");
        std::fs::write(sync.path().join(".oxidrive/token.json"), "secret").expect("write");

        let changed = [RelativePath::from(".oxidrive/token.json")];
        let n = update_index(&changed, sync.path(), index.path())
            .await
            .expect("update");
        assert_eq!(n, 0);
        assert!(!index.path().join(".oxidrive/token.json.md").exists());
    }

    #[tokio::test]
    async fn sidecars_do_not_collide_for_different_source_extensions() {
        let sync = tempdir().expect("tempdir");
        let index = tempdir().expect("tempdir");
        std::fs::write(sync.path().join("report.txt"), "plain").expect("write txt");
        std::fs::write(sync.path().join("report.md"), "# title").expect("write md");

        let changed = [
            RelativePath::from("report.txt"),
            RelativePath::from("report.md"),
        ];
        let n = update_index(&changed, sync.path(), index.path())
            .await
            .expect("update");
        assert_eq!(n, 2);
        assert!(index.path().join("report.txt.md").exists());
        assert!(index.path().join("report.md.md").exists());

        std::fs::remove_file(sync.path().join("report.txt")).expect("remove txt");
        let changed = [RelativePath::from("report.txt")];
        let n = update_index(&changed, sync.path(), index.path())
            .await
            .expect("update cleanup");
        assert_eq!(n, 1);
        assert!(!index.path().join("report.txt.md").exists());
        assert!(index.path().join("report.md.md").exists());
    }
}
