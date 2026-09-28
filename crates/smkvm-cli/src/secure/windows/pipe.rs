//! The one channel into the worker, and the proof that it is the one.
//!
//! Two halves of one question, and the first draft answered only the first.
//!
//! **Who may connect to the pipe this process made.** That is the access
//! control list, which is in [`crate::secure::acl`] with the reasoning for
//! every character of it, plus two flags:
//!
//! * `PIPE_REJECT_REMOTE_CLIENTS`, because a named pipe is reachable over
//!   SMB as `\\machine\pipe\name` unless it says otherwise, and the access
//!   check for that arrives over the network stack. A pipe between two
//!   processes on one machine has no business being openable from another,
//!   list or no list.
//! * `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a name already taken is a refusal
//!   rather than a second instance added quietly beside somebody else's.
//!
//! **Whether the pipe the worker opened is the one this process made.** The
//! list above says nothing about that, and it is the half that matters
//! more, because any authenticated user may create a name in the pipe
//! namespace. Given a name it can predict, an unprivileged process creates
//! it first with a permissive list of its own and waits; the worker -- a
//! process running as the system account -- connects to *that*, and its
//! holder can then both drive keyboard injection onto whatever desktop the
//! worker is on and, by default, put on the worker's identity with
//! `ImpersonateNamedPipeClient`. That is a complete local escalation to
//! the system account, and it is worth being plain that it is only not a
//! hole today because the worker used to run as the person rather than as
//! the system.
//!
//! Three things rule it out, listed as [`acl::Guard`] so that dropping one
//! is a deletion somebody has to make on purpose:
//!
//! 1. the name comes from the system's random number generator, so nobody
//!    can create it in advance;
//! 2. [`connect`] opens with `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`,
//!    so the holder of whatever it reached may learn who the worker is but
//!    may not become it;
//! 3. [`Pipe::server_is_the_system`] asks which process is serving the pipe
//!    and refuses to say a word unless that process is the system account.
//!    This is the one that does not depend on an attacker having failed at
//!    something.
//!
//! ## Why the service's side is overlapped
//!
//! Every wait on this pipe from the service has a deadline, and that is
//! not tidiness either. The first draft waited in `ConnectNamedPipe` with
//! no timeout, on the thread that also minds the worker; a worker that
//! never arrived -- which, with the access list as written, was every
//! worker -- deadlocked that thread on its first poll. No relaunch, no
//! giving up, no stopping the service, and no error anywhere, because a
//! hang is not an error. The same argument applies to writing: a worker
//! wedged on the Winlogon desktop stops reading, the pipe's buffer fills,
//! and an injection that blocks for ever holds the lock that every other
//! injection wants. A deadline turns both into "the worker is dead", which
//! is a thing the rest of the program already knows how to handle.

#![allow(unsafe_code)]

use std::io::{Read, Write};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_IO_PENDING, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE,
    HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    CreateWellKnownSid, EqualSid, GetTokenInformation, TokenUser, WinLocalSystemSid,
    PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES, FILE_FLAG_FIRST_PIPE_INSTANCE,
    FILE_FLAG_OVERLAPPED, FILE_SHARE_MODE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
    SECURITY_IDENTIFICATION, SECURITY_SQOS_PRESENT,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeServerProcessId, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::Threading::{
    CreateEventW, OpenProcess, OpenProcessToken, ResetEvent, WaitForSingleObject,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};

use crate::secure::acl;
use crate::secure::windows::token::Owned;
use crate::secure::windows::Aligned;
use crate::secure::wire::LONGEST_FRAME;

/// How long an injection may take to reach the worker before the worker is
/// treated as dead.
///
/// A write to a pipe whose reader is alive returns in microseconds. A
/// second is several thousand times that, so anything exceeding it is not
/// a slow worker, it is a stopped one.
pub const WRITE_WITHIN: Duration = Duration::from_secs(1);

/// How long to give a worker to come to the pipe after being started.
///
/// Process creation on a loaded machine, plus opening one pipe. Five
/// seconds is generous; the cost of it being too short is one counted
/// failure and another attempt, and the cost of it being absent is the
/// deadlock this replaced.
pub const CONNECT_WITHIN: Duration = Duration::from_secs(5);

/// One end of the pipe.
///
/// `Read` and `Write` are implemented so that `wire::read_frame` works
/// against it unchanged, which also means the framing is exercised by tests
/// on any machine and only the calls below are Windows-only.
///
/// Reading and writing from two threads at once is deliberate and allowed:
/// each thread holds its own [`Pipe`] from [`Pipe::share`], with its own
/// handle and its own event, onto the same underlying pipe.
pub struct Pipe {
    handle: HANDLE,
    /// Set on the service's side, where every wait has a deadline. The
    /// worker's side is plain blocking: a worker with nothing to do should
    /// wait for ever, and a worker that hangs is a worker the service
    /// notices and replaces.
    event: Option<Owned>,
}

// A pipe handle is a kernel object, not a pointer into this process, and has
// no thread affinity: the whole design here is one thread reading it while
// another writes. `HANDLE` is not `Send` in the crate because some handles do
// have affinity; this one does not.
// SAFETY: as above.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Drop for Pipe {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            // SAFETY: the handle is this value's own; closing it is what
            // tells the other end the conversation is over.
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }
}

fn new_event() -> Result<Owned> {
    // SAFETY: a manual-reset, initially unsignalled, unnamed event.
    let event = unsafe { CreateEventW(None, true, false, None) }
        .context("making the event an overlapped wait is signalled through")?;
    Ok(Owned(event))
}

impl Pipe {
    /// A second handle onto the same pipe for the other direction.
    ///
    /// The same underlying object, so what is written here arrives there;
    /// two handles only so that each thread owns one and neither closes the
    /// other's out from under it. An overlapped pipe's copy gets its own
    /// event, because two threads waiting on one event would each take the
    /// other's completion.
    pub fn share(&self) -> Result<Pipe> {
        use windows::Win32::Foundation::DUPLICATE_SAME_ACCESS;
        use windows::Win32::System::Threading::GetCurrentProcess;
        let mut copy = HANDLE::default();
        // SAFETY: a valid handle, duplicated within this process.
        unsafe {
            windows::Win32::Foundation::DuplicateHandle(
                GetCurrentProcess(),
                self.handle,
                GetCurrentProcess(),
                &mut copy,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .context("making a second handle onto the pipe")?;
        let event = match self.event {
            Some(_) => Some(new_event()?),
            None => None,
        };
        Ok(Pipe {
            handle: copy,
            event,
        })
    }

    /// Start an overlapped read or write and see it through, or give up on
    /// it cleanly.
    ///
    /// `within` of `None` waits for ever, which is right for a read: the
    /// service's reader thread has nothing else to do and ends when the
    /// pipe breaks.
    fn awaited(
        &self,
        event: &Owned,
        within: Option<Duration>,
        start: impl FnOnce(*mut OVERLAPPED) -> windows::core::Result<()>,
    ) -> std::io::Result<usize> {
        // SAFETY: a manual-reset event this value owns.
        unsafe { ResetEvent(event.0) }.map_err(std::io::Error::other)?;
        let mut overlapped = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        // The structure lives on this stack frame, and every path below
        // either waits for the operation to finish or cancels it and waits
        // for the cancellation, so it is never still in use on return.
        match start(&mut overlapped) {
            Ok(()) => {}
            Err(e) if e.code() == ERROR_IO_PENDING.to_hresult() => {}
            // A worker that got to the pipe between it being made and
            // this call is reported as this, and is a success. It has to
            // be recognised here, while the error is still a typed
            // `windows` one: everything below wraps it with
            // `io::Error::other`, which keeps the message and drops the
            // code, so a caller downstream cannot tell it apart -- which
            // is what made the branch that used to try unreachable.
            Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult() => return Ok(0),
            Err(e) => return Err(std::io::Error::other(e)),
        }
        let milliseconds = match within {
            Some(within) => within.as_millis().min(u128::from(u32::MAX - 1)) as u32,
            None => windows::Win32::System::Threading::INFINITE,
        };
        // SAFETY: a valid event handle.
        let waited = unsafe { WaitForSingleObject(event.0, milliseconds) };
        if waited != WAIT_OBJECT_0 {
            // SAFETY: cancels the operations this thread started on this
            // handle; the wait below is what makes the OVERLAPPED above
            // safe to let go of.
            unsafe {
                let _ = CancelIoEx(self.handle, Some(&overlapped));
                let mut drained = 0u32;
                let _ = GetOverlappedResult(self.handle, &overlapped, &mut drained, true);
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the worker did not answer in time",
            ));
        }
        let mut moved = 0u32;
        // SAFETY: the operation has completed, as the signalled event says.
        unsafe { GetOverlappedResult(self.handle, &overlapped, &mut moved, false) }
            .map_err(std::io::Error::other)?;
        Ok(moved as usize)
    }

    /// Write, giving up if the worker is not reading.
    ///
    /// The plain `Write` implementation is this with [`WRITE_WITHIN`]; it
    /// is named separately so the call site that cares can say so.
    pub fn write_within(&mut self, buffer: &[u8], within: Duration) -> std::io::Result<usize> {
        match &self.event {
            Some(event) => self.awaited(event, Some(within), |overlapped| {
                // SAFETY: the buffer outlives the wait inside `awaited`,
                // which does not return until the write has finished or
                // been cancelled and drained.
                unsafe { WriteFile(self.handle, Some(buffer), None, Some(overlapped)) }
            }),
            None => {
                let mut written = 0u32;
                // SAFETY: the buffer is valid for its own length.
                unsafe { WriteFile(self.handle, Some(buffer), Some(&mut written), None) }
                    .map_err(std::io::Error::other)?;
                Ok(written as usize)
            }
        }
    }

    /// Which process is on the other end, and is it the system account?
    ///
    /// Asked by the worker, of the service, before the worker says
    /// anything at all. Everything else in this file keeps the wrong
    /// people out of the right pipe; this is what keeps the worker out of
    /// the wrong pipe, and it is the only one of the three that does not
    /// rely on an attacker having been unlucky.
    ///
    /// There is a theoretical race between asking which process serves
    /// the pipe and opening it: that process could exit and its id be
    /// reused by one running as the system account. Exploiting it needs
    /// the attacker's pipe to still be served by a process that has
    /// exited *and* the id to be reused by the system inside the same
    /// window. It is written down here so the next reader does not
    /// rediscover it and wonder whether anybody noticed; it is not worth
    /// code.
    pub fn server_is_the_system(&self) -> Result<()> {
        let mut pid = 0u32;
        // SAFETY: a valid pipe handle and a place for the id.
        unsafe { GetNamedPipeServerProcessId(self.handle, &mut pid) }
            .context("asking which process is serving this pipe")?;

        // SAFETY: asking for a handle by id; wrapped below.
        let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }
            .with_context(|| format!("opening the process serving the pipe (id {pid})"))?;
        let process = Owned(process);

        let mut token = HANDLE::default();
        // SAFETY: a valid process handle and a place for the token.
        unsafe { OpenProcessToken(process.0, TOKEN_QUERY, &mut token) }
            .context("opening the token of the process serving the pipe")?;
        let token = Owned(token);

        let mut needed = 0u32;
        // SAFETY: asking for the size writes only to `needed`; the call
        // fails by design, which is how the size is learned.
        let _ = unsafe { GetTokenInformation(token.0, TokenUser, None, 0, &mut needed) };
        if needed == 0 {
            bail!("the process serving the pipe would not say who it is");
        }
        // Aligned for a structure with a pointer in it, because that is
        // what is read back out of it below.
        let mut buffer = Aligned::new(needed as usize);
        // SAFETY: the buffer is at least `needed` bytes, which is what was
        // asked for above.
        unsafe {
            GetTokenInformation(
                token.0,
                TokenUser,
                Some(buffer.as_mut_ptr() as *mut _),
                buffer.len() as u32,
                &mut needed,
            )
        }
        .context("reading who the process serving the pipe runs as")?;

        let mut system = vec![0u8; 128];
        let mut size = system.len() as u32;
        // SAFETY: a buffer and its length; the call writes a SID into it.
        unsafe {
            CreateWellKnownSid(
                WinLocalSystemSid,
                None,
                PSID(system.as_mut_ptr() as *mut _),
                &mut size,
            )
        }
        .context("building the system account's identifier to compare against")?;

        // SAFETY: the system filled the buffer with a TOKEN_USER whose SID
        // pointer refers into the same buffer; the buffer is `Aligned`, so
        // a reference to the structure in it is properly aligned, which a
        // `Vec<u8>` would not have guaranteed. `system` holds a SID the
        // call above wrote.
        let same = unsafe {
            let user = &*(buffer.as_ptr() as *const TOKEN_USER);
            EqualSid(user.User.Sid, PSID(system.as_mut_ptr() as *mut _))
        };
        if same.is_err() {
            bail!(
                "the pipe is served by process {pid}, which is not the system account. \
                 Something created this pipe name before the service did, and it is not \
                 being spoken to"
            );
        }
        Ok(())
    }
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match &self.event {
            // No deadline: the reader has nothing to do but wait, and a
            // read that ends is how it learns the pipe has gone.
            Some(event) => self.awaited(event, None, |overlapped| {
                // SAFETY: the buffer outlives the wait inside `awaited`.
                unsafe { ReadFile(self.handle, Some(buffer), None, Some(overlapped)) }
            }),
            None => {
                let mut read = 0u32;
                // SAFETY: the buffer is valid for its own length.
                unsafe { ReadFile(self.handle, Some(buffer), Some(&mut read), None) }
                    .map_err(std::io::Error::other)?;
                Ok(read as usize)
            }
        }
    }
}

impl Write for Pipe {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.write_within(buffer, WRITE_WITHIN)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        // A pipe write has already gone to the other end when it returns.
        Ok(())
    }
}

/// A security descriptor built from [`acl::PIPE_SDDL`], freed when dropped.
struct Descriptor(PSECURITY_DESCRIPTOR);

impl Descriptor {
    fn build() -> Result<Descriptor> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: a constant wide string in, a place for the descriptor out.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                &HSTRING::from(acl::PIPE_SDDL),
                SDDL_REVISION_1,
                &mut descriptor,
                None,
            )
        }
        .with_context(|| format!("reading the pipe's access list, {}", acl::PIPE_SDDL))?;
        Ok(Descriptor(descriptor))
    }
}

impl Drop for Descriptor {
    fn drop(&mut self) {
        if !self.0 .0.is_null() {
            // SAFETY: the descriptor came from the call above, which says to
            // free it this way.
            unsafe {
                let _ = LocalFree(windows::Win32::Foundation::HLOCAL(self.0 .0));
            }
        }
    }
}

/// Make the pipe.
///
/// Separate from waiting for the worker, and called *before* the worker is
/// started, so that a name somebody else got to first is a refusal that
/// happens while there is still nothing to tell. The first draft spawned a
/// thread to do this and started the worker without waiting to see whether
/// it had worked, which meant a squatted name produced a worker talking to
/// the squatter -- with a comment above it claiming the opposite.
pub fn create(name: &str) -> Result<Pipe> {
    let descriptor = Descriptor::build()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0 .0,
        // Not inherited. The worker is told the pipe's name and opens it by
        // name; handing it an inherited handle as well would be a second
        // way in, and the point of all this is that there is one.
        bInheritHandle: false.into(),
    };
    let path = HSTRING::from(acl::pipe_path(name));

    // SAFETY: a null-terminated name, a valid descriptor that outlives the
    // call, and buffer sizes in bytes.
    let handle = unsafe {
        CreateNamedPipeW(
            &path,
            PIPE_ACCESS_DUPLEX
                | FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_FIRST_PIPE_INSTANCE.0)
                | FILE_FLAGS_AND_ATTRIBUTES(FILE_FLAG_OVERLAPPED.0),
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
            1,
            LONGEST_FRAME as u32,
            LONGEST_FRAME as u32,
            0,
            Some(&attributes),
        )
    };
    // This one returns the sentinel rather than an error, unlike almost
    // everything else here.
    if handle == INVALID_HANDLE_VALUE {
        let why = windows::core::Error::from_win32();
        bail!(
            "making the pipe {}: {why}. If this says access denied, something else on this \
             machine already holds that name",
            acl::pipe_path(name)
        );
    }
    Ok(Pipe {
        handle,
        event: Some(new_event()?),
    })
}

/// Wait for the worker to come to the pipe, for a bounded time.
pub fn accept(pipe: &Pipe, within: Duration) -> Result<()> {
    let event = pipe
        .event
        .as_ref()
        .context("waiting on a pipe that was not made for waiting")?;
    let outcome = pipe.awaited(event, Some(within), |overlapped| {
        // SAFETY: a valid pipe handle; the overlapped structure outlives
        // the wait inside `awaited`.
        unsafe { ConnectNamedPipe(pipe.handle, Some(overlapped)) }
    });
    match outcome {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => bail!(
            "no worker came to the pipe within {} s",
            within.as_secs_f32()
        ),
        // A worker already connected is a success and is recognised in
        // `awaited`, where the error still carries its code.
        Err(e) => Err(e).context("waiting for the worker"),
    }
}

/// Open the pipe from the worker's side.
///
/// `SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION` is the important part
/// and is not a default. A named pipe opened without it lets whoever is
/// serving it call `ImpersonateNamedPipeClient` and become the client --
/// and this client is a process running as the system account. With
/// identification only, the server may learn who the worker is and may not
/// act as it. That is worth having even though [`Pipe::server_is_the_system`]
/// is meant to make it moot: the two failures are independent, and the
/// cost of both is nothing.
pub fn connect(name: &str) -> Result<Pipe> {
    let path = HSTRING::from(acl::pipe_path(name));
    // SAFETY: a null-terminated path; the handle is wrapped before return.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0),
            None,
        )
    }
    .with_context(|| format!("opening {}", acl::pipe_path(name)))?;
    let pipe = Pipe {
        handle,
        event: None,
    };
    // Before a single byte is sent, including the hello.
    pipe.server_is_the_system()?;
    Ok(pipe)
}
