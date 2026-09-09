# Troubleshooting

Common errors and how to fix them.

## Error: inotify max_user_watches

On Linux, file watching relies on inotify. If the kernel limit is too low, you may see an error related to `max_user_watches`.

**Immediate fix** (until the next reboot):

```bash
echo 524288 | sudo tee /proc/sys/fs/inotify/max_user_watches
```

**Permanent fix**: add a sysctl entry, for example in `/etc/sysctl.d/99-inotify.conf`:

```bash
fs.inotify.max_user_watches=524288
```

Then apply:

```bash
sudo sysctl --system
```

## OAuth2 error: expired token

OAuth2 access tokens expire. If refresh fails or the session is invalid:

1. Run the authentication flow again: `oxidrive setup`.
2. Check permissions on `token.json` (read/write for the user running oxidrive).

## Sync stuck / no files transferred

1. Confirm that `drive_folder_id` in the config points to the intended Drive folder.
2. Run with readable extra logs: `oxidrive sync --once --verbose` to see sync steps. Repeat `--verbose` if you need more detail (same format).
3. Check network connectivity (firewall, proxy, DNS).

## Pending operations remain in status

If `oxidrive status` shows non-zero **Pending ops**, the previous run likely stopped during a multi-step sync action (upload/download/delete reconciliation).

1. Run `oxidrive sync --once` to trigger recovery and flush pending entries.
2. Re-run `oxidrive status` and confirm `Pending ops: 0`.
3. If entries persist, run `oxidrive sync --once --verbose` and read the recovery warnings.

## Conflicts and conflict copies

oxidrive uses a configurable **conflict policy** (default **`conflict_copy`**). With the default, an edit/edit conflict is **non-destructive**: your local edit keeps the canonical name and the diverging remote content is written next to it as `<name>.conflict.<device>.<ts>.<ext>`. Both versions then propagate to every device.

- Review both files (timestamps, content), merge manually, and delete the obsolete conflict copy (the deletion propagates too).
- Every conflict resolution is recorded in `.oxidrive/conflicts.log` (JSONL). Inspect it, or run `oxidrive status`, to see recent conflicts, the active policy, and the device that produced each one.
- Other policies (`local_wins`, `remote_wins`, `rename`) are available in the configuration if you prefer a different behavior.

## Deletions don't propagate immediately

Deletions are **confirmed across sync cycles** before propagating (in both directions), so an accidental or transient disappearance is not pushed to other devices on the first cycle. Expect a deletion to take effect after a second sync. With `safe_delete` enabled (default), removed files are moved to `.trash/` and purged after `trash_ttl_days`; restore from there within the TTL if needed.

## systemd service fails to start

For a **user** unit:

```bash
systemctl --user status oxidrive
journalctl --user -u oxidrive
```

Logs often show a path error, permissions issue, or missing environment (variables needed for OAuth).

## Permission error on sync_dir

The sync directory (`sync_dir`) must be **readable and writable** by the user running oxidrive (or the service).

Check owner and permissions, for example:

```bash
ls -la /path/to/sync_dir
```

Fix with `chown` / `chmod` if needed (without exposing the folder to other users more than necessary).

## Google API rate limit (403 / 429)

Google Drive enforces quotas. oxidrive usually applies **retries with exponential backoff** for transient errors.

To reduce pressure on the API:

- Lower `max_concurrent_uploads` and `max_concurrent_downloads` in the configuration.
- Avoid large bursts of tiny files when possible, or spread the load.

If the issue persists, check the Google Cloud console (quotas, detailed errors) and limits for your account / project type.

## Service logs are too quiet or too chatty

The generated systemd user unit does **not** set `RUST_LOG`. After `oxidrive service install`, the process follows `log_level` in the config (default: warnings and errors). Units installed before this change may still have `Environment=RUST_LOG=info` : run `oxidrive service install` again to rewrite the unit, then `oxidrive service start`.

To see sync steps from the service, put `--verbose` on `ExecStart` (re-install after editing) or set `RUST_LOG` yourself on purpose.
