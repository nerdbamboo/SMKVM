//! Getting a process of our own into the interactive session, on a desktop
//! we name.
//!
//! This is the part a process in the person's own session cannot do, and
//! the reason the service exists.
//!
//! ## Whose token, and why it matters more than anything else here
//!
//! The worker has to run as **LocalSystem**, and it gets there by wearing
//! the service's own token, moved into the console session. It does *not*
//! wear the logged-in person's token. The distinction decides whether any
//! of this works:
//!
//! * The `Winlogon` desktop's access list grants LocalSystem and nobody
//!   else. `CreateProcessAsUser` with `lpDesktop = WinSta0\Winlogon` and a
//!   user's token is refused outright, so the one thing this change exists
//!   to do could not happen.
//! * Even on `WinSta0\Default`, a worker wearing the person's token runs at
//!   medium integrity -- lower than the scheduled task does today. It would
//!   be refused by every window running as administrator, which is a
//!   *regression* against what three machines are running, traded for
//!   nothing.
//! * And the pipe's access list names LocalSystem alone, so a worker that
//!   is not LocalSystem cannot even open it.
//!
//! The first draft of this called `WTSQueryUserToken`, which hands back the
//! interactive user's token, while three comments and the notes said the
//! worker ran as SYSTEM. All three failures above followed from that one
//! line. What is here instead is: duplicate our own token, move the
//! duplicate into the console session, start the process with it.
//!
//! ## What is deliberately not done
//!
//! Nothing here widens the access list on `WinSta0` or on any desktop.
//! LocalSystem already has full access to both, which is precisely why
//! this arrangement is the one that reaches the secure desktop. Granting
//! the interactive user or Administrators access to the `Winlogon` desktop
//! would take apart the boundary that makes a consent prompt mean
//! anything, and would be a worse hole than the one being fixed. If
//! anything in here ever reaches for `SetSecurityInfo` on a window station
//! or a desktop, something else has gone wrong.
//!
//! ## The two traps
//!
//! `SetTokenInformation(TokenSessionId)` needs `SeTcbPrivilege`, which
//! LocalSystem's token *holds* and which is *disabled* until something
//! switches it on. Holding a privilege and having it on are two states of
//! a token, and a privilege held but off fails with
//! `ERROR_PRIVILEGE_NOT_HELD`, which reads exactly like not having it.
//! Worse, `AdjustTokenPrivileges` reports "you do not have it" by
//! *succeeding* and setting the last error, so the obvious code says
//! nothing at all.
//!
//! A null `lpDesktop` means "inherit mine", and a service's station is
//! `Service-0x0-3e7$`, which has no screen. The process would start, see
//! nothing, hook nothing and report no error -- the shape of failure this
//! repository has lost the most time to.

#![allow(unsafe_code)]

use std::path::Path;

use anyhow::{bail, Context, Result};
use windows::core::{PCWSTR, PWSTR};

use crate::secure::windows::Aligned;
use windows::Win32::Foundation::{CloseHandle, ERROR_NOT_ALL_ASSIGNED, HANDLE};
use windows::Win32::Security::{
    AdjustTokenPrivileges, DuplicateTokenEx, GetTokenInformation, LookupPrivilegeValueW,
    SecurityImpersonation, SetTokenInformation, TokenPrimary, TokenSessionId, TokenUser,
    LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED, SE_TCB_NAME, SID_NAME_USE, TOKEN_ACCESS_MASK,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_ALL_ACCESS, TOKEN_DUPLICATE, TOKEN_PRIVILEGES, TOKEN_QUERY,
    TOKEN_USER,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{WTSGetActiveConsoleSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::{
    CreateProcessAsUserW, GetCurrentProcess, OpenProcessToken, TerminateProcess,
    WaitForSingleObject, CREATE_NO_WINDOW, PROCESS_INFORMATION, STARTUPINFOW,
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
            "this process does not hold SeTcbPrivilege, so it cannot put a process in the \
             interactive session. The service has to run as LocalSystem; check the account \
             under `sc qc smkvmsystem`"
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

/// A primary token that is this service's own -- LocalSystem -- but
/// belonging to the session with the screen.
///
/// This is the whole trick, and it is two calls. The duplicate is a
/// primary token, which is what a process can be started with; an
/// impersonation token is refused by `CreateProcessAsUser` and is what
/// plain `DuplicateToken` would give. Moving it into the console session
/// is what puts the started process where the screens are rather than in
/// session 0 with the service.
///
/// The session is set on the **duplicate**, never on this process's own
/// token: moving the service itself between sessions would be a different
/// and much worse thing to do.
pub fn system_token_in_session(session: u32) -> Result<Owned> {
    let mut ours = HANDLE::default();
    // SAFETY: our own process handle needs no closing; the token is wrapped
    // below before anything can return.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_DUPLICATE | TOKEN_QUERY,
            &mut ours,
        )
    }
    .context("opening the service's own token")?;
    let ours = Owned(ours);

    let mut worker = HANDLE::default();
    // SAFETY: a valid token in, a place for the duplicate out.
    unsafe {
        DuplicateTokenEx(
            ours.0,
            TOKEN_ACCESS_MASK(TOKEN_ALL_ACCESS.0),
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut worker,
        )
    }
    .context("duplicating the service's token into one a process can be started with")?;
    let worker = Owned(worker);

    // SAFETY: the value is a u32 the call is told the size of, and the
    // token is the duplicate made just above.
    unsafe {
        SetTokenInformation(
            worker.0,
            TokenSessionId,
            &session as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        )
    }
    .with_context(|| {
        format!(
            "moving the worker's token into session {session}. This needs SeTcbPrivilege \
             enabled, which only the system account holds"
        )
    })?;
    Ok(worker)
}

/// Where the person at the screen keeps their files.
///
/// Asked because a file pasted or dragged onto this machine lands under
/// `transfer.directory`, whose default begins with `~` -- and expanded
/// in a service that is the system profile, which is not a place to put
/// something somebody just asked for.
///
/// This is a question with a good answer, unlike "whose configuration
/// should a service read". There *is* a person at the screen; the
/// system knows who; and their own environment says where their profile
/// is, so nothing here guesses at `C:\Users\<name>` or assumes profiles
/// are in the usual place.
///
/// The worker could be asked instead, being in the session -- but the
/// worker is the system account too, so it would have to ask the same
/// way, and the service already holds the privilege this needs. One
/// round trip fewer for the same answer.
pub fn home_in_session(session: u32) -> Result<std::path::PathBuf> {
    let mut token = HANDLE::default();
    // SAFETY: a place for the token; wrapped below before any return.
    unsafe { WTSQueryUserToken(session, &mut token) }.with_context(|| {
        format!("asking who is logged in at session {session}, to find out where they keep things")
    })?;
    let token = Owned(token);

    let mut block = std::ptr::null_mut();
    // SAFETY: a valid token and a place for the block, freed below.
    unsafe { CreateEnvironmentBlock(&mut block, token.0, false) }
        .context("reading that person's environment")?;
    // SAFETY: the block is a run of null-terminated wide strings ended
    // by a second null, which is what is walked to find its length.
    let found = unsafe {
        let mut len = 0usize;
        // Two zeroes in a row end the block; one ends an entry.
        while !(*block.cast::<u16>().add(len) == 0 && *block.cast::<u16>().add(len + 1) == 0) {
            len += 1;
            if len > 1 << 20 {
                break;
            }
        }
        let wide = std::slice::from_raw_parts(block.cast::<u16>(), len + 1);
        smkvm_config::paths::profile_in_environment_block(wide)
    };
    // SAFETY: the block came from the call above and is not used after.
    unsafe {
        let _ = DestroyEnvironmentBlock(block);
    }
    found.context("that person's environment does not say where their profile is")
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
    // No environment block, and none built. `CreateEnvironmentBlock` from
    // here would build LocalSystem's environment, so TEMP and APPDATA would
    // point into the system profile -- which is the hazard worth avoiding
    // only if the worker read any of it. It does not: it opens one pipe
    // whose name is on its command line, reads no configuration and touches
    // no file. A null block means it inherits the service's, which is the
    // same environment, honestly labelled.
    //
    // Quoted, always. An unquoted path with a space in it lets anything
    // called `C:\Program.exe` be started instead of what was meant, and this
    // starts things as the system account.
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

    // SAFETY: the command line and desktop buffers are null-terminated and
    // live until after the call, and the command line is writable as the
    // call requires.
    unsafe {
        CreateProcessAsUserW(
            token.0,
            None,
            PWSTR(command.as_mut_ptr()),
            None,
            None,
            false,
            CREATE_NO_WINDOW,
            None,
            None,
            &startup,
            &mut information,
        )
    }
    .with_context(|| format!("starting {} in the interactive session", exe.display()))?;

    // The thread handle is of no use to anyone here and is a handle leak if
    // it is left; the process handle is kept, to know when it has gone.
    let _ = Owned(information.hThread);
    Ok(Started {
        process: Owned(information.hProcess),
        pid: information.dwProcessId,
    })
}

/// Who this process is running as, as a name rather than a number.
pub fn whoami() -> String {
    let mut token = HANDLE::default();
    // SAFETY: a place for the token, owned below.
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.is_err() {
        return "(could not be read)".into();
    }
    let token = Owned(token);
    let mut wanted = 0u32;
    // SAFETY: asking for the size first, which is why the buffer is
    // null and the error is expected.
    unsafe {
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut wanted);
    }
    if wanted == 0 {
        return "(could not be read)".into();
    }
    // `Aligned`, not `vec![0u8; ..]`. A `TOKEN_USER` holds a pointer
    // and needs eight-byte alignment; a byte vector promises one, and
    // taking a reference to a structure inside it is undefined
    // behaviour however well the hardware tolerates the read. This
    // exact mistake was found in review, fixed elsewhere, and then
    // written again here -- see the note on `Aligned` itself.
    let mut buffer = Aligned::new(wanted as usize);
    // SAFETY: the buffer is at least the size the call just asked for.
    if unsafe {
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            wanted,
            &mut wanted,
        )
    }
    .is_err()
    {
        return "(could not be read)".into();
    }
    // SAFETY: the buffer holds a TOKEN_USER, which is what was asked
    // for; it is aligned for one, and it outlives the borrow.
    let sid = unsafe { (*buffer.as_ptr().cast::<TOKEN_USER>()).User.Sid };

    let mut name = [0u16; 256];
    let mut name_len = name.len() as u32;
    let mut domain = [0u16; 256];
    let mut domain_len = domain.len() as u32;
    let mut kind = SID_NAME_USE::default();
    // SAFETY: both buffers are valid for the lengths given, and the
    // SID came from the token above.
    let looked_up = unsafe {
        windows::Win32::Security::LookupAccountSidW(
            PCWSTR::null(),
            sid,
            PWSTR(name.as_mut_ptr()),
            &mut name_len,
            PWSTR(domain.as_mut_ptr()),
            &mut domain_len,
            &mut kind,
        )
    };
    if looked_up.is_err() {
        return "(a name could not be found for it)".into();
    }
    format!(
        "{}\\{}",
        String::from_utf16_lossy(&domain[..domain_len as usize]),
        String::from_utf16_lossy(&name[..name_len as usize])
    )
}
