//! Noticing when Windows takes the screen away.
//!
//! A UAC prompt, the lock screen and Ctrl+Alt+Del all run on a separate,
//! more privileged desktop. A process in the user's session cannot put input
//! there, and Windows will not let it: that restriction is the point of the
//! secure desktop, not a bug to be worked around.
//!
//! What can be done is to notice. A client that knows it has gone blind says
//! so and stops trying, which is the difference between the pointer sitting
//! still and the pointer fighting an invisible wall — the juddering that makes
//! a UAC prompt so unpleasant to hit while working on another machine.

#![allow(unsafe_code)]

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop,
    DESKTOP_CONTROL_FLAGS, UOI_NAME,
};
use windows::Win32::System::Threading::GetCurrentThreadId;

/// Which desktop is receiving input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputDesktop {
    /// The ordinary one this process lives on. Input will land.
    Ours,
    /// A different one, named if it would say. Nothing sent will arrive.
    Elsewhere(String),
    /// It would not say, which itself means somewhere out of reach: the call
    /// is refused precisely when the desktop belongs to a higher privilege.
    OutOfReach,
}

impl InputDesktop {
    pub fn is_reachable(&self) -> bool {
        matches!(self, InputDesktop::Ours)
    }
}

fn name_of(handle: HANDLE) -> Option<String> {
    let mut buffer = [0u16; 256];
    let mut needed = 0u32;
    // SAFETY: the buffer is valid and its length is passed in bytes, as the
    // call expects.
    let ok = unsafe {
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(buffer.as_mut_ptr() as *mut _),
            std::mem::size_of_val(&buffer) as u32,
            Some(&mut needed),
        )
    };
    ok.ok()?;
    let len = buffer.iter().position(|c| *c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

/// Ask which desktop currently has input.
pub fn current() -> InputDesktop {
    // SAFETY: opening the input desktop takes no pointers; the handle is
    // closed below on every path that obtains one.
    let input = unsafe { OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, Default::default()) };
    let Ok(input) = input else {
        // Refused, which happens exactly when the input desktop is one this
        // process has no business on.
        return InputDesktop::OutOfReach;
    };

    // SAFETY: the current thread always has a desktop.
    let ours = unsafe { GetThreadDesktop(GetCurrentThreadId()) };

    let input_name = name_of(HANDLE(input.0));
    let ours_name = ours.ok().and_then(|h| name_of(HANDLE(h.0)));

    // SAFETY: the handle came from OpenInputDesktop and is not used after.
    let _ = unsafe { CloseDesktop(input) };

    match (input_name, ours_name) {
        (Some(a), Some(b)) if a == b => InputDesktop::Ours,
        (Some(a), _) => InputDesktop::Elsewhere(a),
        (None, _) => InputDesktop::OutOfReach,
    }
}
