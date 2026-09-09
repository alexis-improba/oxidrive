//! Application configuration loaded from TOML (preferred) or JSON.
//!
//! Missing fields use [`Config::default`] via serde defaults for a workable baseline.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{OxidriveError, Result};

fn default_sync_interval_secs() -> u64 {
    300
}

fn default_max_concurrent_uploads() -> usize {
    4
}

fn default_max_concurrent_downloads() -> usize {
    4
}

fn default_ignore_patterns() -> Vec<String> {
    vec![
        ".oxidrive/**".to_string(),
        "*.part".to_string(),
        "~$*".to_string(),
        ".~lock.*#".to_string(),
        "*.tmp".to_string(),
        "*.temp".to_string(),
        "*.swp".to_string(),
        "*.swo".to_string(),
        "*~".to_string(),
        "4913".to_string(),
        ".DS_Store".to_string(),
        "Thumbs.db".to_string(),
        "*.crdownload".to_string(),
        "*.partial".to_string(),
    ]
}

fn default_log_level() -> String {
    "warn".to_string()
}

fn default_debounce_ms() -> u64 {
    2000
}

fn default_true() -> bool {
    true
}

fn default_stability_ms() -> u64 {
    1500
}

fn default_trash_ttl_days() -> u64 {
    30
}

fn default_token_path() -> PathBuf {
    PathBuf::from("token.json")
}

/// Policy applied when the same file changes on both sides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Keep both sides by creating a conflict copy.
    #[default]
    ConflictCopy,
    /// Prefer the local file when reconciling.
    LocalWins,
    /// Prefer the remote file when reconciling.
    RemoteWins,
    /// Keep both by renaming one side using `suffix`.
    Rename {
        /// Suffix inserted before the file extension (e.g. `"_remote"`).
        suffix: String,
    },
}

/// Runtime settings for sync, I/O, logging, and optional indexing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Config {
    /// Google OAuth client id.
    #[serde(default)]
    pub client_id: String,
    /// Google OAuth client secret.
    #[serde(default)]
    pub client_secret: String,
    /// Path where OAuth token JSON is stored.
    #[serde(default = "default_token_path")]
    pub token_path: PathBuf,
    /// Root directory mirrored with Google Drive.
    pub sync_dir: PathBuf,
    /// Optional stable identity for this device.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Optional Drive folder id to scope the sync.
    pub drive_folder_id: Option<String>,
    /// Interval between automatic syncs when using the service (seconds).
    #[serde(default = "default_sync_interval_secs")]
    pub sync_interval_secs: u64,
    /// How to resolve edit/edit conflicts.
    #[serde(default)]
    pub conflict_policy: ConflictPolicy,
    /// Maximum parallel uploads.
    #[serde(default = "default_max_concurrent_uploads")]
    pub max_concurrent_uploads: usize,
    /// Maximum parallel downloads.
    #[serde(default = "default_max_concurrent_downloads")]
    pub max_concurrent_downloads: usize,
    /// Glob ignore rules: literal names, `*`/`?` wildcards (per path segment),
    /// and `prefix/**` directory trees. See [`crate::sync::scan`] for matching.
    #[serde(default = "default_ignore_patterns")]
    pub ignore_patterns: Vec<String>,
    /// Optional directory for Markdown / search index artifacts.
    pub index_dir: Option<PathBuf>,
    /// Console filter when no `--verbose` / `--quiet` and `RUST_LOG` is unset (`warn`).
    /// Values `info` / `debug` / `trace` do not raise the default console; use `--verbose`.
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Optional JSON log file path (daily rotation under the parent directory).
    #[serde(default)]
    pub log_file: Option<PathBuf>,
    /// Debounce window for filesystem watchers (milliseconds).
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    /// Whether local deletes must go through safer confirmation flow.
    #[serde(default = "default_true")]
    pub safe_delete: bool,
    /// Minimum file age before upload decisions consider a local file stable.
    #[serde(default = "default_stability_ms")]
    pub stability_ms: u64,
    /// Retention in days for files moved under `.trash/` before permanent purge.
    #[serde(default = "default_trash_ttl_days")]
    pub trash_ttl_days: u64,
    /// Enables advisory lease support.
    #[serde(default)]
    pub use_leases: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: String::new(),
            token_path: default_token_path(),
            sync_dir: PathBuf::from("."),
            device_id: None,
            drive_folder_id: None,
            sync_interval_secs: default_sync_interval_secs(),
            conflict_policy: ConflictPolicy::default(),
            max_concurrent_uploads: default_max_concurrent_uploads(),
            max_concurrent_downloads: default_max_concurrent_downloads(),
            ignore_patterns: default_ignore_patterns(),
            index_dir: None,
            log_level: default_log_level(),
            log_file: None,
            debounce_ms: default_debounce_ms(),
            safe_delete: default_true(),
            stability_ms: default_stability_ms(),
            trash_ttl_days: default_trash_ttl_days(),
            use_leases: false,
        }
    }
}

impl Config {
    /// Returns ignore patterns with mandatory internal exclusions added.
    #[must_use]
    pub fn effective_ignore_patterns(&self) -> Vec<String> {
        let mut patterns = self.ignore_patterns.clone();
        ensure_pattern(&mut patterns, ".oxidrive/**");
        ensure_pattern(&mut patterns, ".index/**");
        ensure_pattern(&mut patterns, ".trash/**");
        ensure_pattern(&mut patterns, "*.part");
        ensure_pattern(&mut patterns, "~$*");
        ensure_pattern(&mut patterns, ".~lock.*#");
        ensure_pattern(&mut patterns, "*.tmp");
        ensure_pattern(&mut patterns, "*.temp");
        ensure_pattern(&mut patterns, "*.swp");
        ensure_pattern(&mut patterns, "*.swo");
        ensure_pattern(&mut patterns, "*~");
        ensure_pattern(&mut patterns, "4913");
        ensure_pattern(&mut patterns, ".DS_Store");
        ensure_pattern(&mut patterns, "Thumbs.db");
        ensure_pattern(&mut patterns, "*.crdownload");
        ensure_pattern(&mut patterns, "*.partial");
        if let Some(rel_token) = self.token_path_relative_to_sync_dir() {
            ensure_pattern(&mut patterns, &rel_token);
        }
        patterns
    }

    fn token_path_relative_to_sync_dir(&self) -> Option<String> {
        let cwd = std::env::current_dir().ok()?;
        let sync_abs = if self.sync_dir.is_absolute() {
            self.sync_dir.clone()
        } else {
            cwd.join(&self.sync_dir)
        };
        let token_abs = if self.token_path.is_absolute() {
            self.token_path.clone()
        } else {
            cwd.join(&self.token_path)
        };
        let rel = token_abs.strip_prefix(&sync_abs).ok()?;
        let normalized = rel.to_string_lossy().replace('\\', "/");
        let trimmed = normalized.trim_matches('/');
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    }

    /// Load configuration from disk.
    ///
    /// Resolution order:
    /// 1. Explicit `path` when provided and the file exists.
    /// 2. `./config.toml` in the current working directory.
    /// 3. `./config.json` in the current working directory.
    ///
    /// The file is parsed as TOML first; if that fails, JSON is attempted.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let candidates: Vec<PathBuf> = if let Some(p) = path {
            vec![p.to_path_buf()]
        } else {
            let cwd = std::env::current_dir().map_err(OxidriveError::Io)?;
            vec![cwd.join("config.toml"), cwd.join("config.json")]
        };

        for candidate in candidates {
            if !candidate.is_file() {
                continue;
            }
            return Self::parse_file(&candidate).map_err(|e| {
                OxidriveError::config(format!("failed to parse {}: {e}", candidate.display()))
            });
        }

        if let Some(p) = path {
            if !p.exists() {
                return Err(OxidriveError::config(format!(
                    "configuration file not found: {}",
                    p.display()
                )));
            }
            if !p.is_file() {
                return Err(OxidriveError::config(format!(
                    "configuration path is not a file: {}",
                    p.display()
                )));
            }
        }

        Err(OxidriveError::config(
            "no configuration file found (tried explicit path, ./config.toml, ./config.json)",
        ))
    }

    fn parse_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path).map_err(OxidriveError::Io)?;
        Self::parse_str(&text)
    }

    fn parse_str(text: &str) -> Result<Self> {
        let trimmed = text.trim_start();
        if trimmed.starts_with('{') {
            return serde_json::from_str(text)
                .map_err(|e| OxidriveError::config(format!("invalid JSON configuration: {e}")));
        }
        match toml::from_str::<Self>(text) {
            Ok(cfg) => Ok(cfg),
            Err(toml_err) => serde_json::from_str(text).map_err(|json_err| {
                OxidriveError::config(format!(
                    "invalid configuration (TOML: {toml_err}; JSON: {json_err})"
                ))
            }),
        }
    }
}

fn ensure_pattern(patterns: &mut Vec<String>, pattern: &str) {
    if patterns.iter().all(|p| p != pattern) {
        patterns.push(pattern.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn load_valid_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.toml");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"
sync_dir = "/tmp/sync"
sync_interval_secs = 120
conflict_policy = "local_wins"
max_concurrent_uploads = 2
ignore_patterns = ["*.tmp"]
log_level = "debug"
"#
        )
        .unwrap();
        drop(f);

        let cfg = Config::load(Some(path.as_path())).unwrap();
        assert_eq!(cfg.sync_dir, PathBuf::from("/tmp/sync"));
        assert_eq!(cfg.sync_interval_secs, 120);
        assert_eq!(cfg.conflict_policy, ConflictPolicy::LocalWins);
        assert_eq!(cfg.max_concurrent_uploads, 2);
        assert_eq!(cfg.ignore_patterns, vec!["*.tmp".to_string()]);
        assert_eq!(cfg.log_level, "debug");
    }

    #[test]
    fn load_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cfg.json");
        fs::write(
            &path,
            r#"{
                "sync_dir": "/data/gdrive",
                "drive_folder_id": "abc123",
                "conflict_policy": { "rename": { "suffix": "_remote" } }
            }"#,
        )
        .unwrap();

        let cfg = Config::load(Some(path.as_path())).unwrap();
        assert_eq!(cfg.sync_dir, PathBuf::from("/data/gdrive"));
        assert_eq!(cfg.drive_folder_id.as_deref(), Some("abc123"));
        assert_eq!(
            cfg.conflict_policy,
            ConflictPolicy::Rename {
                suffix: "_remote".into()
            }
        );
    }

    #[test]
    fn reject_invalid_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        fs::write(&path, "not_valid_toml {{{").unwrap();

        let err = Config::load(Some(path.as_path())).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse") || msg.contains("invalid"),
            "unexpected message: {msg}"
        );
    }

    #[test]
    fn effective_ignore_patterns_include_internal_defaults() {
        let cfg = Config {
            sync_dir: PathBuf::from("/tmp/oxidrive-sync"),
            ..Config::default()
        };
        let patterns = cfg.effective_ignore_patterns();
        assert!(patterns.contains(&".oxidrive/**".to_string()));
        assert!(patterns.contains(&".index/**".to_string()));
        assert!(patterns.contains(&".trash/**".to_string()));
        assert!(patterns.contains(&"*.part".to_string()));
        assert!(patterns.contains(&"~$*".to_string()));
        assert!(patterns.contains(&".~lock.*#".to_string()));
        assert!(patterns.contains(&"*.tmp".to_string()));
        assert!(patterns.contains(&"*.temp".to_string()));
        assert!(patterns.contains(&"*.swp".to_string()));
        assert!(patterns.contains(&"*.swo".to_string()));
        assert!(patterns.contains(&"*~".to_string()));
        assert!(patterns.contains(&"4913".to_string()));
        assert!(patterns.contains(&".DS_Store".to_string()));
        assert!(patterns.contains(&"Thumbs.db".to_string()));
        assert!(patterns.contains(&"*.crdownload".to_string()));
        assert!(patterns.contains(&"*.partial".to_string()));
    }

    #[test]
    fn effective_ignore_patterns_add_token_path_when_under_sync_root() {
        let cfg = Config {
            sync_dir: PathBuf::from("/tmp/oxidrive-sync"),
            token_path: PathBuf::from("/tmp/oxidrive-sync/.oxidrive/token.json"),
            ..Config::default()
        };
        let patterns = cfg.effective_ignore_patterns();
        assert!(patterns.contains(&".oxidrive/token.json".to_string()));
    }

    #[test]
    fn default_config_uses_quiet_log_level() {
        assert_eq!(Config::default().log_level, "warn");
    }

    #[test]
    fn default_config_uses_30_day_trash_ttl() {
        let cfg = Config::default();
        assert_eq!(cfg.trash_ttl_days, 30);
    }
}
