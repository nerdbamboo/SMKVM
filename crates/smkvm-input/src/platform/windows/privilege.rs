//! Noticing when the window in front outranks this process.
//!
//! Windows will not deliver injected input to a window whose process runs at
//! a higher integrity level than the one injecting: an administrator's
//! PowerShell, a Task Manager, an installer. `SendInput` returns zero and
//! nothing else happens -- no error dialog, no log line from the system, and
//! from the person's side a pointer that has simply stopped while the machine
//! sending it goes on swallowing their keyboard and mouse.
//!
//! The restriction is deliberate on Windows' part and not something to route
//! around. What can be done is to notice it, say which window it is, and hand
//! the cursor back until the foreground changes.

#![allow(unsafe_code)]

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// What is in front, when it is something injected input cannot reach.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outranked {
    /// The program's file name, or its process id if that could not be read.
    pub program: String,
    pub theirs: u32,
    pub ours: u32,
}

/// The integrity level of a process, as the RID Windows uses: 0x1000 low,
/// 0x2000 medium, 0x3000 high, 0x4000 system.
fn integrity_of(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    // SAFETY: a valid process handle and a place for the token.
    unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.ok()?;
    let level = read_integrity(token);
    // SAFETY: balanced against the open above.
    unsafe {
        let _ = CloseHandle(token);
    }
    level
}

fn read_integrity(token: HANDLE) -> Option<u32> {
    let mut needed = 0u32;
    // SAFETY: asking for the size writes only to `needed`; the call fails by
    // design, which is how the size is learned.
    let _ = unsafe { GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut needed) };
    if needed == 0 {
        return None;
    }
    let mut buffer = vec![0u8; needed as usize];
    // SAFETY: the buffer is at least `needed` bytes, which is what was asked.
    unsafe {
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buffer.as_mut_ptr() as *mut _),
            needed,
            &mut needed,
        )
    }
    .ok()?;
    // SAFETY: the system filled the buffer with a TOKEN_MANDATORY_LABEL whose
    // SID pointer refers into the same buffer.
    let label = unsafe { &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL) };
    let sid = label.Label.Sid;
    // SAFETY: the SID is valid for as long as the buffer is, and the count
    // pointer refers into it.
    unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

fn name_of(process: HANDLE) -> Option<String> {
    let mut buffer = [0u16; 1024];
    let mut len = buffer.len() as u32;
    // SAFETY: the buffer is valid for `len` characters.
    unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    let path = String::from_utf16_lossy(&buffer[..len as usize]);
    Some(path.rsplit(['\\', '/']).next().unwrap_or(&path).to_string())
}

/// The integrity level this process runs at, as the RID.
pub fn our_level() -> Option<u32> {
    // SAFETY: our own process handle needs no closing.
    integrity_of(unsafe { GetCurrentProcess() })
}

/// Does the window in front belong to a process that outranks this one?
///
/// `None` when it does not, or when there is no foreground window, or when
/// the question cannot be answered -- which for a process this one may not
/// even open is itself a fair sign of rank, but not one worth acting on.
pub fn foreground_outranks_us() -> Option<Outranked> {
    // SAFETY: no pointers; a null handle means no foreground window.
    let window = unsafe { GetForegroundWindow() };
    if window.0.is_null() {
        return None;
    }
    let mut pid = 0u32;
    // SAFETY: a valid window handle and a place for the process id.
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    if pid == 0 {
        return None;
    }
    let ours = our_level()?;

    // SAFETY: asking for a handle by id; closed below on every path.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let theirs = integrity_of(process);
    let program = name_of(process).unwrap_or_else(|| format!("process {pid}"));
    // SAFETY: balanced against the open above.
    unsafe {
        let _ = CloseHandle(process);
    }
    let theirs = theirs?;
    (theirs > ours).then_some(Outranked {
        program,
        theirs,
        ours,
    })
}

/// The everyday name for an integrity level.
pub fn describe_level(rid: u32) -> &'static str {
    match rid {
        0..=0x0FFF => "untrusted",
        0x1000..=0x1FFF => "low",
        0x2000..=0x2FFF => "medium (an ordinary program)",
        0x3000..=0x3FFF => "high (run as administrator)",
        _ => "system",
    }
}
