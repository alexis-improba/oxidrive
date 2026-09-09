//! Pure sync decision logic mapping `(local, remote, metadata)` to a [`SyncAction`](crate::types::SyncAction).

use std::collections::HashSet;

use crate::config::ConflictPolicy;
use crate::drive::types::{remote_content_fingerprint, DriveFile};
use crate::sync::coordination::{Ordering3, VersionVector};
use crate::types::{ConflictResolution, LocalFile, RelativePath, SyncAction, SyncRecord};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LocalDelta {
    Unchanged,
    MetaOnly,
    Content,
}

/// Computes local divergence against persisted metadata.
#[must_use]
fn local_delta(local: &LocalFile, meta: &SyncRecord) -> LocalDelta {
    if local.size == meta.local_size && local.mtime == meta.local_mtime {
        return LocalDelta::Unchanged;
    }
    if local.md5 == meta.local_md5 {
        LocalDelta::MetaOnly
    } else {
        LocalDelta::Content
    }
}

/// Returns `true` when converted local content differs from the last exported bytes.
fn local_changed_converted(local: &LocalFile, last_export_md5: Option<&str>) -> bool {
    match last_export_md5 {
        Some(last) => local.md5 != last,
        None => true,
    }
}

fn local_delta_converted(
    local: &LocalFile,
    meta: &SyncRecord,
    last_export_md5: Option<&str>,
) -> LocalDelta {
    match local_delta(local, meta) {
        LocalDelta::Unchanged => LocalDelta::Unchanged,
        LocalDelta::MetaOnly | LocalDelta::Content => {
            if local_changed_converted(local, last_export_md5) {
                LocalDelta::Content
            } else {
                LocalDelta::MetaOnly
            }
        }
    }
}

/// Returns `true` when remote content or modification time differs from the last reconciled metadata.
fn remote_changed(remote: &DriveFile, meta: &SyncRecord) -> bool {
    let current = remote_content_fingerprint(remote);
    match meta.remote_md5.as_deref() {
        Some(stored) => stored != current.as_str(),
        None => meta
            .remote_modified_at
            .map(|t| t != remote.modified_time)
            .unwrap_or(true),
    }
}

/// Returns `true` when the metadata's Drive object is still present in the remote view.
///
/// Used to avoid a destructive [`SyncAction::DeleteLocal`] when a file is only "missing" at its
/// previous path because Drive exposes a duplicate-named folder/file and the listing remapped the
/// object to a deduplicated path (e.g. `slides/x` -> `slides (2)/x`). The Drive object still
/// exists, so deleting the local copy would lose data.
fn drive_object_still_present(meta: &SyncRecord, remote_file_ids: &HashSet<String>) -> bool {
    match meta.drive_file_id.as_deref() {
        Some(id) => remote_file_ids.contains(id),
        None => false,
    }
}

fn conflict_action(
    path: &RelativePath,
    remote: &DriveFile,
    local: &LocalFile,
    policy: &ConflictPolicy,
) -> SyncAction {
    SyncAction::Conflict {
        path: path.clone(),
        remote_id: Some(remote.id.clone()),
        local_md5: Some(local.md5.clone()),
        resolution: conflict_resolution_from_policy(policy),
    }
}

fn upload_action(path: &RelativePath, remote: &DriveFile, meta: &SyncRecord) -> SyncAction {
    SyncAction::Upload {
        path: path.clone(),
        remote_id: meta
            .drive_file_id
            .clone()
            .or_else(|| Some(remote.id.clone())),
    }
}

/// Resolves `(local content changed, remote changed)` with version-vector causality.
///
/// Safety rule: if vectors are unavailable on both sides, fall back to conflict to avoid data loss.
/// Optimistic upload is only allowed when vectors prove remote did not advance causally past the
/// last synced state (`Equal` or `DominatedBy`).
fn resolve_dual_content_change(
    path: &RelativePath,
    local: &LocalFile,
    remote: &DriveFile,
    meta: &SyncRecord,
    policy: &ConflictPolicy,
) -> SyncAction {
    let remote_vv = VersionVector::from_app_properties(&remote.app_properties);
    let stored_vv = VersionVector::from_map(&meta.version_vector);
    if remote_vv.is_empty() && stored_vv.is_empty() {
        return conflict_action(path, remote, local, policy);
    }
    match remote_vv.dominance(&stored_vv) {
        Ordering3::Equal | Ordering3::DominatedBy => upload_action(path, remote, meta),
        Ordering3::Dominates | Ordering3::Concurrent => {
            conflict_action(path, remote, local, policy)
        }
    }
}

/// Chooses the next action for `path` given the three reconciliation views.
///
/// Backwards-compatible wrapper that assumes no remote-id context is available. Prefer
/// [`determine_action_with_remote_ids`] from the sync engine so duplicate-name remaps do not
/// trigger spurious local deletions.
#[allow(dead_code)]
pub fn determine_action(
    path: &RelativePath,
    local: Option<&LocalFile>,
    remote: Option<&DriveFile>,
    metadata: Option<&SyncRecord>,
    policy: &ConflictPolicy,
) -> SyncAction {
    determine_action_with_remote_ids(path, local, remote, metadata, policy, &HashSet::new())
}

/// Like [`determine_action`] but aware of every Drive file id currently visible in the remote view.
///
/// `remote_file_ids` must contain the ids of all files/folders returned by the current Drive
/// listing. It lets the `(local present, remote absent, metadata present)` case distinguish a real
/// remote deletion from a path remap caused by duplicate names on Drive.
pub fn determine_action_with_remote_ids(
    path: &RelativePath,
    local: Option<&LocalFile>,
    remote: Option<&DriveFile>,
    metadata: Option<&SyncRecord>,
    policy: &ConflictPolicy,
    remote_file_ids: &HashSet<String>,
) -> SyncAction {
    match (local, remote, metadata) {
        (Some(l), Some(r), Some(m)) => {
            let lc = local_delta(l, m);
            let rc = remote_changed(r, m);
            match (lc, rc) {
                (LocalDelta::Unchanged, false) => SyncAction::Skip { path: path.clone() },
                (LocalDelta::Unchanged, true) => SyncAction::Download {
                    path: path.clone(),
                    remote_id: r.id.clone(),
                },
                (LocalDelta::MetaOnly, false) => SyncAction::TouchMetadata { path: path.clone() },
                (LocalDelta::MetaOnly, true) => SyncAction::Download {
                    path: path.clone(),
                    remote_id: r.id.clone(),
                },
                (LocalDelta::Content, false) => SyncAction::Upload {
                    path: path.clone(),
                    remote_id: m.drive_file_id.clone().or_else(|| Some(r.id.clone())),
                },
                (LocalDelta::Content, true) => resolve_dual_content_change(path, l, r, m, policy),
            }
        }
        (Some(l), Some(r), None) => match (&r.md5_checksum, &l.md5) {
            (Some(rm), lm) if rm == lm => SyncAction::Skip { path: path.clone() },
            (None, _) => SyncAction::Conflict {
                path: path.clone(),
                remote_id: Some(r.id.clone()),
                local_md5: Some(l.md5.clone()),
                resolution: conflict_resolution_from_policy(policy),
            },
            _ => SyncAction::Conflict {
                path: path.clone(),
                remote_id: Some(r.id.clone()),
                local_md5: Some(l.md5.clone()),
                resolution: conflict_resolution_from_policy(policy),
            },
        },
        (Some(l), None, Some(m)) => {
            if matches!(local_delta(l, m), LocalDelta::Content) {
                // If the Drive object still exists (remapped under a duplicate-name path),
                // update it in place instead of creating a new file (which would duplicate it).
                let remote_id = if drive_object_still_present(m, remote_file_ids) {
                    m.drive_file_id.clone()
                } else {
                    None
                };
                SyncAction::Upload {
                    path: path.clone(),
                    remote_id,
                }
            } else if drive_object_still_present(m, remote_file_ids) {
                tracing::warn!(
                    path = %path,
                    drive_file_id = m.drive_file_id.as_deref().unwrap_or_default(),
                    "remote name is gone but the file is still on Drive; not deleting locally"
                );
                SyncAction::Skip { path: path.clone() }
            } else {
                SyncAction::DeleteLocal { path: path.clone() }
            }
        }
        (Some(_l), None, None) => SyncAction::Upload {
            path: path.clone(),
            remote_id: None,
        },
        (None, Some(r), Some(m)) => {
            if remote_changed(r, m) {
                SyncAction::Download {
                    path: path.clone(),
                    remote_id: r.id.clone(),
                }
            } else {
                SyncAction::DeleteRemote {
                    path: path.clone(),
                    remote_id: m.drive_file_id.clone().unwrap_or_else(|| r.id.clone()),
                }
            }
        }
        (None, Some(r), None) => SyncAction::Download {
            path: path.clone(),
            remote_id: r.id.clone(),
        },
        (None, None, Some(_)) => SyncAction::CleanupMetadata { path: path.clone() },
        (None, None, None) => SyncAction::Skip { path: path.clone() },
    }
}

/// Like [`determine_action`] but aware of converted Google Workspace files.
///
/// When `is_converted` is true, local edits are compared against `last_export_md5` instead of
/// [`SyncRecord::local_md5`], which avoids false local-change detection when a re-export produced
/// identical bytes.
#[allow(dead_code)]
pub fn determine_action_converted(
    path: &RelativePath,
    local: Option<&LocalFile>,
    remote: Option<&DriveFile>,
    metadata: Option<&SyncRecord>,
    policy: &ConflictPolicy,
    is_converted: bool,
    last_export_md5: Option<&str>,
) -> SyncAction {
    determine_action_converted_with_remote_ids(
        path,
        local,
        remote,
        metadata,
        policy,
        is_converted,
        last_export_md5,
        &HashSet::new(),
    )
}

/// Like [`determine_action_converted`] but aware of every Drive file id in the remote view.
///
/// See [`determine_action_with_remote_ids`] for how `remote_file_ids` prevents duplicate-name
/// remaps from triggering destructive local deletions.
#[allow(clippy::too_many_arguments)]
pub fn determine_action_converted_with_remote_ids(
    path: &RelativePath,
    local: Option<&LocalFile>,
    remote: Option<&DriveFile>,
    metadata: Option<&SyncRecord>,
    policy: &ConflictPolicy,
    is_converted: bool,
    last_export_md5: Option<&str>,
    remote_file_ids: &HashSet<String>,
) -> SyncAction {
    if !is_converted {
        return determine_action_with_remote_ids(
            path,
            local,
            remote,
            metadata,
            policy,
            remote_file_ids,
        );
    }

    match (local, remote, metadata) {
        (Some(l), Some(r), Some(m)) => {
            let lc = local_delta_converted(l, m, last_export_md5);
            let rc = remote_changed(r, m);
            match (lc, rc) {
                (LocalDelta::Unchanged, false) => SyncAction::Skip { path: path.clone() },
                (LocalDelta::Unchanged, true) => SyncAction::Download {
                    path: path.clone(),
                    remote_id: r.id.clone(),
                },
                (LocalDelta::MetaOnly, false) => SyncAction::TouchMetadata { path: path.clone() },
                (LocalDelta::MetaOnly, true) => SyncAction::Download {
                    path: path.clone(),
                    remote_id: r.id.clone(),
                },
                (LocalDelta::Content, false) => SyncAction::Upload {
                    path: path.clone(),
                    remote_id: m.drive_file_id.clone().or_else(|| Some(r.id.clone())),
                },
                (LocalDelta::Content, true) => resolve_dual_content_change(path, l, r, m, policy),
            }
        }
        (Some(l), None, Some(m)) => {
            if matches!(
                local_delta_converted(l, m, last_export_md5),
                LocalDelta::Content
            ) {
                let remote_id = if drive_object_still_present(m, remote_file_ids) {
                    m.drive_file_id.clone()
                } else {
                    None
                };
                SyncAction::Upload {
                    path: path.clone(),
                    remote_id,
                }
            } else if drive_object_still_present(m, remote_file_ids) {
                tracing::warn!(
                    path = %path,
                    drive_file_id = m.drive_file_id.as_deref().unwrap_or_default(),
                    "remote name is gone but the file is still on Drive; not deleting locally"
                );
                SyncAction::Skip { path: path.clone() }
            } else {
                SyncAction::DeleteLocal { path: path.clone() }
            }
        }
        _ => {
            determine_action_with_remote_ids(path, local, remote, metadata, policy, remote_file_ids)
        }
    }
}

fn conflict_resolution_from_policy(policy: &ConflictPolicy) -> ConflictResolution {
    match policy {
        ConflictPolicy::ConflictCopy => ConflictResolution::ConflictCopy {
            suffix: format!(".conflict.{}", chrono::Utc::now().format("%Y%m%d%H%M%S")),
        },
        ConflictPolicy::LocalWins => ConflictResolution::LocalWins,
        ConflictPolicy::RemoteWins => ConflictResolution::RemoteWins,
        ConflictPolicy::Rename { suffix } => {
            let ts = chrono::Utc::now().format("%Y%m%d%H%M%S");
            let actual_suffix = if suffix.is_empty() {
                format!(".conflict.{ts}")
            } else {
                format!("{suffix}.{ts}")
            };
            ConflictResolution::Rename {
                suffix: actual_suffix,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    fn t(y: i32, m: u32, d: u32) -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(y, m, d, 12, 0, 0).unwrap()
    }

    fn path(name: &str) -> RelativePath {
        RelativePath::from(name)
    }

    fn local(md5: &str, mtime: chrono::DateTime<Utc>) -> LocalFile {
        LocalFile {
            path: path("ignored"),
            md5: md5.to_string(),
            mtime,
            size: 1,
        }
    }

    fn remote(id: &str, md5: Option<&str>, mtime: chrono::DateTime<Utc>) -> DriveFile {
        DriveFile {
            id: id.into(),
            name: "n".into(),
            mime_type: "text/plain".into(),
            md5_checksum: md5.map(String::from),
            modified_time: mtime,
            size: Some(1),
            head_revision_id: None,
            version: None,
            app_properties: std::collections::BTreeMap::new(),
            parents: vec![],
            trashed: false,
        }
    }

    fn remote_with_vv(
        id: &str,
        md5: Option<&str>,
        mtime: chrono::DateTime<Utc>,
        vv: &str,
    ) -> DriveFile {
        let mut remote = remote(id, md5, mtime);
        remote
            .app_properties
            .insert("ox_vv".to_string(), vv.to_string());
        remote
    }

    fn meta(
        local_md5: &str,
        local_mtime: chrono::DateTime<Utc>,
        remote_md5_stored: Option<&str>,
        drive_id: Option<&str>,
    ) -> SyncRecord {
        SyncRecord {
            drive_file_id: drive_id.map(String::from),
            remote_md5: remote_md5_stored.map(String::from),
            remote_mime_type: None,
            remote_modified_at: None,
            local_md5: local_md5.into(),
            local_mtime,
            local_size: 1,
            last_synced_at: local_mtime,
            remote_head_revision_id: None,
            remote_version: None,
            version_vector: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn matrix_1_skip_both_unchanged() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("a", t(2020, 1, 1))),
            Some(&remote("id", Some("b"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Skip { .. }));
    }

    #[test]
    fn matrix_2_upload_local_changed() {
        let m = meta("old", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("new", t(2020, 1, 2))),
            Some(&remote("id", Some("b"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Upload { remote_id: Some(ref id), .. } if id == "id"));
    }

    #[test]
    fn matrix_content_vs_meta_only_uses_touch_metadata_when_md5_is_same() {
        let m = meta("same-md5", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("same-md5", t(2020, 1, 2))),
            Some(&remote("id", Some("b"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::TouchMetadata { .. }));
    }

    #[test]
    fn matrix_3_download_remote_changed() {
        let m = meta("a", t(2020, 1, 1), Some("old"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("a", t(2020, 1, 1))),
            Some(&remote("id", Some("new"), t(2020, 1, 3))),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Download { remote_id, .. } if remote_id == "id"));
    }

    #[test]
    fn matrix_4_conflict_both_changed() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("a2", t(2020, 1, 2))),
            Some(&remote("id", Some("b2"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(
            a,
            SyncAction::Conflict {
                resolution: ConflictResolution::LocalWins,
                ..
            }
        ));
    }

    #[test]
    fn matrix_4_with_concurrent_vv_keeps_conflict_safe_default() {
        let mut m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        m.version_vector = std::collections::BTreeMap::from([
            ("alice".to_string(), 3_u64),
            ("bob".to_string(), 1_u64),
        ]);
        let a = determine_action(
            &path("f"),
            Some(&local("a2", t(2020, 1, 2))),
            Some(&remote_with_vv(
                "id",
                Some("b2"),
                t(2020, 1, 2),
                "alice:2;bob:2",
            )),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Conflict { .. }));
    }

    #[test]
    fn matrix_4_with_remote_dominated_by_stored_vv_prefers_upload() {
        let mut m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        m.version_vector = std::collections::BTreeMap::from([
            ("alice".to_string(), 3_u64),
            ("bob".to_string(), 1_u64),
        ]);
        let a = determine_action(
            &path("f"),
            Some(&local("a2", t(2020, 1, 2))),
            Some(&remote_with_vv("id", Some("b2"), t(2020, 1, 2), "alice:2")),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(
            a,
            SyncAction::Upload {
                remote_id: Some(ref id),
                ..
            } if id == "id"
        ));
    }

    #[test]
    fn matrix_4_conflict_policy_remote_wins() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("a2", t(2020, 1, 2))),
            Some(&remote("id", Some("b2"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::RemoteWins,
        );
        assert!(matches!(
            a,
            SyncAction::Conflict {
                resolution: ConflictResolution::RemoteWins,
                ..
            }
        ));
    }

    #[test]
    fn matrix_4_conflict_policy_rename_uses_configured_suffix_with_timestamp() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("a2", t(2020, 1, 2))),
            Some(&remote("id", Some("b2"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::Rename {
                suffix: "_ignored".into(),
            },
        );
        match a {
            SyncAction::Conflict {
                resolution: ConflictResolution::Rename { suffix },
                ..
            } => assert!(suffix.starts_with("_ignored.")),
            _ => panic!("expected conflict rename resolution"),
        }
    }

    #[test]
    fn matrix_4_default_policy_uses_conflict_copy() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("a2", t(2020, 1, 3))),
            Some(&remote("id", Some("b2"), t(2020, 1, 3))),
            Some(&m),
            &ConflictPolicy::default(),
        );
        match a {
            SyncAction::Conflict {
                resolution: ConflictResolution::ConflictCopy { suffix },
                ..
            } => assert!(suffix.starts_with(".conflict.")),
            _ => panic!("expected conflict_copy resolution"),
        }
    }

    #[test]
    fn matrix_5_no_meta_md5_equal_skips() {
        let a = determine_action(
            &path("f"),
            Some(&local("same", t(2020, 1, 1))),
            Some(&remote("id", Some("same"), t(2020, 1, 2))),
            None,
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Skip { .. }));
    }

    #[test]
    fn matrix_5_no_meta_md5_diff_conflict() {
        let a = determine_action(
            &path("f"),
            Some(&local("x", t(2020, 1, 1))),
            Some(&remote("id", Some("y"), t(2020, 1, 2))),
            None,
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Conflict { .. }));
    }

    #[test]
    fn matrix_6_delete_local_remote_gone_unchanged() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("a", t(2020, 1, 1))),
            None,
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::DeleteLocal { .. }));
    }

    #[test]
    fn delete_local_skipped_when_drive_object_present_under_other_path() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("drive-123"));
        let mut remote_ids = HashSet::new();
        remote_ids.insert("drive-123".to_string());
        let a = determine_action_with_remote_ids(
            &path("slides/templates/foo.html"),
            Some(&local("a", t(2020, 1, 1))),
            None,
            Some(&m),
            &ConflictPolicy::RemoteWins,
            &remote_ids,
        );
        assert!(
            matches!(a, SyncAction::Skip { .. }),
            "should skip instead of deleting when Drive object still exists elsewhere"
        );
    }

    #[test]
    fn changed_local_updates_existing_id_instead_of_duplicating_on_remap() {
        let m = meta("old", t(2020, 1, 1), Some("b"), Some("drive-123"));
        let mut remote_ids = HashSet::new();
        remote_ids.insert("drive-123".to_string());
        let a = determine_action_with_remote_ids(
            &path("slides/foo.html"),
            Some(&local("new", t(2020, 1, 2))),
            None,
            Some(&m),
            &ConflictPolicy::RemoteWins,
            &remote_ids,
        );
        assert!(
            matches!(a, SyncAction::Upload { remote_id: Some(ref id), .. } if id == "drive-123"),
            "should update the existing remapped Drive object, not create a duplicate"
        );
    }

    #[test]
    fn changed_local_uploads_as_new_when_drive_object_truly_gone() {
        let m = meta("old", t(2020, 1, 1), Some("b"), Some("drive-123"));
        let mut remote_ids = HashSet::new();
        remote_ids.insert("unrelated".to_string());
        let a = determine_action_with_remote_ids(
            &path("docs/foo.txt"),
            Some(&local("new", t(2020, 1, 2))),
            None,
            Some(&m),
            &ConflictPolicy::RemoteWins,
            &remote_ids,
        );
        assert!(matches!(
            a,
            SyncAction::Upload {
                remote_id: None,
                ..
            }
        ));
    }

    #[test]
    fn delete_local_still_fires_when_drive_object_truly_gone() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("drive-123"));
        let mut remote_ids = HashSet::new();
        remote_ids.insert("some-other-id".to_string());
        let a = determine_action_with_remote_ids(
            &path("docs/foo.txt"),
            Some(&local("a", t(2020, 1, 1))),
            None,
            Some(&m),
            &ConflictPolicy::RemoteWins,
            &remote_ids,
        );
        assert!(matches!(a, SyncAction::DeleteLocal { .. }));
    }

    #[test]
    fn converted_delete_local_skipped_when_drive_object_present() {
        let m = meta("ignored", t(2020, 1, 1), Some("b"), Some("drive-xyz"));
        let mut remote_ids = HashSet::new();
        remote_ids.insert("drive-xyz".to_string());
        let a = determine_action_converted_with_remote_ids(
            &path("slides/deck.pptx"),
            Some(&local("last-export", t(2020, 1, 1))),
            None,
            Some(&m),
            &ConflictPolicy::RemoteWins,
            true,
            Some("last-export"),
            &remote_ids,
        );
        assert!(matches!(a, SyncAction::Skip { .. }));
    }

    #[test]
    fn matrix_7_upload_after_remote_delete_local_changed() {
        let m = meta("old", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            Some(&local("new", t(2020, 1, 2))),
            None,
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(
            a,
            SyncAction::Upload {
                remote_id: None,
                ..
            }
        ));
    }

    #[test]
    fn matrix_8_upload_new_local_only() {
        let a = determine_action(
            &path("f"),
            Some(&local("a", t(2020, 1, 1))),
            None,
            None,
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(
            a,
            SyncAction::Upload {
                remote_id: None,
                ..
            }
        ));
    }

    #[test]
    fn matrix_9_delete_remote_local_gone_unchanged_remote() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(
            &path("f"),
            None,
            Some(&remote("id", Some("b"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::DeleteRemote { remote_id, .. } if remote_id == "id"));
    }

    #[test]
    fn matrix_10_download_local_deleted_remote_changed() {
        let m = meta("a", t(2020, 1, 1), Some("old"), Some("id"));
        let a = determine_action(
            &path("f"),
            None,
            Some(&remote("id", Some("new"), t(2020, 1, 3))),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Download { remote_id, .. } if remote_id == "id"));
    }

    #[test]
    fn matrix_11_download_new_remote_only() {
        let a = determine_action(
            &path("f"),
            None,
            Some(&remote("id", Some("b"), t(2020, 1, 2))),
            None,
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Download { remote_id, .. } if remote_id == "id"));
    }

    #[test]
    fn matrix_12_cleanup_both_absent_with_meta() {
        let m = meta("a", t(2020, 1, 1), Some("b"), Some("id"));
        let a = determine_action(&path("f"), None, None, Some(&m), &ConflictPolicy::LocalWins);
        assert!(matches!(a, SyncAction::CleanupMetadata { .. }));
    }

    #[test]
    fn edge_google_native_no_md5_first_sync_conflict() {
        let a = determine_action(
            &path("doc"),
            Some(&local("localhash", t(2020, 1, 1))),
            Some(&remote("id", None, t(2020, 1, 2))),
            None,
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Conflict { .. }));
    }

    #[test]
    fn edge_google_native_tracks_remote_mtime_in_meta() {
        let r = remote("id", None, t(2020, 1, 2));
        let m = SyncRecord {
            drive_file_id: Some("id".into()),
            remote_md5: Some(remote_content_fingerprint(&r)),
            remote_mime_type: Some(r.mime_type.clone()),
            remote_modified_at: Some(r.modified_time),
            local_md5: "a".into(),
            local_mtime: t(2020, 1, 1),
            local_size: 1,
            last_synced_at: t(2020, 1, 1),
            remote_head_revision_id: None,
            remote_version: None,
            version_vector: std::collections::BTreeMap::new(),
        };
        let a = determine_action(
            &path("doc"),
            Some(&local("a", t(2020, 1, 1))),
            Some(&r),
            Some(&m),
            &ConflictPolicy::LocalWins,
        );
        assert!(matches!(a, SyncAction::Skip { .. }));
    }

    #[test]
    fn edge_none_none_none_skips() {
        let a = determine_action(&path("ghost"), None, None, None, &ConflictPolicy::LocalWins);
        assert!(matches!(a, SyncAction::Skip { .. }));
    }

    #[test]
    fn converted_remote_changed_downloads() {
        let m = meta("a", t(2020, 1, 1), Some("old"), Some("id"));
        let a = determine_action_converted(
            &path("doc.docx"),
            Some(&local("last-export", t(2020, 1, 1))),
            Some(&remote("id", None, t(2020, 1, 3))),
            Some(&m),
            &ConflictPolicy::LocalWins,
            true,
            Some("last-export"),
        );
        assert!(matches!(a, SyncAction::Download { remote_id, .. } if remote_id == "id"));
    }

    #[test]
    fn converted_local_changed_uploads() {
        let m = meta("ignored", t(2020, 1, 1), Some("same"), Some("id"));
        let a = determine_action_converted(
            &path("sheet.xlsx"),
            Some(&local("local-edited", t(2020, 1, 2))),
            Some(&remote("id", Some("same"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::LocalWins,
            true,
            Some("last-export"),
        );
        assert!(matches!(a, SyncAction::Upload { remote_id: Some(ref id), .. } if id == "id"));
    }

    #[test]
    fn converted_unchanged_skips() {
        let r = remote("id", None, t(2020, 1, 2));
        let m = SyncRecord {
            drive_file_id: Some("id".into()),
            remote_md5: Some(remote_content_fingerprint(&r)),
            remote_mime_type: Some(r.mime_type.clone()),
            remote_modified_at: Some(r.modified_time),
            local_md5: "ignored".into(),
            local_mtime: t(2020, 1, 1),
            local_size: 1,
            last_synced_at: t(2020, 1, 1),
            remote_head_revision_id: None,
            remote_version: None,
            version_vector: std::collections::BTreeMap::new(),
        };
        let a = determine_action_converted(
            &path("slides.pptx"),
            Some(&local("last-export", t(2020, 1, 1))),
            Some(&r),
            Some(&m),
            &ConflictPolicy::LocalWins,
            true,
            Some("last-export"),
        );
        assert!(matches!(a, SyncAction::Skip { .. }));
    }

    #[test]
    fn converted_both_changed_conflicts() {
        let m = meta("ignored", t(2020, 1, 1), Some("old"), Some("id"));
        let a = determine_action_converted(
            &path("doc.docx"),
            Some(&local("local-edited", t(2020, 1, 2))),
            Some(&remote("id", Some("new"), t(2020, 1, 2))),
            Some(&m),
            &ConflictPolicy::LocalWins,
            true,
            Some("last-export"),
        );
        assert!(matches!(
            a,
            SyncAction::Conflict {
                resolution: ConflictResolution::LocalWins,
                ..
            }
        ));
    }
}
