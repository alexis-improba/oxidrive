# Architecture overview

This document describes the software organization of **oxidrive**: modules, data flows between them, and main technical choices.

---

## ASCII diagram (modules and interactions)

```
                    ┌─────────────────────────────────────────┐
                    │              CLI (main / cli)            │
                    │  setup │ sync │ status │ service        │
                    └───────────────┬─────────────────────────┘
                                    │
                    ┌───────────────┼───────────────┐
                    ▼               ▼               ▼
             ┌──────────┐   ┌──────────┐   ┌──────────────┐
             │  config  │   │   auth   │   │    error     │
             └──────────┘   └──────────┘   └──────────────┘
                                    │
        ┌───────────────────────────┼───────────────────────────┐
        ▼                           ▼                           ▼
 ┌─────────────┐            ┌─────────────┐              ┌─────────────┐
 │   watch     │───events──▶│    sync     │◀───scan────▶│   store     │
 │  (notify)   │            │ scan/decision│              │   (redb)    │
 └─────────────┘            │  /executor   │              └──────▲──────┘
        │                   └──────┬───────┘                     │
        │                          │ read/write metadata         │
        │                          ▼                             │
        │                   ┌─────────────┐                     │
        └──────────────────▶│   drive     │─────────────────────┘
                            │ client API  │    (persist ids, hashes)
                            └──────┬──────┘
                                   │ HTTPS (reqwest + rustls)
                                   ▼
                            ┌─────────────┐
                            │ Google Drive│
                            └─────────────┘

 ┌─────────────┐
 │   index     │◀── Markdown / exported files (read from sync_dir)
 │ (generator) │──▶ artifacts in index_dir (optional)
 └─────────────┘

 ┌─────────────┐
 │   utils     │── hash, fs, retry (used by sync, drive, store)
 └─────────────┘
```

---

## Module descriptions

### `drive/`

**Google Drive API** layer: request authentication, file listing, download, upload, **changes** handling (incremental). Includes optimistic **revision-guarded** uploads (`headRevisionId`/`version` preflight), `appProperties` updates (version vectors), and best-effort advisory **leases** (`locks`, opt-in via `use_leases`). Remote types (`DriveFile`, MIME, parents, revisions, `appProperties`) are isolated here to limit how much of the rest of the code depends on API details.

### `sync/`

Core of **reconciliation**:

- **scan**: local inventory (relative paths, sizes, MD5), ignore-rule matching, plus stability and open/lock-file detection (`is_stable`, `has_open_lock`) so files still being written or open in an application are not synced mid-change.
- **decision**: pure function `(local, remote, persisted metadata) → action` (upload, download, skip, touch-metadata, conflict, deletions, cleanup), MD5-based with version-vector causality for conflicts.
- **executor**: orchestration of concrete operations (`drive` calls, `store` updates), optimistic revision guards, non-destructive conflict copies, safe deletions (`.trash/`, tombstones, cross-cycle confirmation), and version-vector maintenance.
- **engine**: cycle orchestration, incremental vs full remote view, and crash recovery over the pending-operations journal.
- **coordination**: **version vectors** (`ox_vv` / `ox_origin` in Drive `appProperties`) for distributed, server-less causal conflict detection, bounded to Drive's 124-byte property limit.
- **observability**: JSONL conflict log (`.oxidrive/conflicts.log`).

Conflict resolution applies the configured `ConflictPolicy` (default `conflict_copy`) when the decision yields a conflict.

### `watch/`

**Watching the sync folder** via the `notify` library (with configurable debounce). Events trigger sync cycles or queue work on the async runtime (`tokio`).

### `store/`

**Local persistence** of sync state (seen files, last local/remote MD5/mtime, Drive identifiers, resumable upload session cursors). Implemented with **redb** (embedded key-value store, single file). Blocking access is typically isolated in `spawn_blocking` so the async runtime is not blocked.

### `index/`

**Markdown indexing** (and possibly search) over files in `sync_dir` or produced by Workspace export. Writes to `index_dir` when configured.

### `utils/`

Cross-cutting helpers: **hashing** (MD5 for exportable binaries), **filesystem** helpers, **retry** with backoff for fragile network calls.

### Cross-cutting modules

- **`config`**: TOML/JSON deserialization, defaults.
- **`auth`**: Google OAuth2 flow (setup, token refresh).
- **`types`**: shared types (`SyncAction`, `SyncRecord`, relative paths, etc.).
- **`error`**: typed errors (`thiserror`), mapping to CLI exit codes.
- **`main::status`**: read-only diagnostics combining `config` + `store` snapshots (last sync, page token, conversion count, resumable session progress, pending recovery operations, service hints) plus multi-device state: `device_id`, active conflict policy, recent conflicts (from `conflicts.log`), pending deletion tombstones, and active leases.

---

## Data flow: one sync cycle

1. **Load**: read `Config`, open `store` database, authenticated `drive` client.
2. **Scan**: enumerate local files (excluding `ignore_patterns`) and fetch Drive tree / metadata under the configured `drive_folder_id` (required).
3. **Join**: for each relative path known on at least one side, associate with the latest `SyncRecord` in the database.
4. **Decision**: `determine_action` (or `determine_action_converted` for Workspace files) yields a `SyncAction` (skip, upload, download, touch-metadata, conflict, delete local/remote, cleanup metadata), using MD5 content comparison and version-vector causality.
5. **Conflict resolution**: if action is `Conflict`, apply `ConflictPolicy` (default `conflict_copy`: keep both sides non-destructively).
6. **Execution**: transfers and deletions; revision-guarded uploads degrade to conflict copies on mismatch; deletions are confirmed across cycles and routed through `.trash/`; version vectors and (optional) leases are updated in `appProperties`. Each operation is journaled for crash-safe recovery.
7. **Persistence**: write new `SyncRecord` entries / remove stale entries in `store`; clear resolved tombstones and the pending-operations journal.
8. **Index** (optional): if enabled, incremental index update after changes settle.

For multi-device / shared-folder behavior and operational guidance, see [multi-user.md](multi-user.md).

This pipeline can be triggered manually (`sync`), on a timer (`service`), or by the **watcher** after debounce.

---

## Technical choices

| Domain | Choice | Rationale |
|--------|--------|-----------|
| Async / concurrency | **Tokio** | Mature runtime for network I/O and parallel tasks (uploads/downloads). |
| HTTP / TLS | **reqwest** + **rustls** | HTTP client without system OpenSSL; TLS in pure Rust, suited to static binaries. |
| Local storage | **redb** | Embedded, transactional, single-file database; no external server. |
| CLI | **clap** | Subcommands, global flags, consistent help messages. |
| Config | **serde** + **toml** | Human-readable format, evolvable schema. |
| Errors | **thiserror** | Typed errors and stable messages for the CLI. |
| Logging | **tracing** + **tracing-subscriber** | Quiet console by default (Git-style `warning:` / `error:`); optional JSON file. |
| FS watch | **notify** + **notify-debouncer-full** | Event aggregation to avoid sync storms. |
| OAuth2 | **oauth2** | Standard flow for the Google API. |

For details on decision logic, see [decision-tree.md](decision-tree.md).
