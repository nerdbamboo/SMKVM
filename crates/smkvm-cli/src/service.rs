//! Starting when the person logs in, and being stopped and started by hand.
//!
//! A daemon that has to be started by hand after every reboot is one that is
//! not running when it is wanted. What "start at login" means differs by
//! platform, and each has a trap:
//!
//! On Windows the daemon has to run *in the interactive session* and, to be
//! able to type into any window, *elevated*. A program started by ssh or a
//! service lands in session 0 and sees no desktop; one started at an ordinary
//! integrity level is refused by any window running as administrator, and
//! Windows says nothing when it refuses. A scheduled task with an interactive
//! principal and "run with highest privileges" is the one arrangement that has
//! both, so that is what is registered.
//!
//! On Linux the daemon needs the session's display, which a systemd user
//! service does not reliably have. A desktop-autostart entry runs inside the
//! session with everything the session has, and the daemon reconnects for
//! ever on its own, so nothing more is needed.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use smkvm_config::paths;
use smkvm_config::status::Status;

/// The name the task is registered under on Windows, and the file the
/// autostart entry is kept in on Linux.
pub const NAME: &str = "SMKVM";

/// The daemon running on this machine, by its own account -- and only if
/// that process is still there. A report outlives the daemon that wrote it
/// by up to its refresh interval, which is exactly the window in which
/// someone stops a daemon and starts it again; trusting the report alone
/// there refuses the restart that was asked for.
fn running_pid() -> Option<u32> {
    let pid = Status::current(&paths::status_file())?.pid?;
    (crate::process_alive(pid) != Some(false)).then_some(pid)
}

/// The binary a registered start should run: this one, by its full path.
fn this_binary() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("finding this program's own path")?;
    // Canonical, so a start registered from a relative path or a symlink
    // does not depend on where the shell happened to be.
    let exe = exe.canonicalize().unwrap_or(exe);
    // Windows canonicalizes to the verbatim form, `\\?\C:\...`, which the
    // scheduler and the person reading the task both do without.
    let shown = exe.to_string_lossy();
    Ok(match shown.strip_prefix(r"\\?\") {
        Some(plain) if plain.len() > 1 && plain.as_bytes()[1] == b':' => PathBuf::from(plain),
        _ => exe,
    })
}

/// Register a start at login. `user` is the account whose login starts it,
/// for the case where the account registering is not the one sitting at the
/// desk; `limited` gives up the elevation that lets it type into
/// administrator windows.
pub fn install(user: Option<String>, limited: bool) -> Result<()> {
    let exe = this_binary()?;
    #[cfg(windows)]
    {
        windows::install(&exe, user, limited)
    }
    #[cfg(not(windows))]
    {
        if user.is_some() {
            bail!("--user is for Windows, where a task can be registered for another account");
        }
        if limited {
            eprintln!("note: --limited means nothing here; a login item runs as you already");
        }
        linux::install(&exe)
    }
}

pub fn uninstall() -> Result<()> {
    #[cfg(windows)]
    {
        windows::uninstall()
    }
    #[cfg(not(windows))]
    {
        linux::uninstall()
    }
}

pub fn start() -> Result<()> {
    #[cfg(windows)]
    {
        windows::start()
    }
    #[cfg(not(windows))]
    {
        linux::start()
    }
}

pub fn stop() -> Result<()> {
    #[cfg(windows)]
    {
        windows::stop()
    }
    #[cfg(not(windows))]
    {
        linux::stop()
    }
}

/// Say what is registered and what is running.
pub fn status() -> Result<()> {
    #[cfg(windows)]
    {
        windows::status()?;
    }
    #[cfg(not(windows))]
    {
        linux::status()?;
    }
    match Status::current(&paths::status_file()) {
        Some(report) => println!(
            "running: {} is {} (pid {}), reported {} s ago",
            report.name,
            match report.role {
                smkvm_proto::Role::Server => "sharing its keyboard and mouse",
                smkvm_proto::Role::Client => "receiving the cursor",
            },
            report
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "?".into()),
            report.age().as_secs()
        ),
        None => println!(
            "running: nothing (no recent report at {})",
            paths::status_file().display()
        ),
    }
    Ok(())
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::process::Command;

    /// Run a PowerShell script and hand back what it printed.
    ///
    /// Encoded rather than passed as text: the script has quotes in it, and
    /// quoting through cmd and then through PowerShell has cost hours before.
    fn powershell(script: &str) -> Result<String> {
        let utf16: Vec<u8> = script
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        let output = Command::new("powershell.exe")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
                &base64(&utf16),
            ])
            .output()
            .context("running PowerShell")?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
            bail!(
                "PowerShell refused: {}",
                if stderr.is_empty() { &stdout } else { &stderr }
            );
        }
        Ok(stdout)
    }

    pub(super) fn base64(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let mut n = [0u8; 3];
            n[..chunk.len()].copy_from_slice(chunk);
            let v = (u32::from(n[0]) << 16) | (u32::from(n[1]) << 8) | u32::from(n[2]);
            for i in 0..4 {
                if i <= chunk.len() {
                    out.push(TABLE[((v >> (18 - 6 * i)) & 63) as usize] as char);
                } else {
                    out.push('=');
                }
            }
        }
        out
    }

    /// A string for PowerShell to read back as exactly this text.
    fn quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "''"))
    }

    /// The task, as the scheduler's own XML, with `$sid` left for PowerShell
    /// to fill in.
    ///
    /// Written out rather than built with the `New-ScheduledTask*` cmdlets
    /// because those look the principal up by name, and on a machine whose
    /// account is a cloud or domain one with no line to its directory the
    /// lookup fails (0x80070534) although the account logs in every day. The
    /// XML names the principal by SID, and registering it that way is
    /// accepted with no lookup at all.
    ///
    /// The action runs through a headless console host, so no console window
    /// appears at login. The one that did appear was closed by the person --
    /// which is the daemon being killed -- because a window with nothing in
    /// it looks like something to close.
    pub(super) fn task_xml(exe: &std::path::Path, limited: bool) -> String {
        let exe = exe
            .to_string_lossy()
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        let run_level = if limited {
            "LeastPrivilege"
        } else {
            "HighestAvailable"
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>Share one keyboard and mouse across several machines</Description>
  </RegistrationInfo>
  <Triggers>
    <LogonTrigger>
      <Enabled>true</Enabled>
      <UserId>$sid</UserId>
    </LogonTrigger>
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>$sid</UserId>
      <LogonType>InteractiveToken</LogonType>
      <RunLevel>{run_level}</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>5</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>5</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>conhost.exe</Command>
      <Arguments>--headless &quot;{exe}&quot; run --unattended</Arguments>
    </Exec>
  </Actions>
</Task>
"#
        )
    }

    pub fn install(exe: &std::path::Path, user: Option<String>, limited: bool) -> Result<()> {
        let who = match user {
            Some(user) => quote(&user),
            None => "$null".to_string(),
        };
        // The SID: by lookup when the machine can do one, and from the profile
        // list otherwise, which remembers every account that has logged in
        // here whether or not its directory is reachable today.
        let script = format!(
            r#"$ErrorActionPreference = 'Stop'
$who = {who}
if ($who) {{
  try {{
    $sid = (New-Object System.Security.Principal.NTAccount($who)).Translate([System.Security.Principal.SecurityIdentifier]).Value
  }} catch {{
    $short = ($who -split '\\')[-1]
    $sid = Get-ChildItem 'HKLM:\SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList' |
      Where-Object {{ (Split-Path -Leaf ([string]$_.GetValue('ProfileImagePath'))) -ieq $short }} |
      ForEach-Object {{ $_.PSChildName }} | Select-Object -First 1
    if (-not $sid) {{ throw "no account called $who has a profile on this machine" }}
  }}
}} else {{
  $me = [System.Security.Principal.WindowsIdentity]::GetCurrent()
  $who = $me.Name
  $sid = $me.User.Value
}}
$xml = @"
{xml}
"@
Register-ScheduledTask -TaskName {name} -Xml $xml -Force | Out-Null
"registered for $who ($sid)"
"#,
            xml = task_xml(exe, limited),
            name = quote(NAME),
        );
        let said = powershell(&script).with_context(|| {
            if limited {
                "registering the task".to_string()
            } else {
                "registering the task to run with highest privileges, which needs an \
                 administrator; run this from an administrator prompt, or pass --limited \
                 and accept that windows running as administrator will not take input"
                    .to_string()
            }
        })?;
        println!(
            "{said}: {} will start `{} run` at login, in the desktop session and \
             without a console window{}.",
            NAME,
            exe.display(),
            if limited {
                ""
            } else {
                ", with highest privileges so it can type into any window"
            }
        );
        println!("Start it now with `smkvm service start`.");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = stop();
        powershell(&format!(
            "Unregister-ScheduledTask -TaskName {} -Confirm:$false -ErrorAction SilentlyContinue",
            quote(NAME)
        ))?;
        println!("{NAME} no longer starts at login.");
        Ok(())
    }

    pub fn start() -> Result<()> {
        if let Some(pid) = running_pid() {
            println!("already running as pid {pid} (started by hand, not by the task).");
            return Ok(());
        }
        powershell(&format!("Start-ScheduledTask -TaskName {}", quote(NAME)))
            .context("starting the task; is it installed? `smkvm service install`")?;
        println!("started.");
        Ok(())
    }

    pub fn stop() -> Result<()> {
        // Stopping the task ends the process, and with it the input hooks,
        // which is what gives the keyboard and mouse back on a server whose
        // cursor was elsewhere. A daemon started by hand is not the task's
        // to stop, so it is stopped by its pid.
        powershell(&format!(
            "Stop-ScheduledTask -TaskName {} -ErrorAction SilentlyContinue",
            quote(NAME)
        ))?;
        if let Some(pid) = running_pid() {
            powershell(&format!("Stop-Process -Id {pid} -Force -ErrorAction Stop"))
                .with_context(|| format!("stopping pid {pid}"))?;
            println!("stopped pid {pid}.");
        } else {
            println!("stopped.");
        }
        Ok(())
    }

    pub fn status() -> Result<()> {
        let said = powershell(&format!(
            r#"$t = Get-ScheduledTask -TaskName {} -ErrorAction SilentlyContinue
if ($t) {{ "task: " + $t.State + ", runs as " + $t.Principal.UserId + " (" + $t.Principal.RunLevel + "), " + ($t.Actions | % {{ $_.Execute + " " + $_.Arguments }}) }} else {{ "task: not installed" }}"#,
            quote(NAME)
        ))?;
        println!("{said}");
        Ok(())
    }
}

#[cfg(not(windows))]
mod linux {
    use super::*;

    /// The daemon's process id, from its report, when it is running.
    fn autostart_dir() -> PathBuf {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."))
            .join("autostart")
    }

    fn entry_path() -> PathBuf {
        autostart_dir().join(format!("{}.desktop", NAME.to_lowercase()))
    }

    /// The autostart entry, as the desktop reads it.
    pub fn entry(exe: &std::path::Path) -> String {
        format!(
            "[Desktop Entry]\n\
             Type=Application\n\
             Name=SMKVM\n\
             Comment=Share one keyboard and mouse across several machines\n\
             Exec={} run --unattended\n\
             Terminal=false\n\
             NoDisplay=true\n\
             X-GNOME-Autostart-enabled=true\n",
            exe.display()
        )
    }

    pub fn install(exe: &std::path::Path) -> Result<()> {
        let path = entry_path();
        std::fs::create_dir_all(autostart_dir()).context("making the autostart directory")?;
        std::fs::write(&path, entry(exe)).with_context(|| format!("writing {}", path.display()))?;
        println!(
            "{} will start `{} run` when you log in ({}).",
            NAME,
            exe.display(),
            path.display()
        );
        println!("Start it now with `smkvm service start`.");
        Ok(())
    }

    pub fn uninstall() -> Result<()> {
        let _ = stop();
        let path = entry_path();
        match std::fs::remove_file(&path) {
            Ok(()) => println!("{NAME} no longer starts at login."),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                println!("{NAME} was not set to start at login.")
            }
            Err(e) => bail!("removing {}: {e}", path.display()),
        }
        Ok(())
    }

    pub fn start() -> Result<()> {
        use std::os::unix::process::CommandExt as _;
        if let Some(pid) = running_pid() {
            println!("already running as pid {pid}.");
            return Ok(());
        }
        let exe = this_binary()?;
        // Detached: its own process group, no terminal, so it outlives this
        // shell and logs to the file rather than to a screen nobody watches.
        let child = std::process::Command::new(&exe)
            .args(["run", "--unattended"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .with_context(|| format!("starting {}", exe.display()))?;
        println!(
            "started as pid {}; the log is {}.",
            child.id(),
            paths::log_file().display()
        );
        Ok(())
    }

    pub fn stop() -> Result<()> {
        let Some(pid) = running_pid() else {
            println!("nothing is running.");
            return Ok(());
        };
        // A polite request; the daemon removes its report and lets go of
        // everything on the way out.
        let status = std::process::Command::new("kill")
            .args(["-TERM", &pid.to_string()])
            .status()
            .context("sending the stop signal")?;
        if !status.success() {
            bail!("could not stop pid {pid}");
        }
        println!("stopped pid {pid}.");
        Ok(())
    }

    pub fn status() -> Result<()> {
        let path = entry_path();
        if path.exists() {
            println!("login item: {} (starts at login)", path.display());
        } else {
            println!("login item: none; `smkvm service install` adds one");
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn the_entry_runs_this_binary_without_a_terminal() {
            let text = entry(std::path::Path::new("/opt/smkvm/smkvm"));
            assert!(text.starts_with("[Desktop Entry]\n"));
            assert!(text.contains("Exec=/opt/smkvm/smkvm run --unattended\n"));
            assert!(text.contains("Terminal=false\n"));
            assert!(text.contains("NoDisplay=true\n"));
        }
    }
}

#[cfg(all(test, windows))]
mod task_tests {
    use super::windows::task_xml;

    #[test]
    fn the_task_runs_this_binary_headless_as_the_sid() {
        let xml = task_xml(std::path::Path::new(r"C:\Tools & Co\smkvm.exe"), false);
        assert!(xml.contains("<UserId>$sid</UserId>"));
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains(
            r"<Arguments>--headless &quot;C:\Tools &amp; Co\smkvm.exe&quot; run --unattended</Arguments>"
        ));
        assert!(!xml.contains("\"@"));
    }

    #[test]
    fn limited_gives_up_the_elevation() {
        let xml = task_xml(std::path::Path::new(r"C:\smkvm\smkvm.exe"), true);
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    }
}

#[cfg(all(test, windows))]
mod base64_tests {
    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(super::windows::base64(b""), "");
        assert_eq!(super::windows::base64(b"f"), "Zg==");
        assert_eq!(super::windows::base64(b"fo"), "Zm8=");
        assert_eq!(super::windows::base64(b"foo"), "Zm9v");
        assert_eq!(super::windows::base64(b"foobar"), "Zm9vYmFy");
    }
}
