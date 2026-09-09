//! Debounced local filesystem notifications relative to a sync root.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use notify::event::{EventKind, ModifyKind};
use notify::RecursiveMode;
use notify_debouncer_full::{
    new_debouncer, DebounceEventResult, DebouncedEvent, Debouncer, RecommendedCache,
};
use tokio::sync::mpsc;

use crate::error::OxidriveError;
use crate::types::RelativePath;

/// Normalized change notification emitted after debouncing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// A new file appeared.
    Created(RelativePath),
    /// File contents or metadata changed.
    Modified(RelativePath),
    /// A path was deleted.
    Deleted(RelativePath),
    /// A path was renamed within the watched tree.
    Renamed {
        /// Previous relative path.
        from: RelativePath,
        /// New relative path.
        to: RelativePath,
    },
}

/// Watches `root` recursively with `debounce_ms` quiet period.
pub struct LocalWatcher {
    root: PathBuf,
    debounce_ms: u64,
}

impl LocalWatcher {
    /// Builds a watcher for `root` (must exist).
    pub fn new(root: PathBuf, debounce_ms: u64) -> Result<Self, OxidriveError> {
        warn_inotify_limits();
        if !root.is_dir() {
            return Err(OxidriveError::sync(format!(
                "watch root is not a directory: {}",
                root.display()
            )));
        }
        Ok(Self { root, debounce_ms })
    }

    /// Starts watching and returns a stream of [`WatchEvent`] values.
    ///
    /// A background thread runs the debouncer; the returned channel is closed if that thread stops.
    pub async fn watch(&mut self) -> Result<mpsc::Receiver<WatchEvent>, OxidriveError> {
        let (tx, rx) = mpsc::channel(256);
        let root = self.root.clone();
        let debounce = Duration::from_millis(self.debounce_ms.max(1));

        std::thread::Builder::new()
            .name("oxidrive-notify".into())
            .spawn(move || {
                if let Some((mut _debouncer, std_rx)) = try_native_watcher(&root, debounce) {
                    while let Ok(res) = std_rx.recv() {
                        let events = match res {
                            Ok(ev) => ev,
                            Err(errs) => {
                                for e in errs {
                                    tracing::warn!("file watcher error: {e}");
                                }
                                continue;
                            }
                        };
                        for ev in events {
                            if let Some(mapped) = map_event(&root, &ev) {
                                if tx.blocking_send(mapped).is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    return;
                }

                tracing::warn!(
                    interval_ms = debounce.as_millis() as u64,
                    "falling back to polling the folder for changes"
                );
                run_polling_watcher(&root, debounce, &tx);
            })
            .map_err(|e| OxidriveError::sync(format!("spawn notify thread: {e}")))?;

        Ok(rx)
    }
}

fn warn_inotify_limits() {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/sys/fs/inotify/max_user_watches") {
            if let Ok(n) = s.trim().parse::<u64>() {
                if n < 50_000 {
                    tracing::warn!(
                        max_user_watches = n,
                        "inotify max_user_watches is low; large folders may miss changes"
                    );
                }
            }
        }
    }
}

type NativeDebouncer = Debouncer<notify::RecommendedWatcher, RecommendedCache>;

fn try_native_watcher(
    root: &Path,
    debounce: Duration,
) -> Option<(
    NativeDebouncer,
    std::sync::mpsc::Receiver<DebounceEventResult>,
)> {
    let (std_tx, std_rx) = std::sync::mpsc::channel::<DebounceEventResult>();
    let mut debouncer = match new_debouncer(debounce, None, move |res| {
        let _ = std_tx.send(res);
    }) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("could not set up the file-watcher debouncer: {e}");
            return None;
        }
    };

    if let Err(e) = debouncer.watch(root, RecursiveMode::Recursive) {
        tracing::warn!("could not start the file watcher: {e}");
        return None;
    }

    Some((debouncer, std_rx))
}

fn run_polling_watcher(root: &Path, interval: Duration, tx: &mpsc::Sender<WatchEvent>) {
    let mut known: HashMap<PathBuf, SystemTime> = HashMap::new();
    scan_directory(root, &mut known);

    loop {
        std::thread::sleep(interval);

        let mut current: HashMap<PathBuf, SystemTime> = HashMap::new();
        scan_directory(root, &mut current);

        for (path, mtime) in &current {
            match known.get(path) {
                None => {
                    if let Some(ev) = to_relative(root, path).map(WatchEvent::Created) {
                        if tx.blocking_send(ev).is_err() {
                            return;
                        }
                    }
                }
                Some(old_mtime) if old_mtime != mtime => {
                    if let Some(ev) = to_relative(root, path).map(WatchEvent::Modified) {
                        if tx.blocking_send(ev).is_err() {
                            return;
                        }
                    }
                }
                _ => {}
            }
        }

        for path in known.keys() {
            if !current.contains_key(path) {
                if let Some(ev) = to_relative(root, path).map(WatchEvent::Deleted) {
                    if tx.blocking_send(ev).is_err() {
                        return;
                    }
                }
            }
        }

        known = current;
    }
}

fn scan_directory(root: &Path, map: &mut HashMap<PathBuf, SystemTime>) {
    let walker = match std::fs::read_dir(root) {
        Ok(rd) => rd,
        Err(_) => return,
    };

    let mut stack = vec![walker];
    while let Some(rd) = stack.last_mut() {
        match rd.next() {
            Some(Ok(entry)) => {
                let path = entry.path();
                if let Ok(ft) = entry.file_type() {
                    if ft.is_dir() {
                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                            if matches!(name, ".index" | ".oxidrive" | ".trash") {
                                continue;
                            }
                        }
                        if let Ok(sub) = std::fs::read_dir(&path) {
                            stack.push(sub);
                        }
                    } else if let Ok(meta) = entry.metadata() {
                        if let Ok(mtime) = meta.modified() {
                            map.insert(path, mtime);
                        }
                    }
                }
            }
            Some(Err(_)) => continue,
            None => {
                stack.pop();
            }
        }
    }
}

fn map_event(root: &Path, ev: &DebouncedEvent) -> Option<WatchEvent> {
    match ev.kind {
        EventKind::Create(_) => {
            let p = ev.paths.last()?;
            Some(WatchEvent::Created(to_relative(root, p)?))
        }
        EventKind::Modify(ModifyKind::Name(_)) if ev.paths.len() >= 2 => {
            let from = to_relative(root, &ev.paths[0])?;
            let to = to_relative(root, &ev.paths[1])?;
            Some(WatchEvent::Renamed { from, to })
        }
        EventKind::Modify(_) => {
            let p = ev.paths.last()?;
            Some(WatchEvent::Modified(to_relative(root, p)?))
        }
        EventKind::Remove(_) => {
            let p = ev.paths.last()?;
            Some(WatchEvent::Deleted(to_relative(root, p)?))
        }
        _ => None,
    }
}

fn to_relative(root: &Path, path: &Path) -> Option<RelativePath> {
    let rel = path.strip_prefix(root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    let trimmed = s.trim_start_matches('/');
    if trimmed.is_empty()
        || trimmed.starts_with(".index/")
        || trimmed == ".index"
        || trimmed.starts_with(".oxidrive/")
        || trimmed == ".oxidrive"
        || trimmed.starts_with(".trash/")
        || trimmed == ".trash"
        || trimmed.ends_with(".part")
    {
        None
    } else {
        let rel = RelativePath::from(trimmed);
        if rel.is_safe_non_empty() {
            Some(rel)
        } else {
            tracing::warn!(path = trimmed, "skipping unsafe local watcher path");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::to_relative;
    use tempfile::tempdir;

    #[test]
    fn internal_paths_are_filtered_from_events() {
        let root = tempdir().expect("tempdir");
        assert!(to_relative(root.path(), &root.path().join(".index/a.md")).is_none());
        assert!(to_relative(root.path(), &root.path().join(".oxidrive/state.redb")).is_none());
        assert!(to_relative(root.path(), &root.path().join(".trash/deleted.txt")).is_none());
        assert!(to_relative(root.path(), &root.path().join("file.bin.part")).is_none());
    }

    #[test]
    fn regular_paths_are_kept() {
        let root = tempdir().expect("tempdir");
        let rel = to_relative(root.path(), &root.path().join("docs/readme.md"))
            .expect("expected a relative path");
        assert_eq!(rel.as_str(), "docs/readme.md");
    }
}
