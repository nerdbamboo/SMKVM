//! Getting hold of the interactive session, and starting something in it.
//!
//! This is the part a process in the person's own session cannot do, and
//! the reason the service exists. Three things have to be true at once:
//!
//! 1. The caller runs as LocalSystem. Only LocalSystem's token is granted
//!    `SeTcbPrivilege` by default policy, and `WTSQueryUserToken` requires
//!    both the account and the privilege.
//! 2. That privilege is *enabled*, not merely held. Holding a privilege and
//!    having it switched on are two different states of a token, and a
//!    privilege that is held but off fails the call with
//!    `ERROR_PRIVILEGE_NOT_HELD` -- which reads exactly like not having it,
//!    and is the mistake everyone makes once.
//! 3. The started process is told a window station and desktop by name. A
//!    service's own station is `Service-0x0-3e7$` and has no screen, and a
//!    null `lpDesktop` means "inherit mine". The process would start, see
//!    nothing, hook nothing and report no error at all -- the shape of
//!    failure this repository has lost the most time to.

#![allow(unsafe_code)]

use std::path::Path;

use anyhow::{bail, Context, Result};
use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, ERROR_NOT_ALL_ASSIGNED, HANDLE};
use windows::Win32::Security::{AdjustTokenPrivileges, LookupPrivilegeValueW, SE_TCB_NAME};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TokenPrimary, LUID_AND_ATTRIBUTES,
    SE_PRIVILEGE_ENABLED, TOKEN_ACCESS_MASK, TOKEN_ADJUST_PRIVILEGES, TOKEN_ALL_ACCESS,
    TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, TerminateProcess,
    WaitForSingleObject, CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION,
    STARTUPINFOW,
};

/// A handle that is closed when it goes out of scope.
///
/// Every path through this module obtains two or three handles and most of
/// them have an early return between the open and the close. Counting them
/// by hand is how a service ends up leaking a token per poll.
pub struct Owned(pub HANDLE);

// A handle is an opaque number the kernel gives meaning to; it is not a
// pointer into this process and is valid from any thread of it. The
// `windows` crate leaves `HANDLE` not `Send` because a handle can be one of
// many things and a few of them have thread affinity. A token and a process
// handle do not, so this is a statement about these two uses rather than
// about handles in general.
// SAFETY: as above -- these hold only tokens and process handles.
unsafe impl Send for Owned {}
unsafe impl Sync for Owned {}

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle came from an API that says to close it, and
            // nothing else holds it -- this type is not Clone or Copy.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

/// Switch on `SeTcbPrivilege` in this process's own token.
///
/// Called once, at the top of the service, so that a machine where it is
/// not held says so in the log at startup rather than at the first consent
/// prompt -- which is a moment nobody is reading a log.
pub fn enable_tcb_privilege() -> Result<()> {
    let mut token = HANDLE::default();
    // SAFETY: our own process handle needs no closing; the token does, and
    // is wrapped below before anything can return.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }
    .context("opening this process's token")?;
    let token = Owned(token);

    let mut luid = Default::default();
    // SAFETY: a null machine name means this machine; the name is a constant
    // wide string from the crate.
    unsafe { LookupPrivilegeValueW(None, SE_TCB_NAME, &mut luid) }
        .context("looking up SeTcbPrivilege")?;

    let privileges = TOKEN_PRIVILEGES {
        PrivilegeCount: 1,
        Privileges: [LUID_AND_ATTRIBUTES {
            Luid: luid,
            Attributes: SE_PRIVILEGE_ENABLED,
        }],
    };
    // SAFETY: one privilege, and the count says one.
    unsafe { AdjustTokenPrivileges(token.0, false, Some(&privileges), 0, None, None) }
        .context("enabling SeTcbPrivilege")?;
    // The call succeeds while doing nothing at all when the privilege is not
    // in the token: it reports that only through the last error, which is
    // the one place nobody looks.
    // SAFETY: reads this thread's last error and takes no pointers.
    if unsafe { windows::Win32::Foundation::GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
        bail!(
            "this process does not hold SeTcbPrivilege, so it cannot reach the interactive \
             session. The service has to run as LocalSystem; check the account under \
             `sc qc smkvm`"
        );
    }
    Ok(())
}

/// The session attached to the screen, if one is.
///
/// `0xFFFFFFFF` is the documented answer for "nobody is attached just now",
/// which happens while a session is being switched or connected and is a
/// wait rather than a fault.
pub fn console_session() -> Option<u32> {
    // SAFETY: takes no arguments.
    let session = unsafe { WTSGetActiveConsoleSessionId() };
    (session != u32::MAX).then_some(session)
}

/// A primary token for whoever is logged in at the screen.
///
/// `WTSQueryUserToken` already hands back a primary token, but it is
/// duplicated all the same: the duplicate is ours to set access on and to
/// close on our own schedule, and `CreateProcessAsUser` wants
/// `TOKEN_ASSIGN_PRIMARY` which the original is not guaranteed to carry.
pub fn session_token(session: u32) -> Result<Owned> {
    let mut token = HANDLE::default();
    // SAFETY: a place for the token; closed by the wrapper below.
    unsafe { WTSQueryUserToken(session, &mut token) }.with_context(|| {
        format!(
            "asking for session {session}'s token. This needs LocalSystem with \
             SeTcbPrivilege enabled; nobody may be logged in yet"
        )
    })?;
    let token = Owned(token);

    let mut primary = HANDLE::default();
    // SAFETY: a valid token in, a place for the duplicate out.
    unsafe {
        DuplicateTokenEx(
            token.0,
            TOKEN_ACCESS_MASK(TOKEN_ALL_ACCESS.0),
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    }
    .context("duplicating the session token into one a process can be started with")?;
    Ok(Owned(primary))
}

/// A process started in the interactive session, on a named desktop.
pub struct Started {
    process: Owned,
    pub pid: u32,
}

// SAFETY: it holds a process handle, which see above.
unsafe impl Send for Started {}
unsafe impl Sync for Started {}

impl Started {
    /// Has it exited?
    pub fn gone(&self) -> bool {
        // SAFETY: a valid process handle; zero means do not wait at all.
        let waited = unsafe { WaitForSingleObject(self.process.0, 0) };
        waited == windows::Win32::Foundation::WAIT_OBJECT_0
    }

    /// End it.
    ///
    /// The polite request goes down the pipe as `ToWorker::Stop`, which is
    /// what lets the worker release whatever it is holding on the desktop
    /// it is leaving. This is what happens when that is not answered.
    pub fn kill(&self) {
        // SAFETY: a valid process handle. A process that has already exited
        // makes this fail, which is the outcome wanted anyway.
        unsafe {
            let _ = TerminateProcess(self.process.0, 1);
        }
    }
}

/// Start `exe` in the interactive session, attached to `desktop`.
///
/// `desktop` is a full `WinSta0\Name` -- see `watch::on_station` for why the
/// station half is not optional.
pub fn start_on_desktop(
    token: &Owned,
    exe: &Path,
    arguments: &str,
    desktop: &str,
) -> Result<Started> {
    // The person's environment rather than LocalSystem's: `CreateProcessAsUser`
    // does not build one, and a null block means the child inherits the
    // service's, where TEMP and APPDATA point into the system profile.
    let mut environment = std::ptr::null_mut();
    // SAFETY: a valid token and a place for the block, freed below.
    let have_environment =
        unsafe { CreateEnvironmentBlock(&mut environment, token.0, false) }.is_ok();

    // Quoted, always. An unquoted path with a space in it lets anything
    // called `C:\Program.exe` be started instead of what was meant, and this
    // starts things as SYSTEM.
    let mut command: Vec<u16> = format!("\"{}\" {arguments}", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let mut desktop: Vec<u16> = desktop.encode_utf16().chain(std::iter::once(0)).collect();

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desktop.as_mut_ptr()),
        ..Default::default()
    };
    let mut information = PROCESS_INFORMATION::default();

    // SAFETY: the command line and desktop buffers are null-terminated, live
    // until after the call, and the command line is writable as the call
    // requires. The environment block is either a valid Unicode block or
    // null, and the flag matching it is set only in the first case.
    let started = unsafe {
        CreateProcessAsUserW(
            token.0,
            None,
            PWSTR(command.as_mut_ptr()),
            None,
            None,
            false,
            if have_environment {
                CREATE_UNICODE_ENVIRONMENT | CREATE_NO_WINDOW
            } else {
                CREATE_NO_WINDOW
            },
            have_environment.then_some(environment),
            None,
            &startup,
            &mut information,
        )
    };
    if have_environment {
        // SAFETY: the block came from the call above and is not used after.
        unsafe {
            let _ = DestroyEnvironmentBlock(environment);
        }
    }
    started.with_context(|| format!("starting {} in the interactive session", exe.display()))?;

    // The thread handle is of no use to anyone here and is a handle leak if
    // it is left; the process handle is kept, to know when it has gone.
    let _ = Owned(information.hThread);
    Ok(Started {
        process: Owned(information.hProcess),
        pid: information.dwProcessId,
    })
}
