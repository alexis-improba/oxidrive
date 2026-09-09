//! Platform-specific service installation and lifecycle helpers for `oxidrive`.

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use std::path::Path;

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
use crate::error::OxidriveError;

#[cfg(any(target_os = "linux", target_os = "windows"))]
const UNIT_SERVICE_NAME: &str = "oxidrive";

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tracing::{error, info};

    use crate::error::OxidriveError;
    use crate::service::UNIT_SERVICE_NAME;

    /// Returns the path to `~/.config/systemd/user/oxidrive.service`.
    fn unit_file_path() -> Result<PathBuf, OxidriveError> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            OxidriveError::other("HOME is not set; cannot resolve systemd user unit path")
        })?;
        Ok(PathBuf::from(home).join(".config/systemd/user/oxidrive.service"))
    }

    /// Quotes a single argument for safe use on an `ExecStart=` line.
    fn systemd_exec_arg_token(s: &str) -> String {
        if s.chars()
            .any(|c| c.is_whitespace() || c == '"' || c == '\\')
        {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        } else {
            s.to_string()
        }
    }

    fn build_exec_start_line(
        exe: &Path,
        config_path: Option<&Path>,
    ) -> Result<String, OxidriveError> {
        let exe_str = exe.to_str().ok_or_else(|| {
            OxidriveError::other(
                "current executable path is not valid UTF-8; cannot write systemd unit",
            )
        })?;
        let mut parts = vec![systemd_exec_arg_token(exe_str), "sync".to_string()];
        if let Some(cfg) = config_path {
            let cfg_str = cfg.to_str().ok_or_else(|| {
                OxidriveError::other("config path is not valid UTF-8; cannot write systemd unit")
            })?;
            parts.push("--config".to_string());
            parts.push(systemd_exec_arg_token(cfg_str));
        }
        Ok(parts.join(" "))
    }

    fn run_systemctl_user(args: &[&str]) -> Result<(), OxidriveError> {
        info!(?args, "running systemctl --user");
        let output = Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| {
                error!(error = %e, "failed to spawn systemctl");
                OxidriveError::other(format!("failed to run systemctl: {e}"))
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            error!(
                code = ?output.status.code(),
                stdout = %stdout,
                stderr = %stderr,
                "systemctl --user command failed"
            );
            return Err(OxidriveError::other(format!(
                "systemctl --user {} failed: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        info!(
            code = ?output.status.code(),
            stdout = %stdout.trim(),
            "systemctl --user command succeeded"
        );
        Ok(())
    }

    pub fn install_service(config_path: Option<&Path>) -> Result<(), OxidriveError> {
        let exe = std::env::current_exe()?;
        let exec_start = build_exec_start_line(&exe, config_path)?;
        let unit_path = unit_file_path()?;

        if let Some(dir) = unit_path.parent() {
            std::fs::create_dir_all(dir)?;
        }

        let unit_body = format!(
            "\
[Unit]
Description=oxidrive - Google Drive bidirectional sync
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart={exec_start}
Restart=on-failure
RestartSec=30

[Install]
WantedBy=default.target
"
        );

        let mut file = std::fs::File::create(&unit_path)?;
        file.write_all(unit_body.as_bytes())?;
        file.sync_all()?;

        info!(path = %unit_path.display(), "wrote systemd user unit");
        run_systemctl_user(&["daemon-reload"])?;
        run_systemctl_user(&["enable", UNIT_SERVICE_NAME])?;
        Ok(())
    }

    pub fn uninstall_service() -> Result<(), OxidriveError> {
        run_systemctl_user(&["disable", UNIT_SERVICE_NAME])?;
        let unit_path = unit_file_path()?;
        if unit_path.is_file() {
            std::fs::remove_file(&unit_path)?;
            info!(path = %unit_path.display(), "removed systemd user unit file");
        }
        run_systemctl_user(&["daemon-reload"])?;
        Ok(())
    }

    pub fn start_service() -> Result<(), OxidriveError> {
        run_systemctl_user(&["start", UNIT_SERVICE_NAME])
    }

    pub fn stop_service() -> Result<(), OxidriveError> {
        run_systemctl_user(&["stop", UNIT_SERVICE_NAME])
    }
}

#[cfg(target_os = "linux")]
pub use linux::{install_service, start_service, stop_service, uninstall_service};

#[cfg(target_os = "macos")]
mod macos {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use tracing::{error, info};

    use crate::error::OxidriveError;

    const LAUNCHD_LABEL: &str = "com.oxidrive.sync";

    fn launch_agent_plist_path() -> Result<PathBuf, OxidriveError> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            OxidriveError::other("HOME is not set; cannot resolve launchd agent path")
        })?;
        Ok(PathBuf::from(home).join("Library/LaunchAgents/com.oxidrive.sync.plist"))
    }

    fn launch_log_path() -> Result<PathBuf, OxidriveError> {
        let home = std::env::var_os("HOME").ok_or_else(|| {
            OxidriveError::other("HOME is not set; cannot resolve launchd log path")
        })?;
        Ok(PathBuf::from(home).join("Library/Logs/oxidrive.log"))
    }

    fn xml_escape(s: &str) -> String {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
    }

    fn run_launchctl(args: &[&str]) -> Result<(), OxidriveError> {
        info!(?args, "running launchctl");
        let output = Command::new("launchctl").args(args).output().map_err(|e| {
            error!(error = %e, "failed to spawn launchctl");
            OxidriveError::other(format!("failed to run launchctl: {e}"))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            error!(
                code = ?output.status.code(),
                stdout = %stdout,
                stderr = %stderr,
                "launchctl command failed"
            );
            return Err(OxidriveError::other(format!(
                "launchctl {} failed: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        info!(
            code = ?output.status.code(),
            stdout = %stdout.trim(),
            "launchctl command succeeded"
        );
        Ok(())
    }

    pub fn install_service(config_path: Option<&Path>) -> Result<(), OxidriveError> {
        let exe = std::env::current_exe()?;
        let exe_str = xml_escape(&exe.to_string_lossy());
        let plist_path = launch_agent_plist_path()?;
        let log_path = launch_log_path()?;
        let log_path_xml = xml_escape(&log_path.to_string_lossy());

        if let Some(dir) = plist_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }

        let mut args_xml =
            format!("    <array>\n      <string>{exe_str}</string>\n      <string>sync</string>\n");
        if let Some(cfg) = config_path {
            let cfg_str = xml_escape(&cfg.to_string_lossy());
            args_xml.push_str("      <string>--config</string>\n");
            args_xml.push_str(&format!("      <string>{cfg_str}</string>\n"));
        }
        args_xml.push_str("    </array>\n");

        let plist_body = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
  <dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
{args_xml}    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{log_path_xml}</string>
    <key>StandardErrorPath</key>
    <string>{log_path_xml}</string>
  </dict>
</plist>
"#
        );

        let mut file = std::fs::File::create(&plist_path)?;
        file.write_all(plist_body.as_bytes())?;
        file.sync_all()?;

        info!(path = %plist_path.display(), "wrote launchd plist");
        let plist_path_str = plist_path.to_string_lossy().to_string();
        run_launchctl(&["load", &plist_path_str])?;
        Ok(())
    }

    pub fn uninstall_service() -> Result<(), OxidriveError> {
        let plist_path = launch_agent_plist_path()?;
        let plist_path_str = plist_path.to_string_lossy().to_string();
        let _ = run_launchctl(&["unload", &plist_path_str]);
        if plist_path.is_file() {
            std::fs::remove_file(&plist_path)?;
            info!(path = %plist_path.display(), "removed launchd plist");
        }
        Ok(())
    }

    pub fn start_service() -> Result<(), OxidriveError> {
        run_launchctl(&["start", LAUNCHD_LABEL])
    }

    pub fn stop_service() -> Result<(), OxidriveError> {
        run_launchctl(&["stop", LAUNCHD_LABEL])
    }
}

#[cfg(target_os = "macos")]
pub use macos::{install_service, start_service, stop_service, uninstall_service};

#[cfg(target_os = "windows")]
mod windows {
    use std::path::Path;
    use std::process::Command;

    use tracing::{error, info};

    use crate::error::OxidriveError;
    use crate::service::UNIT_SERVICE_NAME;

    fn quote_windows_arg(arg: &str) -> String {
        if !arg.is_empty() && !arg.chars().any(|c| c.is_whitespace() || c == '"') {
            return arg.to_string();
        }

        let mut quoted = String::from("\"");
        let mut backslashes = 0usize;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    quoted.push_str(&"\\".repeat(backslashes.saturating_mul(2).saturating_add(1)));
                    quoted.push('"');
                    backslashes = 0;
                }
                _ => {
                    if backslashes > 0 {
                        quoted.push_str(&"\\".repeat(backslashes));
                        backslashes = 0;
                    }
                    quoted.push(ch);
                }
            }
        }
        if backslashes > 0 {
            quoted.push_str(&"\\".repeat(backslashes.saturating_mul(2)));
        }
        quoted.push('"');
        quoted
    }

    fn build_task_command(exe: &str, config_path: Option<&str>) -> String {
        let mut parts = vec![quote_windows_arg(exe), "sync".to_string()];
        if let Some(cfg) = config_path {
            parts.push("--config".to_string());
            parts.push(quote_windows_arg(cfg));
        }
        parts.join(" ")
    }

    fn run_schtasks(args: &[&str]) -> Result<(), OxidriveError> {
        info!(?args, "running schtasks");
        let output = Command::new("schtasks").args(args).output().map_err(|e| {
            error!(error = %e, "failed to spawn schtasks");
            OxidriveError::other(format!("failed to run schtasks: {e}"))
        })?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() {
            error!(
                code = ?output.status.code(),
                stdout = %stdout,
                stderr = %stderr,
                "schtasks command failed"
            );
            return Err(OxidriveError::other(format!(
                "schtasks {} failed: {}",
                args.join(" "),
                stderr.trim()
            )));
        }
        info!(
            code = ?output.status.code(),
            stdout = %stdout.trim(),
            "schtasks command succeeded"
        );
        Ok(())
    }

    pub fn install_service(config_path: Option<&Path>) -> Result<(), OxidriveError> {
        let exe = std::env::current_exe()?;
        let exe_str = exe.to_str().ok_or_else(|| {
            OxidriveError::other(
                "current executable path is not valid UTF-8; cannot create scheduled task",
            )
        })?;

        let cfg_string = config_path
            .map(|cfg| {
                cfg.to_str().ok_or_else(|| {
                    OxidriveError::other(
                        "config path is not valid UTF-8; cannot create scheduled task",
                    )
                })
            })
            .transpose()?;
        let command = build_task_command(exe_str, cfg_string);

        info!(task = UNIT_SERVICE_NAME, %command, "creating scheduled task");
        run_schtasks(&[
            "/Create",
            "/TN",
            UNIT_SERVICE_NAME,
            "/TR",
            &command,
            "/SC",
            "ONLOGON",
            "/RL",
            "HIGHEST",
            "/F",
        ])
    }

    pub fn uninstall_service() -> Result<(), OxidriveError> {
        info!(task = UNIT_SERVICE_NAME, "deleting scheduled task");
        run_schtasks(&["/Delete", "/TN", UNIT_SERVICE_NAME, "/F"])
    }

    pub fn start_service() -> Result<(), OxidriveError> {
        info!(task = UNIT_SERVICE_NAME, "running scheduled task");
        run_schtasks(&["/Run", "/TN", UNIT_SERVICE_NAME])
    }

    pub fn stop_service() -> Result<(), OxidriveError> {
        info!(task = UNIT_SERVICE_NAME, "ending scheduled task");
        run_schtasks(&["/End", "/TN", UNIT_SERVICE_NAME])
    }
}

#[cfg(target_os = "windows")]
pub use windows::{install_service, start_service, stop_service, uninstall_service};

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn install_service(_config_path: Option<&Path>) -> Result<(), OxidriveError> {
    Err(OxidriveError::other(
        "oxidrive service management is not supported on this platform; Linux (systemd), macOS (launchd), and Windows (Task Scheduler) are supported",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn uninstall_service() -> Result<(), OxidriveError> {
    Err(OxidriveError::other(
        "oxidrive service management is not supported on this platform; Linux (systemd), macOS (launchd), and Windows (Task Scheduler) are supported",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn start_service() -> Result<(), OxidriveError> {
    Err(OxidriveError::other(
        "oxidrive service management is not supported on this platform; Linux (systemd), macOS (launchd), and Windows (Task Scheduler) are supported",
    ))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub fn stop_service() -> Result<(), OxidriveError> {
    Err(OxidriveError::other(
        "oxidrive service management is not supported on this platform; Linux (systemd), macOS (launchd), and Windows (Task Scheduler) are supported",
    ))
}
