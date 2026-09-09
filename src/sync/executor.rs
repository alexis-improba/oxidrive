//! Applies planned [`SyncAction`](crate::types::SyncAction) values with bounded upload/download concurrency.

use std::io::{self, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use indicatif::{ProgressBar, ProgressStyle};
use tokio::fs;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::drive::download::{download_file, export_file_with_fallback};
use crate::drive::list::find_remote_file_id_by_content;
use crate::drive::locks::{lease_is_active, parse_lease};
use crate::drive::types::{
    export_format_sync, is_google_workspace, remote_content_fingerprint, DriveFile, FOLDER,
};
use crate::drive::upload::{
    get_file_metadata, preflight_revision_mismatch, update_app_properties, update_file_with_resume,
    update_file_with_resume_guarded, upload_file_with_resume, upload_with_conversion_with_resume,
    GuardedUpdate, ResumableUploadState, RevisionGuard, RESUMABLE_UPLOAD_THRESHOLD_BYTES,
};
use crate::drive::DriveClient;
use crate::error::OxidriveError;
use crate::store::{RedbStore, Store};
use crate::sync::coordination::VersionVector;
use crate::sync::observability::{append_conflict_log, ConflictLogEntry};
use crate::sync::scan::{has_open_lock, is_stable};
use crate::types::{
    ConflictResolution, PendingOp, PendingOpKind, PendingOpStage, RelativePath, SyncAction,
    SyncRecord, SyncReport, Tombstone, UploadSession, UploadSessionMode, WorkspaceConversion,
    RESUMABLE_UPLOAD_SESSION_TTL_HOURS,
};
use crate::utils::hash::compute_md5;

type TaskResult = Result<Outcome, (RelativePath, OxidriveError)>;
const DELETE_CONFIRMATIONS_REQUIRED: u32 = 1;

/// Counts discrete sync steps so the progress bar total matches synchronous work plus join completions.
fn sync_progress_total(actions: &[SyncAction]) -> u64 {
    let mut n = 0u64;
    for action in actions {
        match action {
            SyncAction::Skip { .. }
            | SyncAction::CleanupMetadata { .. }
            | SyncAction::TouchMetadata { .. }
            | SyncAction::DeleteLocal { .. } => n += 1,
            SyncAction::Upload { .. }
            | SyncAction::Download { .. }
            | SyncAction::DeleteRemote { .. } => n += 1,
            SyncAction::Conflict {
                path,
                remote_id,
                resolution,
                ..
            } => match conflict_resolution_actions(path, remote_id.clone(), resolution.clone()) {
                Ok(follow_ups) => {
                    for fu in &follow_ups {
                        match fu {
                            SyncAction::Upload { .. } | SyncAction::Download { .. } => n += 1,
                            _ => n += 1,
                        }
                    }
                }
                Err(_) => n += 1,
            },
        }
    }
    n
}

/// Runs sync I/O with separate semaphores for uploads and downloads.
pub struct SyncExecutor {
    upload_sem: Arc<Semaphore>,
    download_sem: Arc<Semaphore>,
    stability_ms: u64,
    device_id: String,
    safe_delete: bool,
    use_leases: bool,
}

impl SyncExecutor {
    /// Creates an executor with the given concurrency limits (each at least one slot).
    pub fn new(
        max_uploads: usize,
        max_downloads: usize,
        stability_ms: u64,
        device_id: String,
        safe_delete: bool,
        use_leases: bool,
    ) -> Self {
        Self {
            upload_sem: Arc::new(Semaphore::new(max_uploads.max(1))),
            download_sem: Arc::new(Semaphore::new(max_downloads.max(1))),
            stability_ms,
            device_id,
            safe_delete,
            use_leases,
        }
    }

    /// Executes every action, returning an aggregated [`SyncReport`].
    pub async fn execute(
        &self,
        actions: Vec<SyncAction>,
        client: &DriveClient,
        store: &Store,
        redb: &RedbStore,
    ) -> Result<SyncReport, OxidriveError> {
        let started = Instant::now();
        let root = store.sync_dir().clone();
        let remote_snap = store.remote_snapshot()?.unwrap_or_default();
        let store = store.clone();
        let client = client.clone();
        let redb = redb.clone();
        let stale_upload_sessions = store.purge_stale_upload_sessions(chrono::Duration::hours(
            RESUMABLE_UPLOAD_SESSION_TTL_HOURS,
        ))?;
        if stale_upload_sessions > 0 {
            tracing::info!(
                stale_upload_sessions,
                "Removed stale resumable upload sessions"
            );
        }

        let mut report = SyncReport {
            uploaded: Vec::new(),
            downloaded: Vec::new(),
            deleted_local: Vec::new(),
            deleted_remote: Vec::new(),
            conflicts: Vec::new(),
            skipped: 0,
            errors: Vec::new(),
            duration: Duration::ZERO,
        };

        let total_steps = sync_progress_total(&actions);
        let pb = if io::stdout().is_terminal() {
            let pb = ProgressBar::new(total_steps);
            pb.set_style(
                ProgressStyle::with_template(
                    "{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} {msg}",
                )
                .unwrap_or_else(|_| ProgressStyle::default_bar()),
            );
            pb
        } else {
            ProgressBar::hidden()
        };

        let mut pending: JoinSet<TaskResult> = JoinSet::new();
        let stability_ms = self.stability_ms;
        let device_id = self.device_id.clone();
        let use_leases = self.use_leases;

        for action in actions {
            match action {
                SyncAction::Skip { .. } => {
                    report.skipped += 1;
                    pb.inc(1);
                    pb.set_message("skip");
                }
                SyncAction::Conflict {
                    path,
                    remote_id,
                    resolution,
                    ..
                } => {
                    let resolution =
                        tag_conflict_copy_resolution(resolution, self.device_id.as_str());
                    report.conflicts.push(path.clone());
                    if let ConflictResolution::Rename { suffix }
                    | ConflictResolution::ConflictCopy { suffix } = resolution.clone()
                    {
                        let Some(remote_id) = remote_id else {
                            report.errors.push((
                                path.clone(),
                                "conflict rename requires a remote id".to_string(),
                            ));
                            pb.inc(1);
                            pb.set_message("error");
                            continue;
                        };
                        let renamed_path = path_with_suffix(&path, &suffix);
                        let copy_path = renamed_path.clone();
                        let remote_meta = remote_snap.get(&path).cloned();
                        let listing_row = remote_meta.clone();
                        let remote_origin = remote_meta
                            .as_ref()
                            .and_then(|remote| remote.app_properties.get("ox_origin").cloned());
                        let resolution_label = match &resolution {
                            ConflictResolution::ConflictCopy { .. } => "conflict_copy",
                            ConflictResolution::Rename { .. } => "rename",
                            _ => "conflict",
                        };

                        tracing::warn!(
                            path = %path,
                            resolution = ?resolution,
                            action_count = 2,
                            "keeping both versions after a conflict"
                        );

                        let download = async {
                            let _permit = self
                                .download_sem
                                .acquire()
                                .await
                                .map_err(|e| OxidriveError::sync(e.to_string()))?;
                            run_download(
                                &client,
                                &store,
                                &redb,
                                &root,
                                renamed_path,
                                remote_id.clone(),
                                remote_meta.as_ref(),
                            )
                            .await
                        }
                        .await;

                        match download {
                            Ok(outcome) => {
                                apply_outcome(&mut report, outcome);
                                pb.inc(1);
                                pb.set_message("download");
                            }
                            Err(e) => {
                                report.errors.push((path.clone(), e.to_string()));
                                pb.inc(1);
                                pb.set_message("error");
                                continue;
                            }
                        }

                        let upload = async {
                            let _permit = self
                                .upload_sem
                                .acquire()
                                .await
                                .map_err(|e| OxidriveError::sync(e.to_string()))?;
                            run_upload(
                                &client,
                                &store,
                                &redb,
                                &root,
                                path.clone(),
                                Some(remote_id),
                                listing_row,
                                stability_ms,
                                false,
                                self.device_id.as_str(),
                                use_leases,
                            )
                            .await
                        }
                        .await;

                        match upload {
                            Ok(outcome) => {
                                apply_outcome(&mut report, outcome);
                                pb.inc(1);
                                pb.set_message("upload");
                                append_resolved_conflict_log(
                                    &root,
                                    &path,
                                    resolution_label,
                                    self.device_id.as_str(),
                                    remote_origin,
                                    Some(&copy_path),
                                );
                            }
                            Err(e) => {
                                report.errors.push((path.clone(), e.to_string()));
                                pb.inc(1);
                                pb.set_message("error");
                            }
                        }
                        continue;
                    }

                    let follow_ups = match conflict_resolution_actions(
                        &path,
                        remote_id.clone(),
                        resolution.clone(),
                    ) {
                        Ok(actions) => actions,
                        Err(e) => {
                            report.errors.push((path.clone(), e.to_string()));
                            pb.inc(1);
                            pb.set_message("conflict");
                            continue;
                        }
                    };
                    tracing::warn!(
                        path = %path,
                        resolution = ?resolution,
                        action_count = follow_ups.len(),
                        "keeping both versions after a conflict"
                    );

                    let original_remote_meta = remote_snap.get(&path).cloned();
                    for follow_up in follow_ups {
                        match follow_up {
                            SyncAction::Upload { path, remote_id } => {
                                let store = store.clone();
                                let client = client.clone();
                                let sem = Arc::clone(&self.upload_sem);
                                let root = root.clone();
                                let listing_row = remote_snap.get(&path).cloned();
                                let path_err = path.clone();
                                let redb = redb.clone();
                                let task_device_id = device_id.clone();
                                pending.spawn(async move {
                                    let res = async {
                                        let _permit = sem
                                            .acquire()
                                            .await
                                            .map_err(|e| OxidriveError::sync(e.to_string()))?;
                                        run_upload(
                                            &client,
                                            &store,
                                            &redb,
                                            &root,
                                            path,
                                            remote_id,
                                            listing_row,
                                            stability_ms,
                                            false,
                                            task_device_id.as_str(),
                                            use_leases,
                                        )
                                        .await
                                    }
                                    .await;
                                    res.map_err(|e| (path_err, e))
                                });
                            }
                            SyncAction::Download { path, remote_id } => {
                                let store = store.clone();
                                let client = client.clone();
                                let sem = Arc::clone(&self.download_sem);
                                let root = root.clone();
                                let remote_meta = remote_snap
                                    .get(&path)
                                    .cloned()
                                    .or_else(|| original_remote_meta.clone());
                                let path_err = path.clone();
                                let redb = redb.clone();
                                pending.spawn(async move {
                                    let res = async {
                                        let _permit = sem
                                            .acquire()
                                            .await
                                            .map_err(|e| OxidriveError::sync(e.to_string()))?;
                                        run_download(
                                            &client,
                                            &store,
                                            &redb,
                                            &root,
                                            path,
                                            remote_id,
                                            remote_meta.as_ref(),
                                        )
                                        .await
                                    }
                                    .await;
                                    res.map_err(|e| (path_err, e))
                                });
                            }
                            _ => {
                                report.errors.push((
                                    path.clone(),
                                    "invalid follow-up action generated for conflict".to_string(),
                                ));
                                pb.inc(1);
                                pb.set_message("error");
                            }
                        }
                    }
                }
                SyncAction::CleanupMetadata { path } => {
                    if let Err(e) = clear_path_state(&store, &path) {
                        report
                            .errors
                            .push((path.clone(), format!("cleanup metadata: {e}")));
                    }
                    pb.inc(1);
                    pb.set_message("cleanup");
                }
                SyncAction::Upload { path, remote_id } => {
                    let store = store.clone();
                    let client = client.clone();
                    let sem = Arc::clone(&self.upload_sem);
                    let root = root.clone();
                    let listing_row = remote_snap.get(&path).cloned();
                    let path_err = path.clone();
                    let redb = redb.clone();
                    let task_device_id = device_id.clone();
                    pending.spawn(async move {
                        let res = async {
                            let _permit = sem
                                .acquire()
                                .await
                                .map_err(|e| OxidriveError::sync(e.to_string()))?;
                            run_upload(
                                &client,
                                &store,
                                &redb,
                                &root,
                                path,
                                remote_id,
                                listing_row,
                                stability_ms,
                                true,
                                task_device_id.as_str(),
                                use_leases,
                            )
                            .await
                        }
                        .await;
                        res.map_err(|e| (path_err, e))
                    });
                }
                SyncAction::TouchMetadata { path } => {
                    if let Err(e) = run_touch_metadata(&store, &root, &path).await {
                        report
                            .errors
                            .push((path.clone(), format!("touch metadata: {e}")));
                        pb.inc(1);
                        pb.set_message("error");
                        continue;
                    }
                    report.skipped += 1;
                    pb.inc(1);
                    pb.set_message("touch metadata");
                }
                SyncAction::Download { path, remote_id } => {
                    let store = store.clone();
                    let client = client.clone();
                    let sem = Arc::clone(&self.download_sem);
                    let root = root.clone();
                    let remote_meta = remote_snap.get(&path).cloned();
                    let path_err = path.clone();
                    let redb = redb.clone();
                    pending.spawn(async move {
                        let res = async {
                            let _permit = sem
                                .acquire()
                                .await
                                .map_err(|e| OxidriveError::sync(e.to_string()))?;
                            run_download(
                                &client,
                                &store,
                                &redb,
                                &root,
                                path,
                                remote_id,
                                remote_meta.as_ref(),
                            )
                            .await
                        }
                        .await;
                        res.map_err(|e| (path_err, e))
                    });
                }
                SyncAction::DeleteLocal { path } => {
                    let p = path.clone();
                    let drive_file_id = store.get(&path)?.and_then(|record| record.drive_file_id);
                    if !confirm_delete_observation(
                        &redb,
                        &path,
                        drive_file_id.clone(),
                        self.device_id.as_str(),
                    )? {
                        tracing::info!(
                            path = %path,
                            required_confirmations = DELETE_CONFIRMATIONS_REQUIRED,
                            "Waiting for another cycle before deleting this local file"
                        );
                        report.skipped += 1;
                        pb.inc(1);
                        pb.set_message("defer delete");
                        continue;
                    }
                    if let Err(e) = set_pending_op(
                        &redb,
                        &p,
                        PendingOpKind::DeleteLocal,
                        PendingOpStage::Planned,
                    ) {
                        report
                            .errors
                            .push((p, format!("delete local pending op: {e}")));
                        pb.inc(1);
                        pb.set_message("error");
                        continue;
                    }
                    let full = match resolve_local_path(&root, &path) {
                        Ok(full) => full,
                        Err(e) => {
                            report.errors.push((p, format!("delete local: {e}")));
                            let _ = clear_pending_op(&redb, &path);
                            pb.inc(1);
                            pb.set_message("error");
                            continue;
                        }
                    };
                    if let Err(e) = set_pending_op(
                        &redb,
                        &path,
                        PendingOpKind::DeleteLocal,
                        PendingOpStage::SideEffectStarted,
                    ) {
                        report
                            .errors
                            .push((path.clone(), format!("delete local pending op: {e}")));
                        pb.inc(1);
                        pb.set_message("error");
                        continue;
                    }
                    let deletion_result = if self.safe_delete {
                        move_local_file_to_trash(&root, &path, &full).await
                    } else {
                        fs::remove_file(&full).await.map_err(|e| {
                            OxidriveError::sync(format!("remove {}: {e}", full.display()))
                        })
                    };
                    match deletion_result {
                        Ok(()) => {
                            if let Err(e) = set_pending_op(
                                &redb,
                                &path,
                                PendingOpKind::DeleteLocal,
                                PendingOpStage::SideEffectDone,
                            ) {
                                report
                                    .errors
                                    .push((path.clone(), format!("delete local pending op: {e}")));
                                pb.inc(1);
                                pb.set_message("error");
                                continue;
                            }
                            if let Err(e) = clear_path_state(&store, &p) {
                                report
                                    .errors
                                    .push((p.clone(), format!("delete local metadata: {e}")));
                            } else {
                                report.deleted_local.push(p.clone());
                                if let Err(e) = mark_pending_metadata_committed(
                                    &redb,
                                    &p,
                                    PendingOpKind::DeleteLocal,
                                ) {
                                    report
                                        .errors
                                        .push((p.clone(), format!("delete local pending op: {e}")));
                                }
                            }
                            let _ = write_tombstone(
                                &redb,
                                &path,
                                Tombstone {
                                    drive_file_id,
                                    deleted_at: Utc::now(),
                                    by_device: self.device_id.clone(),
                                    confirmations: 0,
                                },
                            );
                        }
                        Err(e) => {
                            report
                                .errors
                                .push((p.clone(), format!("delete local: {e}")));
                            let _ = clear_pending_op(&redb, &p);
                        }
                    }
                    pb.inc(1);
                    pb.set_message("delete local");
                }
                SyncAction::DeleteRemote { path, remote_id } => {
                    // Symmetric safety: require the local deletion to be observed
                    // across the confirmation threshold before trashing the remote
                    // file, so an accidental or transient local `rm` is not
                    // propagated to every other device on the first cycle.
                    let drive_file_id = store
                        .get(&path)?
                        .and_then(|record| record.drive_file_id)
                        .or_else(|| Some(remote_id.clone()));
                    if !confirm_delete_observation(
                        &redb,
                        &path,
                        drive_file_id,
                        self.device_id.as_str(),
                    )? {
                        tracing::info!(
                            path = %path,
                            required_confirmations = DELETE_CONFIRMATIONS_REQUIRED,
                            "Waiting for another cycle before trashing this remote file"
                        );
                        report.skipped += 1;
                        pb.inc(1);
                        pb.set_message("defer delete");
                        continue;
                    }
                    let client = client.clone();
                    let store = store.clone();
                    let redb = redb.clone();
                    let sem = Arc::clone(&self.upload_sem);
                    let path_err = path.clone();
                    let task_device_id = device_id.clone();
                    pending.spawn(async move {
                        let res = async {
                            let _permit = sem
                                .acquire()
                                .await
                                .map_err(|e| OxidriveError::sync(e.to_string()))?;
                            run_trash_remote(
                                &client,
                                &store,
                                &redb,
                                path,
                                remote_id,
                                task_device_id.as_str(),
                            )
                            .await
                        }
                        .await;
                        res.map_err(|e| (path_err, e))
                    });
                }
            }
        }

        while let Some(joined) = pending.join_next().await {
            match joined {
                Ok(Ok(Outcome::RevisionMismatch {
                    path,
                    remote_id,
                    remote,
                })) => {
                    report.conflicts.push(path.clone());
                    tracing::warn!(
                        path = %path,
                        drive_file_id = %remote_id,
                        remote_head_revision_id = ?remote.head_revision_id,
                        remote_version = ?remote.version,
                        "remote file changed during upload; keeping both versions"
                    );
                    let conflict_copy = ConflictResolution::ConflictCopy {
                        suffix: conflict_copy_suffix(self.device_id.as_str()),
                    };
                    let follow_ups = match conflict_resolution_actions(
                        &path,
                        Some(remote_id.clone()),
                        conflict_copy,
                    ) {
                        Ok(actions) => actions,
                        Err(e) => {
                            report.errors.push((path.clone(), e.to_string()));
                            pb.inc(1);
                            pb.set_message("error");
                            continue;
                        }
                    };
                    let remote_origin = remote.app_properties.get("ox_origin").cloned();
                    let conflict_copy_path = follow_ups.iter().find_map(|action| match action {
                        SyncAction::Download {
                            path: candidate, ..
                        } if candidate != &path => Some(candidate.clone()),
                        _ => None,
                    });

                    let mut follow_up_failed = false;
                    for follow_up in follow_ups {
                        match follow_up {
                            SyncAction::Download { path, remote_id } => {
                                let follow_up_path = path.clone();
                                let result = async {
                                    let _permit = self
                                        .download_sem
                                        .acquire()
                                        .await
                                        .map_err(|e| OxidriveError::sync(e.to_string()))?;
                                    run_download(
                                        &client,
                                        &store,
                                        &redb,
                                        &root,
                                        path,
                                        remote_id,
                                        Some(remote.as_ref()),
                                    )
                                    .await
                                }
                                .await;
                                match result {
                                    Ok(outcome) => apply_outcome(&mut report, outcome),
                                    Err(e) => {
                                        report.errors.push((follow_up_path, e.to_string()));
                                        follow_up_failed = true;
                                        break;
                                    }
                                }
                            }
                            SyncAction::Upload { path, remote_id } => {
                                let follow_up_path = path.clone();
                                let result = async {
                                    let _permit = self
                                        .upload_sem
                                        .acquire()
                                        .await
                                        .map_err(|e| OxidriveError::sync(e.to_string()))?;
                                    run_upload(
                                        &client,
                                        &store,
                                        &redb,
                                        &root,
                                        path,
                                        remote_id,
                                        Some((*remote).clone()),
                                        stability_ms,
                                        false,
                                        self.device_id.as_str(),
                                        use_leases,
                                    )
                                    .await
                                }
                                .await;
                                match result {
                                    Ok(outcome) => apply_outcome(&mut report, outcome),
                                    Err(e) => {
                                        report.errors.push((follow_up_path, e.to_string()));
                                        follow_up_failed = true;
                                        break;
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    pb.inc(1);
                    pb.set_message(if follow_up_failed {
                        "error"
                    } else {
                        "conflict_copy"
                    });
                    if !follow_up_failed {
                        append_resolved_conflict_log(
                            &root,
                            &path,
                            "revision_mismatch",
                            self.device_id.as_str(),
                            remote_origin,
                            conflict_copy_path.as_ref(),
                        );
                    }
                }
                Ok(Ok(outcome)) => {
                    let msg = match &outcome {
                        Outcome::Uploaded(_) => "upload",
                        Outcome::Downloaded(_) => "download",
                        Outcome::DeletedRemote(_) => "delete remote",
                        Outcome::RevisionMismatch { .. } => "conflict_copy",
                        Outcome::Skipped => "skip",
                    };
                    apply_outcome(&mut report, outcome);
                    pb.inc(1);
                    pb.set_message(msg);
                }
                Ok(Err((p, e))) => {
                    report.errors.push((p, e.to_string()));
                    pb.inc(1);
                    pb.set_message("error");
                }
                Err(j) => {
                    report
                        .errors
                        .push((RelativePath::from("__join__"), format!("task join: {j}")));
                    pb.inc(1);
                    pb.set_message("error");
                }
            }
        }

        pb.finish_with_message("Sync complete");
        report.duration = started.elapsed();
        Ok(report)
    }
}

enum Outcome {
    Uploaded(RelativePath),
    Downloaded(RelativePath),
    DeletedRemote(RelativePath),
    RevisionMismatch {
        path: RelativePath,
        remote_id: String,
        remote: Box<DriveFile>,
    },
    Skipped,
}

fn apply_outcome(report: &mut SyncReport, o: Outcome) {
    match o {
        Outcome::Uploaded(p) => report.uploaded.push(p),
        Outcome::Downloaded(p) => report.downloaded.push(p),
        Outcome::DeletedRemote(p) => report.deleted_remote.push(p),
        Outcome::RevisionMismatch { .. } => {}
        Outcome::Skipped => {
            report.skipped += 1;
        }
    }
}

fn append_resolved_conflict_log(
    root: &Path,
    path: &RelativePath,
    resolution: &str,
    local_device: &str,
    remote_origin: Option<String>,
    copy_path: Option<&RelativePath>,
) {
    let entry = ConflictLogEntry {
        timestamp: Utc::now(),
        path: path.as_str().to_string(),
        resolution: resolution.to_string(),
        local_device: local_device.to_string(),
        remote_origin,
        copy_path: copy_path.map(|p| p.as_str().to_string()),
    };
    if let Err(error) = append_conflict_log(&root.join(".oxidrive"), &entry) {
        tracing::warn!(
            path = %path,
            resolution,
            error = %error,
            "could not write the conflict log"
        );
    }
}

fn session_is_valid(
    session: &UploadSession,
    expected_mode: &UploadSessionMode,
    local_md5: &str,
    local_size: u64,
) -> bool {
    if session.mode != *expected_mode
        || session.local_md5 != local_md5
        || session.file_size != local_size
        || session.next_offset >= local_size
    {
        return false;
    }
    let age = Utc::now() - session.updated_at;
    age <= chrono::Duration::hours(RESUMABLE_UPLOAD_SESSION_TTL_HOURS)
}

fn revision_guard_from_record(record: Option<&SyncRecord>) -> RevisionGuard {
    let Some(record) = record else {
        return RevisionGuard::default();
    };
    RevisionGuard {
        head_revision_id: record.remote_head_revision_id.clone(),
        version: record.remote_version,
        remote_fingerprint: record.remote_md5.clone(),
        modified_time: record.remote_modified_at,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_upload(
    client: &DriveClient,
    store: &Store,
    redb: &RedbStore,
    root: &Path,
    path: RelativePath,
    remote_id: Option<String>,
    listing_row: Option<DriveFile>,
    stability_ms: u64,
    enforce_revision_guard: bool,
    device_id: &str,
    use_leases: bool,
) -> Result<Outcome, OxidriveError> {
    let full = resolve_local_path(root, &path)?;
    if has_open_lock(root, &path) {
        tracing::debug!(
            path = %path,
            "deferring upload because a matching lock file indicates the document is open"
        );
        return Ok(Outcome::Skipped);
    }
    if use_leases {
        let lease_remote = match (listing_row.clone(), remote_id.as_deref()) {
            (Some(remote), _) => Some(remote),
            (None, Some(id)) if !id.is_empty() => Some(get_file_metadata(client, id).await?),
            _ => None,
        };
        if let Some(remote) = lease_remote.as_ref() {
            if skip_upload_for_active_foreign_lease(redb, &path, remote, device_id) {
                return Ok(Outcome::Skipped);
            }
        }
    }
    let local_meta = fs::metadata(&full)
        .await
        .map_err(|e| OxidriveError::sync(format!("stat {}: {e}", full.display())))?;
    let local_mtime = local_meta
        .modified()
        .map_err(|e| OxidriveError::sync(format!("mtime {}: {e}", full.display())))?;
    let local_mtime_utc = DateTime::<Utc>::from(local_mtime);
    if !is_stable(local_mtime_utc, Utc::now(), stability_ms) {
        tracing::debug!(
            path = %path,
            stability_ms,
            "deferring unstable local upload candidate"
        );
        return Ok(Outcome::Skipped);
    }
    let name = path
        .as_str()
        .rsplit_once('/')
        .map(|(_, n)| n)
        .unwrap_or_else(|| path.as_str());
    let root_folder_id = store
        .root_drive_folder_id()?
        .ok_or_else(|| OxidriveError::sync("root drive folder id not set on store"))?;
    let parent_id = store.parent_drive_id(&path, &root_folder_id)?;
    let conversion = store.get_conversion(&path)?;
    let local_size = local_meta.len();
    let local_md5 = compute_md5(&full).await?;
    let existing_record = store.get(&path)?;
    let stored_vv = existing_record
        .as_ref()
        .map_or_else(VersionVector::default, |record| {
            VersionVector::from_map(&record.version_vector)
        });
    let remote_vv = listing_row
        .as_ref()
        .map_or_else(VersionVector::default, |remote| {
            VersionVector::from_app_properties(&remote.app_properties)
        });
    let mut next_vv = stored_vv.merge(&remote_vv);
    next_vv.increment(device_id);
    let mut next_app_properties = listing_row
        .as_ref()
        .map_or_else(std::collections::BTreeMap::new, |remote| {
            remote.app_properties.clone()
        });
    next_vv.write_into_app_properties(&mut next_app_properties, device_id);
    set_pending_op(redb, &path, PendingOpKind::Upload, PendingOpStage::Planned)?;
    match remote_id {
        Some(ref id) if !id.is_empty() => {
            let session_mode = if let Some(c) = conversion.as_ref() {
                UploadSessionMode::Convert {
                    drive_id: id.clone(),
                    google_mime: c.google_mime.clone(),
                }
            } else {
                UploadSessionMode::Update {
                    drive_id: id.clone(),
                }
            };
            let resume_state = if local_size > RESUMABLE_UPLOAD_THRESHOLD_BYTES {
                match store.get_upload_session(&path)? {
                    Some(session)
                        if session_is_valid(&session, &session_mode, &local_md5, local_size) =>
                    {
                        Some(ResumableUploadState {
                            session_url: session.session_url,
                            next_offset: session.next_offset,
                            file_size: session.file_size,
                        })
                    }
                    Some(_) => {
                        let _ = store.remove_upload_session(&path);
                        None
                    }
                    None => None,
                }
            } else {
                let _ = store.remove_upload_session(&path);
                None
            };
            set_pending_op(
                redb,
                &path,
                PendingOpKind::Upload,
                PendingOpStage::SideEffectStarted,
            )?;

            let _media_remote = if let Some(c) = conversion.as_ref() {
                // Guarded conversion upload: Workspace files have no md5, so the
                // preflight relies on headRevisionId/version/modifiedTime. On a
                // mismatch we degrade to a conflict copy instead of overwriting a
                // concurrently-edited Drive revision.
                if enforce_revision_guard {
                    let expected = revision_guard_from_record(existing_record.as_ref());
                    if let Some(remote) = preflight_revision_mismatch(client, id, &expected).await?
                    {
                        clear_pending_op(redb, &path)?;
                        return Ok(Outcome::RevisionMismatch {
                            path,
                            remote_id: id.clone(),
                            remote: Box::new(remote),
                        });
                    }
                }
                let path_for_session = path.clone();
                let mode_for_session = session_mode.clone();
                let local_md5_for_session = local_md5.clone();
                let store_for_session = store.clone();
                upload_with_conversion_with_resume(
                    client,
                    &full,
                    id,
                    &c.google_mime,
                    resume_state,
                    move |state| {
                        store_for_session.upsert_upload_session(
                            path_for_session.clone(),
                            UploadSession {
                                mode: mode_for_session.clone(),
                                session_url: state.session_url,
                                next_offset: state.next_offset,
                                file_size: state.file_size,
                                local_md5: local_md5_for_session.clone(),
                                updated_at: Utc::now(),
                            },
                        )
                    },
                )
                .await?;
                Some(get_file_metadata(client, id).await?)
            } else if enforce_revision_guard {
                let expected = revision_guard_from_record(existing_record.as_ref());
                let path_for_session = path.clone();
                let mode_for_session = session_mode.clone();
                let local_md5_for_session = local_md5.clone();
                let store_for_session = store.clone();
                let guarded = update_file_with_resume_guarded(
                    client,
                    &full,
                    id,
                    &expected,
                    resume_state,
                    move |state| {
                        store_for_session.upsert_upload_session(
                            path_for_session.clone(),
                            UploadSession {
                                mode: mode_for_session.clone(),
                                session_url: state.session_url,
                                next_offset: state.next_offset,
                                file_size: state.file_size,
                                local_md5: local_md5_for_session.clone(),
                                updated_at: Utc::now(),
                            },
                        )
                    },
                )
                .await?;
                match guarded {
                    GuardedUpdate::Updated { remote } => Some(remote),
                    GuardedUpdate::RevisionMismatch { remote } => {
                        clear_pending_op(redb, &path)?;
                        return Ok(Outcome::RevisionMismatch {
                            path,
                            remote_id: id.clone(),
                            remote: Box::new(remote),
                        });
                    }
                }
            } else {
                let path_for_session = path.clone();
                let mode_for_session = session_mode.clone();
                let local_md5_for_session = local_md5.clone();
                let store_for_session = store.clone();
                update_file_with_resume(client, &full, id, resume_state, move |state| {
                    store_for_session.upsert_upload_session(
                        path_for_session.clone(),
                        UploadSession {
                            mode: mode_for_session.clone(),
                            session_url: state.session_url,
                            next_offset: state.next_offset,
                            file_size: state.file_size,
                            local_md5: local_md5_for_session.clone(),
                            updated_at: Utc::now(),
                        },
                    )
                })
                .await?;
                Some(get_file_metadata(client, id).await?)
            };

            set_pending_op(
                redb,
                &path,
                PendingOpKind::Upload,
                PendingOpStage::SideEffectDone,
            )?;
            let refreshed_remote = update_app_properties(client, id, &next_app_properties).await?;
            let merged = Some(refreshed_remote);
            upsert_local_record(
                store,
                &path,
                &full,
                merged.as_ref(),
                Some(local_md5.as_str()),
                Some(local_size),
            )
            .await?;
            if let Some(mut c) = conversion {
                c.last_export_md5 = Some(local_md5.clone());
                store.upsert_conversion(path.clone(), c)?;
            }
            let _ = store.remove_upload_session(&path);
        }
        _ => {
            // Guard against creating a duplicate of an identical file that already exists on
            // Drive but is not yet tracked locally (e.g. incremental cycle with a stub remote
            // view). Reusing the id only on an exact md5 match never overwrites a distinct file.
            let existing_identical =
                find_remote_file_id_by_content(client, name, &parent_id, &local_md5).await?;
            let new_id = if let Some(existing_id) = existing_identical {
                tracing::warn!(
                    path = %path,
                    drive_file_id = %existing_id,
                    "identical file already exists on Drive; linking instead of uploading a duplicate"
                );
                let _ = store.remove_upload_session(&path);
                existing_id
            } else {
                let session_mode = UploadSessionMode::Create {
                    parent_id: parent_id.clone(),
                    name: name.to_string(),
                };
                let resume_state = if local_size > RESUMABLE_UPLOAD_THRESHOLD_BYTES {
                    match store.get_upload_session(&path)? {
                        Some(session)
                            if session_is_valid(
                                &session,
                                &session_mode,
                                &local_md5,
                                local_size,
                            ) =>
                        {
                            Some(ResumableUploadState {
                                session_url: session.session_url,
                                next_offset: session.next_offset,
                                file_size: session.file_size,
                            })
                        }
                        Some(_) => {
                            let _ = store.remove_upload_session(&path);
                            None
                        }
                        None => None,
                    }
                } else {
                    let _ = store.remove_upload_session(&path);
                    None
                };
                set_pending_op(
                    redb,
                    &path,
                    PendingOpKind::Upload,
                    PendingOpStage::SideEffectStarted,
                )?;
                let path_for_session = path.clone();
                let mode_for_session = session_mode.clone();
                let local_md5_for_session = local_md5.clone();
                let store_for_session = store.clone();
                let id = upload_file_with_resume(
                    client,
                    &full,
                    &parent_id,
                    name,
                    Some(&next_app_properties),
                    resume_state,
                    move |state| {
                        store_for_session.upsert_upload_session(
                            path_for_session.clone(),
                            UploadSession {
                                mode: mode_for_session.clone(),
                                session_url: state.session_url,
                                next_offset: state.next_offset,
                                file_size: state.file_size,
                                local_md5: local_md5_for_session.clone(),
                                updated_at: Utc::now(),
                            },
                        )
                    },
                )
                .await?;
                set_pending_op(
                    redb,
                    &path,
                    PendingOpKind::Upload,
                    PendingOpStage::SideEffectDone,
                )?;
                id
            };
            let remote_after_create = get_file_metadata(client, &new_id).await?;
            upsert_local_record(
                store,
                &path,
                &full,
                Some(&remote_after_create),
                Some(local_md5.as_str()),
                Some(local_size),
            )
            .await?;
            if let Some(mut c) = conversion {
                c.last_export_md5 = Some(local_md5);
                store.upsert_conversion(path.clone(), c)?;
            } else {
                let _ = store.remove_conversion(&path);
            }
            let _ = store.remove_upload_session(&path);
        }
    }
    if let Err(e) = mark_pending_metadata_committed(redb, &path, PendingOpKind::Upload) {
        let _ = clear_path_state(store, &path);
        return Err(OxidriveError::sync(format!(
            "mark upload metadata committed for '{}': {e}",
            path
        )));
    }
    Ok(Outcome::Uploaded(path))
}

async fn run_download(
    client: &DriveClient,
    store: &Store,
    redb: &RedbStore,
    root: &Path,
    path: RelativePath,
    remote_id: String,
    remote_meta: Option<&DriveFile>,
) -> Result<Outcome, OxidriveError> {
    let mut local_path = path.clone();
    let mut full = resolve_local_path(root, &local_path)?;
    if let Some(dir) = full.parent() {
        fs::create_dir_all(dir)
            .await
            .map_err(|e| OxidriveError::sync(format!("mkdir {}: {e}", dir.display())))?;
    }
    set_pending_op(
        redb,
        &path,
        PendingOpKind::Download,
        PendingOpStage::Planned,
    )?;

    if let Some(r) = remote_meta {
        if r.mime_type == FOLDER {
            set_pending_op(
                redb,
                &path,
                PendingOpKind::Download,
                PendingOpStage::SideEffectStarted,
            )?;
            fs::create_dir_all(&full).await.map_err(|e| {
                OxidriveError::sync(format!("mkdir folder {}: {e}", full.display()))
            })?;
            set_pending_op(
                redb,
                &path,
                PendingOpKind::Download,
                PendingOpStage::SideEffectDone,
            )?;
            if let Err(e) = mark_pending_metadata_committed(redb, &path, PendingOpKind::Download) {
                return Err(OxidriveError::sync(format!(
                    "mark folder download metadata committed for '{}': {e}",
                    path
                )));
            }
            tracing::debug!(path = %path, "created local folder mirror");
            return Ok(Outcome::Downloaded(path));
        }
        if is_google_workspace(&r.mime_type) {
            let fmt = export_format_sync(&r.mime_type).ok_or_else(|| {
                OxidriveError::sync(format!(
                    "missing sync export format for mime '{}'",
                    r.mime_type
                ))
            })?;
            local_path = converted_relative_path(&path, fmt.extension);
            full = resolve_local_path(root, &local_path)?;
            if let Some(dir) = full.parent() {
                fs::create_dir_all(dir)
                    .await
                    .map_err(|e| OxidriveError::sync(format!("mkdir {}: {e}", dir.display())))?;
            }
            set_pending_op(
                redb,
                &path,
                PendingOpKind::Download,
                PendingOpStage::SideEffectStarted,
            )?;
            export_file_with_fallback(client, &remote_id, fmt.export_mime, &full).await?;
            set_pending_op(
                redb,
                &path,
                PendingOpKind::Download,
                PendingOpStage::SideEffectDone,
            )?;
            let export_md5 =
                upsert_local_record(store, &local_path, &full, Some(r), None, None).await?;
            store.upsert_conversion(
                local_path.clone(),
                WorkspaceConversion {
                    drive_file_id: remote_id.clone(),
                    google_mime: r.mime_type.clone(),
                    last_export_md5: Some(export_md5),
                },
            )?;
            let _ = store.remove_upload_session(&local_path);
            if local_path != path {
                let _ = store.remove(&path);
                let _ = store.remove_conversion(&path);
                let _ = store.remove_upload_session(&path);
            }
            if let Err(e) = mark_pending_metadata_committed(redb, &path, PendingOpKind::Download) {
                let _ = clear_path_state(store, &local_path);
                if local_path != path {
                    let _ = clear_path_state(store, &path);
                }
                return Err(OxidriveError::sync(format!(
                    "mark export download metadata committed for '{}': {e}",
                    path
                )));
            }
            return Ok(Outcome::Downloaded(local_path));
        }
    }

    set_pending_op(
        redb,
        &path,
        PendingOpKind::Download,
        PendingOpStage::SideEffectStarted,
    )?;
    download_file(client, &remote_id, &full).await?;
    set_pending_op(
        redb,
        &path,
        PendingOpKind::Download,
        PendingOpStage::SideEffectDone,
    )?;
    let r = remote_meta.cloned();
    let _ = upsert_local_record(store, &local_path, &full, r.as_ref(), None, None).await?;
    let _ = store.remove_conversion(&local_path);
    let _ = store.remove_upload_session(&local_path);
    if let Err(e) = mark_pending_metadata_committed(redb, &path, PendingOpKind::Download) {
        let _ = clear_path_state(store, &local_path);
        if local_path != path {
            let _ = clear_path_state(store, &path);
        }
        return Err(OxidriveError::sync(format!(
            "mark binary download metadata committed for '{}': {e}",
            path
        )));
    }
    Ok(Outcome::Downloaded(path))
}

async fn run_touch_metadata(
    store: &Store,
    root: &Path,
    path: &RelativePath,
) -> Result<(), OxidriveError> {
    let full = resolve_local_path(root, path)?;
    let existing = store.get(path)?.ok_or_else(|| {
        OxidriveError::sync(format!(
            "missing sync metadata while applying touch_metadata for '{}'",
            path
        ))
    })?;
    let local_meta = fs::metadata(&full)
        .await
        .map_err(|e| OxidriveError::sync(format!("stat {}: {e}", full.display())))?;
    let local_size = local_meta.len();
    let local_md5 = compute_md5(&full).await?;
    let mtime = local_meta
        .modified()
        .map_err(|e| OxidriveError::sync(format!("mtime {}: {e}", full.display())))?;
    let record = SyncRecord {
        drive_file_id: existing.drive_file_id,
        remote_md5: existing.remote_md5,
        remote_mime_type: existing.remote_mime_type,
        remote_modified_at: existing.remote_modified_at,
        local_md5,
        local_mtime: DateTime::<Utc>::from(mtime),
        local_size,
        last_synced_at: Utc::now(),
        remote_head_revision_id: existing.remote_head_revision_id,
        remote_version: existing.remote_version,
        version_vector: existing.version_vector,
    };
    store.upsert(path.clone(), record)?;
    Ok(())
}

fn resolve_local_path(root: &Path, path: &RelativePath) -> Result<PathBuf, OxidriveError> {
    if !path.is_safe_non_empty() {
        return Err(OxidriveError::sync(format!(
            "unsafe relative path '{}'",
            path.as_str()
        )));
    }
    Ok(root.join(path.as_str()))
}

async fn run_trash_remote(
    client: &DriveClient,
    store: &Store,
    redb: &RedbStore,
    path: RelativePath,
    remote_id: String,
    device_id: &str,
) -> Result<Outcome, OxidriveError> {
    set_pending_op(
        redb,
        &path,
        PendingOpKind::DeleteRemote,
        PendingOpStage::Planned,
    )?;
    set_pending_op(
        redb,
        &path,
        PendingOpKind::DeleteRemote,
        PendingOpStage::SideEffectStarted,
    )?;
    let body = Arc::new(serde_json::json!({ "trashed": true }));
    let url = client.drive_api_url(&format!("/files/{remote_id}?supportsAllDrives=true"));
    let _ = client
        .request(reqwest::Method::PATCH, &url, move |b| b.json(body.as_ref()))
        .await?;
    set_pending_op(
        redb,
        &path,
        PendingOpKind::DeleteRemote,
        PendingOpStage::SideEffectDone,
    )?;
    let _ = write_tombstone(
        redb,
        &path,
        Tombstone {
            drive_file_id: Some(remote_id.clone()),
            deleted_at: Utc::now(),
            by_device: device_id.to_string(),
            confirmations: 0,
        },
    );
    clear_path_state(store, &path)?;
    mark_pending_metadata_committed(redb, &path, PendingOpKind::DeleteRemote)?;
    Ok(Outcome::DeletedRemote(path))
}

fn mark_pending_metadata_committed(
    redb: &RedbStore,
    path: &RelativePath,
    kind: PendingOpKind,
) -> Result<(), OxidriveError> {
    set_pending_op(redb, path, kind, PendingOpStage::MetadataCommitted)
}

fn set_pending_op(
    redb: &RedbStore,
    path: &RelativePath,
    kind: PendingOpKind,
    stage: PendingOpStage,
) -> Result<(), OxidriveError> {
    let entry = PendingOp {
        kind,
        stage,
        updated_at: Utc::now(),
    };
    let data = bincode::serialize(&entry)
        .map_err(|e| OxidriveError::store(format!("encode pending op for '{}': {e}", path)))?;
    redb.set_pending_op_sync(path.as_str(), &data)
}

fn clear_pending_op(redb: &RedbStore, path: &RelativePath) -> Result<(), OxidriveError> {
    redb.delete_pending_op_sync(path.as_str())
}

fn clear_path_state(store: &Store, path: &RelativePath) -> Result<(), OxidriveError> {
    let mut issues = Vec::new();
    if let Err(e) = store.remove(path) {
        issues.push(format!("sync record: {e}"));
    }
    if let Err(e) = store.remove_conversion(path) {
        issues.push(format!("conversion: {e}"));
    }
    if let Err(e) = store.remove_upload_session(path) {
        issues.push(format!("upload session: {e}"));
    }
    if issues.is_empty() {
        Ok(())
    } else {
        Err(OxidriveError::sync(issues.join("; ")))
    }
}

async fn upsert_local_record(
    store: &Store,
    path: &RelativePath,
    local_path: &std::path::Path,
    remote: Option<&DriveFile>,
    known_md5: Option<&str>,
    known_size: Option<u64>,
) -> Result<String, OxidriveError> {
    let md5 = match known_md5 {
        Some(value) => value.to_string(),
        None => compute_md5(local_path).await?,
    };
    let meta = fs::metadata(local_path)
        .await
        .map_err(|e| OxidriveError::sync(format!("stat {}: {e}", local_path.display())))?;
    let mtime = meta
        .modified()
        .map_err(|e| OxidriveError::sync(format!("mtime: {e}")))?;
    let mtime_utc = DateTime::<Utc>::from(mtime);

    let record = SyncRecord {
        drive_file_id: remote.map(|r| r.id.clone()),
        remote_md5: remote.map(remote_content_fingerprint),
        remote_mime_type: remote.map(|r| r.mime_type.clone()),
        remote_modified_at: remote.map(|r| r.modified_time),
        local_md5: md5.clone(),
        local_mtime: mtime_utc,
        local_size: known_size.unwrap_or(meta.len()),
        last_synced_at: Utc::now(),
        remote_head_revision_id: remote.and_then(|r| r.head_revision_id.clone()),
        remote_version: remote.and_then(|r| r.version),
        version_vector: remote.map_or_else(std::collections::BTreeMap::new, |r| {
            VersionVector::from_app_properties(&r.app_properties).into_map()
        }),
    };
    store.upsert(path.clone(), record)?;
    Ok(md5)
}

fn tag_conflict_copy_resolution(
    resolution: ConflictResolution,
    device_id: &str,
) -> ConflictResolution {
    match resolution {
        ConflictResolution::ConflictCopy { .. } => ConflictResolution::ConflictCopy {
            suffix: conflict_copy_suffix(device_id),
        },
        other => other,
    }
}

fn conflict_copy_suffix(device_id: &str) -> String {
    format!(
        ".conflict.{}.{}",
        suffix_device_component(device_id),
        chrono::Utc::now().format("%Y%m%d%H%M%S")
    )
}

fn suffix_device_component(device_id: &str) -> String {
    let mut out = String::with_capacity(device_id.len());
    for ch in device_id.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let compact = out.trim_matches('-');
    if compact.is_empty() {
        "device".to_string()
    } else {
        compact.to_string()
    }
}

fn path_with_suffix(path: &RelativePath, suffix: &str) -> RelativePath {
    let raw = path.as_str();
    let (dir, file_name) = match raw.rsplit_once('/') {
        Some((d, f)) => (Some(d), f),
        None => (None, raw),
    };
    let renamed = apply_suffix_to_file_name(file_name, suffix);
    match dir {
        Some(d) => RelativePath::from(format!("{d}/{renamed}")),
        None => RelativePath::from(renamed),
    }
}

fn conflict_resolution_actions(
    path: &RelativePath,
    remote_id: Option<String>,
    resolution: ConflictResolution,
) -> Result<Vec<SyncAction>, OxidriveError> {
    match resolution {
        ConflictResolution::LocalWins => Ok(vec![SyncAction::Upload {
            path: path.clone(),
            remote_id,
        }]),
        ConflictResolution::RemoteWins => {
            let remote_id = remote_id
                .ok_or_else(|| OxidriveError::sync("conflict remote_wins requires a remote id"))?;
            Ok(vec![SyncAction::Download {
                path: path.clone(),
                remote_id,
            }])
        }
        ConflictResolution::Rename { suffix } => {
            rename_like_actions(path, remote_id, suffix, "rename")
        }
        ConflictResolution::ConflictCopy { suffix } => {
            rename_like_actions(path, remote_id, suffix, "conflict_copy")
        }
    }
}

fn rename_like_actions(
    path: &RelativePath,
    remote_id: Option<String>,
    suffix: String,
    mode: &str,
) -> Result<Vec<SyncAction>, OxidriveError> {
    let remote_id = remote_id
        .ok_or_else(|| OxidriveError::sync(format!("conflict {mode} requires a remote id")))?;
    Ok(vec![
        SyncAction::Download {
            path: path_with_suffix(path, &suffix),
            remote_id: remote_id.clone(),
        },
        SyncAction::Upload {
            path: path.clone(),
            remote_id: Some(remote_id),
        },
    ])
}

fn apply_suffix_to_file_name(name: &str, suffix: &str) -> String {
    match name.rfind('.') {
        Some(dot) if dot > 0 => format!("{}{}{}", &name[..dot], suffix, &name[dot..]),
        _ => format!("{name}{suffix}"),
    }
}

fn converted_relative_path(path: &RelativePath, extension: &str) -> RelativePath {
    let raw = path.as_str();
    let (dir, file_name) = match raw.rsplit_once('/') {
        Some((d, f)) => (Some(d), f),
        None => (None, raw),
    };
    let stem = match file_name.rsplit_once('.') {
        Some((s, ext)) if !s.is_empty() && !ext.is_empty() => s,
        _ => file_name,
    };
    let renamed = format!("{stem}.{extension}");
    match dir {
        Some(d) => RelativePath::from(format!("{d}/{renamed}")),
        None => RelativePath::from(renamed),
    }
}

fn read_tombstone(
    redb: &RedbStore,
    path: &RelativePath,
) -> Result<Option<Tombstone>, OxidriveError> {
    let Some(raw) = redb.get_tombstone_sync(path.as_str())? else {
        return Ok(None);
    };
    let parsed = bincode::deserialize::<Tombstone>(&raw).map_err(|e| {
        OxidriveError::store(format!("decode tombstone for '{}': {e}", path.as_str()))
    })?;
    Ok(Some(parsed))
}

fn write_tombstone(
    redb: &RedbStore,
    path: &RelativePath,
    tombstone: Tombstone,
) -> Result<(), OxidriveError> {
    let payload = bincode::serialize(&tombstone)
        .map_err(|e| OxidriveError::store(format!("encode tombstone for '{}': {e}", path)))?;
    redb.set_tombstone_sync(path.as_str(), &payload)
}

pub fn clear_tombstone(redb: &RedbStore, path: &RelativePath) -> Result<(), OxidriveError> {
    redb.delete_tombstone_sync(path.as_str())
}

/// Counts repeated observations of a one-sided deletion (local-gone or
/// remote-gone) and only returns `true` once the confirmation threshold is met.
///
/// Used symmetrically for both delete directions so that a transient
/// disappearance (a brief `rm`, a half-synced checkout, an atomic-save rename
/// race) does not propagate a destructive deletion on the very first cycle.
fn confirm_delete_observation(
    redb: &RedbStore,
    path: &RelativePath,
    drive_file_id: Option<String>,
    device_id: &str,
) -> Result<bool, OxidriveError> {
    let now = Utc::now();
    let Some(mut tombstone) = read_tombstone(redb, path)? else {
        write_tombstone(
            redb,
            path,
            Tombstone {
                drive_file_id,
                deleted_at: now,
                by_device: device_id.to_string(),
                confirmations: 0,
            },
        )?;
        return Ok(false);
    };
    if tombstone.drive_file_id.is_none() {
        tombstone.drive_file_id = drive_file_id;
    }
    tombstone.by_device = device_id.to_string();
    tombstone.deleted_at = now;
    tombstone.confirmations = tombstone.confirmations.saturating_add(1);
    let confirmed = tombstone.confirmations >= DELETE_CONFIRMATIONS_REQUIRED;
    write_tombstone(redb, path, tombstone)?;
    Ok(confirmed)
}

fn skip_upload_for_active_foreign_lease(
    redb: &RedbStore,
    path: &RelativePath,
    remote: &DriveFile,
    device_id: &str,
) -> bool {
    let Some(mut lease) = parse_lease(&remote.app_properties) else {
        let _ = redb.delete_lease_sync(&remote.id);
        return false;
    };
    if lease.drive_file_id.is_empty() {
        lease.drive_file_id = remote.id.clone();
    }
    if lease_is_active(&lease, Utc::now()) && lease.owner_device != device_id {
        match bincode::serialize(&lease) {
            Ok(payload) => {
                if let Err(error) = redb.set_lease_sync(&lease.drive_file_id, &payload) {
                    tracing::warn!(
                        path = %path,
                        drive_file_id = %lease.drive_file_id,
                        error = %error,
                        "could not save an active file lease"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    path = %path,
                    drive_file_id = %lease.drive_file_id,
                    error = %error,
                    "could not encode an active file lease"
                );
            }
        }
        tracing::warn!(
            path = %path,
            owner = %lease.owner_device,
            "this file is being edited on another device"
        );
        return true;
    }
    let _ = redb.delete_lease_sync(&remote.id);
    false
}

async fn move_local_file_to_trash(
    root: &Path,
    rel_path: &RelativePath,
    source: &Path,
) -> Result<(), OxidriveError> {
    let trash_root = root.join(".trash");
    let mut target = trash_root.join(rel_path.as_str());
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .await
            .map_err(|e| OxidriveError::sync(format!("mkdir {}: {e}", parent.display())))?;
    }
    if fs::try_exists(&target)
        .await
        .map_err(|e| OxidriveError::sync(format!("exists {}: {e}", target.display())))?
    {
        target = unique_trash_collision_path(&target).await?;
    }
    fs::rename(source, &target).await.map_err(|e| {
        OxidriveError::sync(format!(
            "move '{}' to trash '{}': {e}",
            source.display(),
            target.display()
        ))
    })?;
    Ok(())
}

async fn unique_trash_collision_path(initial: &Path) -> Result<PathBuf, OxidriveError> {
    let stamp = Utc::now().format("%Y%m%d%H%M%S").to_string();
    for attempt in 0_u32..1024 {
        let candidate = with_collision_suffix(initial, &stamp, attempt);
        if !fs::try_exists(&candidate)
            .await
            .map_err(|e| OxidriveError::sync(format!("exists {}: {e}", candidate.display())))?
        {
            return Ok(candidate);
        }
    }
    Err(OxidriveError::sync(format!(
        "unable to allocate unique trash path for '{}'",
        initial.display()
    )))
}

fn with_collision_suffix(path: &Path, stamp: &str, attempt: u32) -> PathBuf {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let numbered = if attempt == 0 {
        stamp.to_string()
    } else {
        format!("{stamp}-{attempt}")
    };
    let renamed = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!("{stem}.{numbered}.{ext}")
        }
        _ => format!("{name}.{numbered}"),
    };
    path.with_file_name(renamed)
}

/// Removes files from `.trash/` older than `ttl_days`, returning the number of purged files.
pub fn purge_trash(
    trash_dir: &Path,
    ttl_days: u64,
    now: DateTime<Utc>,
) -> Result<usize, OxidriveError> {
    if !trash_dir.exists() {
        return Ok(0);
    }
    let ttl_days_i64 = i64::try_from(ttl_days).unwrap_or(i64::MAX);
    let ttl = chrono::Duration::days(ttl_days_i64);
    let mut removed = 0usize;
    purge_trash_dir_recursive(trash_dir, ttl, now, &mut removed)?;
    Ok(removed)
}

fn purge_trash_dir_recursive(
    dir: &Path,
    ttl: chrono::Duration,
    now: DateTime<Utc>,
    removed: &mut usize,
) -> Result<(), OxidriveError> {
    for entry in std::fs::read_dir(dir)
        .map_err(|e| OxidriveError::sync(format!("read_dir {}: {e}", dir.display())))?
    {
        let entry = entry
            .map_err(|e| OxidriveError::sync(format!("read_dir entry {}: {e}", dir.display())))?;
        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|e| OxidriveError::sync(format!("metadata {}: {e}", path.display())))?;
        if metadata.is_dir() {
            purge_trash_dir_recursive(&path, ttl, now, removed)?;
            if std::fs::read_dir(&path)
                .map_err(|e| OxidriveError::sync(format!("read_dir {}: {e}", path.display())))?
                .next()
                .is_none()
            {
                std::fs::remove_dir(&path)
                    .map_err(|e| OxidriveError::sync(format!("rmdir {}: {e}", path.display())))?;
            }
            continue;
        }
        let modified = metadata
            .modified()
            .map_err(|e| OxidriveError::sync(format!("mtime {}: {e}", path.display())))?;
        let modified_at = DateTime::<Utc>::from(modified);
        if (now - modified_at) >= ttl {
            std::fs::remove_file(&path)
                .map_err(|e| OxidriveError::sync(format!("remove {}: {e}", path.display())))?;
            *removed = removed.saturating_add(1);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::drive::DriveClient;
    use crate::store::{RedbStore, Store};
    use crate::types::{
        ConflictResolution, RelativePath, SyncAction, SyncRecord, UploadSession, UploadSessionMode,
        WorkspaceConversion,
    };
    use chrono::Utc;
    use tempfile::tempdir;

    use super::{conflict_resolution_actions, path_with_suffix, SyncExecutor};

    #[test]
    fn rename_suffix_inserted_before_extension() {
        let renamed = path_with_suffix(
            &RelativePath::from("docs/report.txt"),
            " (conflict 2026-04-10)",
        );
        assert_eq!(renamed.as_str(), "docs/report (conflict 2026-04-10).txt");
    }

    #[test]
    fn rename_suffix_appended_when_no_extension() {
        let renamed = path_with_suffix(&RelativePath::from("README"), ".conflict");
        assert_eq!(renamed.as_str(), "README.conflict");
    }

    #[test]
    fn rename_conflict_resolution_variant_is_supported() {
        let path = RelativePath::from("notes/todo.md");
        let actions = conflict_resolution_actions(
            &path,
            Some("remote-42".to_string()),
            ConflictResolution::Rename {
                suffix: " (conflict)".to_string(),
            },
        )
        .unwrap();

        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            SyncAction::Download { path, remote_id }
                if path.as_str() == "notes/todo (conflict).md" && remote_id == "remote-42"
        ));
        assert!(matches!(
            &actions[1],
            SyncAction::Upload { path, remote_id: Some(remote_id) }
                if path.as_str() == "notes/todo.md" && remote_id == "remote-42"
        ));
    }

    #[test]
    fn conflict_copy_resolution_variant_is_supported() {
        let path = RelativePath::from("notes/todo.md");
        let actions = conflict_resolution_actions(
            &path,
            Some("remote-42".to_string()),
            ConflictResolution::ConflictCopy {
                suffix: ".conflict.20260101112233".to_string(),
            },
        )
        .unwrap();
        assert_eq!(actions.len(), 2);
        assert!(matches!(
            &actions[0],
            SyncAction::Download { path, remote_id }
                if path.as_str() == "notes/todo.conflict.20260101112233.md" && remote_id == "remote-42"
        ));
        assert!(matches!(
            &actions[1],
            SyncAction::Upload { path, remote_id: Some(remote_id) }
                if path.as_str() == "notes/todo.md" && remote_id == "remote-42"
        ));
    }

    #[test]
    fn remote_wins_requires_remote_id() {
        let path = RelativePath::from("notes/todo.md");
        let err = conflict_resolution_actions(&path, None, ConflictResolution::RemoteWins)
            .unwrap_err()
            .to_string();
        assert!(err.contains("remote_wins requires a remote id"));
    }

    #[tokio::test]
    async fn cleanup_metadata_also_removes_conversion_state() {
        let dir = tempdir().expect("tempdir");
        let store = Store::open(dir.path()).expect("open store");
        store
            .set_root_drive_folder_id(Some("root-folder".to_string()))
            .expect("set root id");
        let path = RelativePath::from("docs/report.docx");
        let now = Utc::now();
        store
            .upsert(
                path.clone(),
                SyncRecord {
                    drive_file_id: Some("doc-1".to_string()),
                    remote_md5: Some("abcd".to_string()),
                    remote_mime_type: Some("application/vnd.google-apps.document".to_string()),
                    remote_modified_at: Some(now),
                    local_md5: "efgh".to_string(),
                    local_mtime: now,
                    local_size: 42,
                    last_synced_at: now,
                    remote_head_revision_id: None,
                    remote_version: None,
                    version_vector: std::collections::BTreeMap::new(),
                },
            )
            .expect("seed record");
        store
            .upsert_conversion(
                path.clone(),
                WorkspaceConversion {
                    drive_file_id: "doc-1".to_string(),
                    google_mime: "application/vnd.google-apps.document".to_string(),
                    last_export_md5: Some("abcd".to_string()),
                },
            )
            .expect("seed conversion");
        store
            .upsert_upload_session(
                path.clone(),
                UploadSession {
                    mode: UploadSessionMode::Convert {
                        drive_id: "doc-1".to_string(),
                        google_mime: "application/vnd.google-apps.document".to_string(),
                    },
                    session_url: "https://upload.example/session".to_string(),
                    next_offset: 1,
                    file_size: 2,
                    local_md5: "efgh".to_string(),
                    updated_at: now,
                },
            )
            .expect("seed upload session");

        let executor = SyncExecutor::new(1, 1, 0, "test-device".to_string(), true, false);
        let client = DriveClient::new("token".to_string());
        let redb_file = tempfile::NamedTempFile::new().expect("tempfile");
        let redb = RedbStore::open(redb_file.path()).expect("open redb");
        let report = executor
            .execute(
                vec![SyncAction::CleanupMetadata { path: path.clone() }],
                &client,
                &store,
                &redb,
            )
            .await
            .expect("execute cleanup");

        assert!(report.errors.is_empty());
        assert_eq!(store.get(&path).expect("get record"), None);
        assert_eq!(store.get_conversion(&path).expect("get conversion"), None);
        assert_eq!(
            store.get_upload_session(&path).expect("get upload session"),
            None
        );
    }
}
