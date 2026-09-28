//! The one channel into the worker.
//!
//! The access control list is in [`crate::secure::acl`] with the reasoning
//! for every character of it. What is here is the making of the pipe, and
//! the two flags that the list alone would not cover:
//!
//! * `PIPE_REJECT_REMOTE_CLIENTS`, because a named pipe is reachable over
//!   SMB as `\\machine\pipe\name` unless it says otherwise, and the access
//!   check for that arrives over the network stack. A pipe between two
//!   processes on one machine has no business being openable from another,
//!   list or no list.
//! * `FILE_FLAG_FIRST_PIPE_INSTANCE`, because without it a process that got
//!   there first owns the name and the worker connects to *that*. A name
//!   already taken has to be a refusal that is seen, not a second instance
//!   quietly added beside somebody else's.

#![allow(unsafe_code)]

use std::io::{Read, Write};

use anyhow::{bail, Context, Result};
use windows::core::{HSTRING, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, LocalFree, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_MODE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS,
    PIPE_TYPE_BYTE, PIPE_WAIT,
};

use crate::secure::acl;
use crate::secure::wire::LONGEST_FRAME;

/// One end of the pipe.
///
/// `Read` and `Write` are implemented so that `wire::read_frame` works
/// against it unchanged, which also means the framing is exercised by tests
/// on any machine and only the two calls below are Windows-only.
///
/// Reading and writing from two threads at once is deliberate and allowed:
/// a byte-mode pipe handle carries an independent read side and write side,
/// and the alternative -- one lock around both -- would have every
/// injection wait behind a read that is blocked until the person moves the
/// mouse.
pub struct Pipe(HANDLE);

// A pipe handle is a kernel object, not a pointer into this process, and has
// no thread affinity: the whole design here is one thread reading it while
// another writes. `HANDLE` is not `Send` in the crate because some handles do
// have affinity; this one does not.
// SAFETY: as above.
unsafe impl Send for Pipe {}
unsafe impl Sync for Pipe {}

impl Pipe {
    /// A second handle onto the same pipe for the other direction.
    ///
    /// The same underlying object, so what is written here arrives there;
    /// two handles only so that each thread owns one and neither closes the
    /// other's out from under it.
    pub fn share(&self) -> Result<Pipe> {
        use windows::Win32::Foundation::DUPLICATE_SAME_ACCESS;
        use windows::Win32::System::Threading::GetCurrentProcess;
        let mut copy = HANDLE::default();
        // SAFETY: a valid handle, duplicated within this process.
        unsafe {
            windows::Win32::Foundation::DuplicateHandle(
                GetCurrentProcess(),
                self.0,
                GetCurrentProcess(),
                &mut copy,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )
        }
        .context("making a second handle onto the pipe")?;
        Ok(Pipe(copy))
    }
}

impl Drop for Pipe {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle is this value's own; closing it is what
            // tells the other end the conversation is over.
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

impl Read for Pipe {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let mut read = 0u32;
        // SAFETY: the buffer is valid for its own length.
        unsafe { ReadFile(self.0, Some(buffer), Some(&mut read), None) }
            .map_err(std::io::Error::other)?;
        Ok(read as usize)
    }
}

impl Write for Pipe {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let mut written = 0u32;
        // SAFETY: the buffer is valid for its own length.
        unsafe { WriteFile(self.0, Some(buffer), Some(&mut written), None) }
            .map_err(std::io::Error::other)?;
        Ok(written as usize)
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

/// Make the pipe and wait for the worker to come to it.
///
/// Blocks until something connects, which is why it runs on the thread that
/// is minding the worker rather than on the daemon's.
pub fn serve(name: &str) -> Result<Pipe> {
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
                | windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(
                    FILE_FLAG_FIRST_PIPE_INSTANCE.0,
                ),
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
        bail!("making the pipe {}: {why}", acl::pipe_path(name));
    }
    let pipe = Pipe(handle);

    // SAFETY: a valid pipe handle, and no overlapped structure because this
    // is a blocking wait on purpose.
    unsafe { ConnectNamedPipe(pipe.0, None) }.or_else(|e| {
        // The worker can get there between the make and the wait, which is
        // reported as this and is not a failure.
        if e.code() == windows::Win32::Foundation::ERROR_PIPE_CONNECTED.to_hresult() {
            Ok(())
        } else {
            Err(e)
        }
    })?;
    Ok(pipe)
}

/// Open the pipe from the worker's side.
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
            Default::default(),
            None,
        )
    }
    .with_context(|| format!("opening {}", acl::pipe_path(name)))?;
    Ok(Pipe(handle))
}
