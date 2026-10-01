//! Two measurements that decide a design, taken in one run.
//!
//! A process running as the system account cannot see what the
//! logged-on user copied. That is measured and settled. What is not
//! settled is which of two shapes fixes it, and the whole of the next
//! piece of work rests on the answer:
//!
//! 1. Does a process the **service** starts, with the logged-on
//!    user's token, see what that user copied? A process started by
//!    hand does. The token path here is different -- `WTSQueryUserToken`
//!    and `CreateProcessAsUserW` rather than a shell -- and the design
//!    rests on it, so it is worth the few minutes rather than three
//!    days.
//! 2. Does a system thread that **impersonates** the user see it? If
//!    it does, one process can do both jobs and there is no second
//!    helper to supervise, which is a cheaper road and the person
//!    should be offered it before anything is built.
//!
//! Both are taken here, next to each other, by the same code looking
//! the same way, so the three answers can be read down one page
//! instead of assembled from three runs.
//!
//! This exists to be deleted. When the road is chosen it has done its
//! job.

#![allow(unsafe_code)]

use std::path::PathBuf;

use anyhow::{Context, Result};
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Security::{
    GetTokenInformation, ImpersonateLoggedOnUser, RevertToSelf, TokenUser, SID_NAME_USE,
    TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::RemoteDesktop::WTSQueryUserToken;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::secure::windows::token::{self, Owned};

/// Where a child leaves what it saw, inside the profile of the person
/// it is running as -- the one place it is certain to be able to
/// write and the parent, being the system account, is certain to be
/// able to read.
const CHILD_LEAVES_IT: &str = "smkvm-clipboard-probe.txt";

/// How long to wait for the child to look and write.
const CHILD_WITHIN: std::time::Duration = std::time::Duration::from_secs(20);

/// Take the measurements, or -- as a child -- report and exit.
pub fn probe(report_to: Option<PathBuf>) -> Result<()> {
    let mine = look("this process");

    if let Some(path) = report_to {
        // The child half. Says what it saw and stops; the parent
        // prints it, because the child has no console to print to.
        std::fs::write(&path, mine)
            .with_context(|| format!("writing what was seen to {}", path.display()))?;
        return Ok(());
    }

    println!("{mine}");

    if !running_as_the_system_account() {
        println!(
            "Run this as the system account as well. As the person at the desk it shows\n\
             only the first row of the table, which is the row we already know."
        );
        return Ok(());
    }

    println!("{}", impersonating());
    println!("{}", as_a_child_of_the_service());
    println!(
        "Reading the three: if the child sees the formats, the read-only helper is\n\
         viable. If the impersonating thread sees them too, one process can do both\n\
         jobs and the helper is not needed. If neither sees them, both roads are shut\n\
         and the finding is bigger than the plan."
    );
    Ok(())
}

/// What one observer sees, under a heading.
fn look(who: &str) -> String {
    let mut out = format!("--- {who} ---\n");
    out.push_str(&format!("running as     {}\n", whoami()));
    out.push_str(&format!(
        "session        {}\n",
        token::console_session()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "(nobody is at the screen)".into())
    ));
    out.push_str(&format!(
        "desktop        {}\n",
        smkvm_input::platform::windows::desktop::ours()
            .unwrap_or_else(|| "(could not be read)".into())
    ));
    for line in smkvm_clipboard::platform::windows::verdict() {
        out.push_str(&format!("{line}\n"));
    }
    out
}

/// The second measurement: a system thread wearing the user's token.
fn impersonating() -> String {
    let Some(session) = token::console_session() else {
        return "--- impersonating the user ---\nnobody is at the screen\n".to_string();
    };
    let mut handle = HANDLE::default();
    // SAFETY: a place for the token, owned below before any return.
    if let Err(e) = unsafe { WTSQueryUserToken(session, &mut handle) } {
        return format!("--- impersonating the user ---\ncould not get their token: {e}\n");
    }
    let theirs = Owned(handle);
    // SAFETY: a token that has just been obtained for this purpose.
    if let Err(e) = unsafe { ImpersonateLoggedOnUser(theirs.0) } {
        return format!("--- impersonating the user ---\ncould not put it on: {e}\n");
    }
    let seen = look("impersonating the user");
    // SAFETY: balanced against the impersonate above. Taken off before
    // anything else happens on this thread, because a thread left
    // wearing somebody else's token is a security fault rather than an
    // untidiness.
    unsafe {
        let _ = RevertToSelf();
    }
    seen
}

/// The first measurement: a process the service starts as the user.
fn as_a_child_of_the_service() -> String {
    let heading = "--- a child started as the user ---";
    let Some(session) = token::console_session() else {
        return format!("{heading}\nnobody is at the screen\n");
    };
    if let Err(e) = token::enable_tcb_privilege() {
        return format!("{heading}\nwithout the privilege to start one: {e}\n");
    }
    let home = match token::home_in_session(session) {
        Ok(home) => home,
        Err(e) => return format!("{heading}\ncould not find where they keep things: {e}\n"),
    };
    let leaves_it = home.join(CHILD_LEAVES_IT);
    let _ = std::fs::remove_file(&leaves_it);

    let mut handle = HANDLE::default();
    // SAFETY: a place for the token, owned below before any return.
    if let Err(e) = unsafe { WTSQueryUserToken(session, &mut handle) } {
        return format!("{heading}\ncould not get their token: {e}\n");
    }
    let theirs = Owned(handle);
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return format!("{heading}\ncould not find this program: {e}\n"),
    };
    // `Default` on purpose: this is about the desktop the person's
    // copies live on, not the one a consent prompt is on.
    let started = token::start_on_desktop(
        &theirs,
        &exe,
        &format!("clipboard-probe --report-to \"{}\"", leaves_it.display()),
        r"WinSta0\Default",
    );
    let started = match started {
        Ok(started) => started,
        Err(e) => return format!("{heading}\ncould not start one: {e:#}\n"),
    };

    let deadline = std::time::Instant::now() + CHILD_WITHIN;
    while std::time::Instant::now() < deadline {
        if leaves_it.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let seen = match std::fs::read_to_string(&leaves_it) {
        Ok(seen) => seen,
        Err(e) => format!(
            "{heading}\nit was started (pid {}) but left nothing at {} within {} s: {e}\n",
            started.pid,
            leaves_it.display(),
            CHILD_WITHIN.as_secs()
        ),
    };
    let _ = std::fs::remove_file(&leaves_it);
    seen
}

/// Who this process is running as, as a name rather than a number.
fn whoami() -> String {
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
    let mut buffer = vec![0u8; wanted as usize];
    // SAFETY: the buffer is the size the call just asked for.
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
    // for, and it outlives the borrow.
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

/// Is this the system account?
///
/// By name, which is enough for a probe and wrong for a decision --
/// names are translated and can be impersonated. Nothing is granted
/// on the strength of this; it only decides which rows of the table
/// can be filled in.
fn running_as_the_system_account() -> bool {
    whoami().to_ascii_uppercase().ends_with("\\SYSTEM")
}
