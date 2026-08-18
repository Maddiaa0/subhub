//! Background-service management: LaunchAgents on macOS, systemd user units
//! on Linux. Everything platform-specific about running the gateway as a
//! daemon lives here.

use crate::gateway::GatewayTransport;
use crate::{Error, Result};
use std::env;
#[cfg(target_os = "macos")]
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "com.subhub.gateway";
#[cfg(target_os = "linux")]
const SYSTEMD_UNIT_NAME: &str = "subhub-gateway.service";

#[cfg(target_os = "macos")]
pub(super) fn background_service_path() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| {
            home.join("Library")
                .join("LaunchAgents")
                .join(format!("{LAUNCH_AGENT_LABEL}.plist"))
        })
        .ok_or_else(|| Error::Message("HOME is not set".into()))
}

#[cfg(target_os = "linux")]
pub(super) fn background_service_path() -> Result<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .map(|config| config.join("systemd").join("user").join(SYSTEMD_UNIT_NAME))
        .ok_or_else(|| Error::Message("XDG_CONFIG_HOME and HOME are not set".into()))
}

#[cfg(target_os = "macos")]
pub(super) fn write_background_service(
    path: &Path,
    binary: &Path,
    transport: GatewayTransport,
) -> Result<()> {
    let binary = xml_escape(
        binary
            .to_str()
            .ok_or_else(|| Error::Message("Subhub executable path is not UTF-8".into()))?,
    );
    let transport_arguments = if transport == GatewayTransport::Iron {
        "\n    <string>--transport</string>\n    <string>iron</string>"
    } else {
        ""
    };
    let contents = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LAUNCH_AGENT_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{binary}</string>
    <string>gateway</string>
    <string>serve</string>
    <string>--background</string>{transport_arguments}
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>ProcessType</key>
  <string>Background</string>
  <key>ThrottleInterval</key>
  <integer>10</integer>
</dict>
</plist>
"#
    );
    let parent = path
        .parent()
        .ok_or_else(|| Error::Message("LaunchAgent path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    fs::write(path, contents)
        .map_err(|error| Error::Message(format!("could not write {}: {error}", path.display())))
}

#[cfg(target_os = "linux")]
pub(super) fn write_background_service(
    path: &Path,
    binary: &Path,
    transport: GatewayTransport,
) -> Result<()> {
    let executable = systemd_quote(binary)?;
    let transport_argument = if transport == GatewayTransport::Iron {
        " --transport iron"
    } else {
        ""
    };
    let contents = format!(
        "[Unit]\nDescription=Subhub credential-routing gateway\nAfter=network-online.target\nWants=network-online.target\n\n[Service]\nType=simple\nExecStart={executable} gateway serve --background{transport_argument}\nRestart=on-failure\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n"
    );
    super::write_private_file(path, contents.as_bytes())
}

#[cfg(any(target_os = "macos", test))]
fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(target_os = "macos")]
fn launch_domain() -> Result<String> {
    Ok(format!("gui/{}", user_id()?))
}

#[cfg(target_os = "macos")]
fn launch_target() -> Result<String> {
    Ok(format!("{}/{LAUNCH_AGENT_LABEL}", launch_domain()?))
}

#[cfg(target_os = "macos")]
fn user_id() -> Result<String> {
    let output = Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .map_err(|error| Error::Message(format!("could not run `id -u`: {error}")))?;
    if !output.status.success() {
        return Err(Error::Message("`id -u` failed".into()));
    }
    String::from_utf8(output.stdout)
        .map(|value| value.trim().to_owned())
        .map_err(|_| Error::Message("`id -u` returned non-UTF-8 output".into()))
}

#[cfg(target_os = "macos")]
fn launchctl<const N: usize>(arguments: [&str; N]) -> Result<()> {
    let output = Command::new("/bin/launchctl")
        .args(arguments)
        .output()
        .map_err(|error| Error::Message(format!("could not run launchctl: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(Error::Message(if detail.is_empty() {
        format!("launchctl failed with {}", output.status)
    } else {
        format!("launchctl failed: {detail}")
    }))
}

#[cfg(target_os = "macos")]
pub(super) fn background_service_running() -> bool {
    launch_target().is_ok_and(|target| launchctl(["print", &target]).is_ok())
}

#[cfg(target_os = "macos")]
pub(super) fn start_background_service(path: &Path) -> Result<()> {
    launchctl([
        "bootstrap",
        &launch_domain()?,
        path.to_str()
            .ok_or_else(|| Error::Message("LaunchAgent path is not UTF-8".into()))?,
    ])
}

#[cfg(target_os = "macos")]
pub(super) fn stop_background_service() -> Result<()> {
    launchctl(["bootout", &launch_target()?])
}

#[cfg(target_os = "macos")]
pub(super) fn restart_background_service() -> Result<()> {
    launchctl(["kickstart", "-k", &launch_target()?])
}

#[cfg(target_os = "macos")]
pub(super) fn disable_background_service() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn refresh_background_services() -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn systemctl(arguments: &[&str]) -> Result<()> {
    let output = Command::new("systemctl")
        .arg("--user")
        .args(arguments)
        .output()
        .map_err(|error| Error::Message(format!("could not run `systemctl --user`: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    Err(Error::Message(if detail.is_empty() {
        format!("systemctl --user failed with {}", output.status)
    } else {
        format!("systemctl --user failed: {detail}")
    }))
}

#[cfg(target_os = "linux")]
pub(super) fn background_service_running() -> bool {
    systemctl(&["is-active", "--quiet", SYSTEMD_UNIT_NAME]).is_ok()
}

#[cfg(target_os = "linux")]
pub(super) fn start_background_service(_path: &Path) -> Result<()> {
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", "--now", SYSTEMD_UNIT_NAME])
}

#[cfg(target_os = "linux")]
pub(super) fn stop_background_service() -> Result<()> {
    systemctl(&["stop", SYSTEMD_UNIT_NAME])
}

#[cfg(target_os = "linux")]
pub(super) fn restart_background_service() -> Result<()> {
    systemctl(&["restart", SYSTEMD_UNIT_NAME])
}

#[cfg(target_os = "linux")]
pub(super) fn disable_background_service() -> Result<()> {
    let _ = systemctl(&["disable", SYSTEMD_UNIT_NAME]);
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn refresh_background_services() -> Result<()> {
    systemctl(&["daemon-reload"])
}

#[cfg(target_os = "linux")]
fn systemd_quote(path: &Path) -> Result<String> {
    let path = path
        .to_str()
        .ok_or_else(|| Error::Message("Subhub executable path is not UTF-8".into()))?;
    Ok(format!(
        "\"{}\"",
        path.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_escaping_handles_special_paths() {
        assert_eq!(xml_escape("a&<b>"), "a&amp;&lt;b&gt;");
    }
}
