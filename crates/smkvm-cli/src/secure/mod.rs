//! Reaching the desktop a UAC prompt lives on.
//!
//! Everything else in this program runs in the person's own session, and
//! that is enough for every window they can see -- once it runs at a high
//! integrity level, which the login task already arranges. It is not enough
//! for the UAC consent dialog, the lock screen or Ctrl+Alt+Del. Those do not
//! run on the person's desktop at all: winlogon creates a second desktop
//! ("Winlogon", on the WinSta0 window station), switches input to it, and a
//! process in the session cannot open it, hook it or inject into it at any
//! integrity level whatsoever. That is the point of it, and no amount of
//! elevation changes it. `desktop::current()` notices, the client suspends,
//! and the cursor goes home -- which is where this program's reach has
//! stopped until now.
//!
//! The one arrangement that does reach it is the one Synergy and Input Leap
//! use, and it is the only one, because it follows from who is allowed to do
//! what:
//!
//! * A **service running as LocalSystem** holds `SE_TCB_NAME`, so it may ask
//!   for the interactive session's token and start a process on a desktop of
//!   its choosing. Nothing in the person's session may do that.
//! * The service alone cannot do the work: a service lives in session 0,
//!   which has no screen and no input of its own. It has to put a process
//!   *in the interactive session, attached to a named desktop*, which is
//!   what `CreateProcessAsUser` with `STARTUPINFOW.lpDesktop` is for.
//! * That process -- the **worker** -- does the hooking and the injecting for
//!   whichever desktop it was attached to. It runs as SYSTEM, so nothing on
//!   the secure desktop outranks it, and it can both see input going to the
//!   UAC dialog and put input into it.
//! * The service **watches which desktop has the input** and relaunches the
//!   worker onto the new one when it changes. There is no notification for
//!   this; polling `OpenInputDesktop` a few times a second is what everyone
//!   does, and is what [`watch`] paces.
//!
//! The service keeps the network link and the state machine, exactly as
//! `server.rs` and `client.rs` do today: it is glue around the same
//! `smkvm-core` machines, and the worker is only an arm it reaches the
//! screen with. They talk over a local named pipe.
//!
//! ## Why this is all opt-in
//!
//! A scheduled task running at highest privileges is what three machines are
//! running today and what the person relies on; it is not to be replaced by
//! something new and untested on the strength of a nicer architecture.
//! `smkvm service install` still registers exactly that task. `--system`
//! asks for the service instead, and if the service will not start the
//! daemon is still the daemon: the worker is an alternative way to reach the
//! screen, never a required one.
//!
//! ## The security of it
//!
//! The worker runs at SYSTEM integrity and its whole job is to inject
//! keystrokes. Anything that can tell it what to type owns the machine. So
//! there is exactly one thing that can: the service, over a pipe whose
//! access control list names LocalSystem and nobody else (see [`acl`]). The
//! service in turn takes instructions only from where it always has -- the
//! paired, Noise-authenticated link. No new way in is opened anywhere.
//!
//! That list secures one side of the pipe. The other side is the worker
//! satisfying itself that the pipe it opened is the service's, because any
//! authenticated user may create a name in the pipe namespace and a name
//! created first is the one the worker would reach. The three things that
//! settle it are written down as [`acl::Guard`] and applied in
//! `windows::pipe` and `windows::worker`: a name from the system's random
//! number generator rather than a counter, an open that refuses
//! impersonation, and the worker checking that the process serving the
//! pipe is the system account. Dropping any one of them hands a SYSTEM
//! process to whoever got to the name first.

// The Win32 half that acts on all of this is behind `cfg(windows)`, so in a
// Linux build every decision below is exercised by its tests and called by
// nothing. That is the arrangement on purpose: the judgement lives here where
// it can be tested on whatever machine happens to be building, and the
// platform code is left thin enough to have no judgement left in it.
#![cfg_attr(not(windows), allow(dead_code))]

pub mod acl;
pub mod budget;
pub mod carry;
pub mod outbox;
pub mod plan;
pub mod reach;
pub mod watch;
pub mod wire;

#[cfg(windows)]
pub mod windows;
