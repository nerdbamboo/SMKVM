//! The arm on the desktop.
//!
//! Started by the service, as SYSTEM, in the interactive session, already
//! attached to a named desktop by `STARTUPINFOW.lpDesktop`. It hooks and
//! injects there and says nothing to anybody but the pipe it was told to
//! open. It holds no configuration, reads no file, listens on no socket and
//! has no opinion about where the cursor should be: everything it does, it
//! was told to do by the service one frame ago.
//!
//! That is not tidiness. This process can type anything into a UAC consent
//! prompt. The smaller the set of things that can make it type, the shorter
//! the argument that it is safe, and the set here is one pipe that admits
//! LocalSystem alone.
//!
//! It exits when the pipe closes. So a service that dies takes its workers
//! with it rather than leaving a SYSTEM process on the logon desktop
//! waiting for whoever comes along next.

#![allow(unsafe_code)]

use anyhow::{Context, Result};
use smkvm_input::platform::windows::capture::{Capture, Captured};
use smkvm_input::platform::windows::{desktop, WindowsInput};
use smkvm_input::{Inject, Monitors};

use crate::secure::acl;

use crate::secure::windows::pipe;
use crate::secure::wire::{frame, read_frame, FromWorker, Saw, ToWorker, WORKER_PROTOCOL};

/// Run as the worker until the pipe closes.
pub fn run(pipe_name: &str) -> Result<()> {
    // `connect` applies every one of these before a byte is sent, and
    // refuses rather than returning if any of them does not hold. Naming
    // them here rather than in a comment means removing one is a change
    // somebody has to make to code that is compiled.
    let mut to_service = pipe::connect(pipe_name).with_context(|| {
        format!(
            "reaching the service. The pipe must satisfy all of {:?} before this process, \
             which runs as the system account and injects keystrokes, says anything to it",
            acl::Guard::ALL
        )
    })?;
    let mut from_service = to_service.share()?;

    // Which desktop this actually is, asked rather than assumed. The
    // service decided some milliseconds ago and the input may have moved
    // since; it is the service's business to know, and only this process
    // can see from here.
    let here = match desktop::current() {
        desktop::InputDesktop::Elsewhere(name) => name,
        // Either the input desktop is this one, or it would not say --
        // and this process's own desktop is the answer to the question
        // being asked either way, which is where *it* is.
        desktop::InputDesktop::Ours | desktop::InputDesktop::OutOfReach => desktop::ours()
            .context(
                "this worker cannot read the name of the desktop it is on. It is not \
                 started rather than reported as being somewhere unnameable: the service \
                 compares that name against the desktop with the input, and a name that \
                 can never match makes it replace this process on every look",
            )?,
    };
    say(
        &mut to_service,
        &FromWorker::Ready {
            protocol: WORKER_PROTOCOL,
            desktop: here.clone(),
        },
    )?;
    tracing::info!(desktop = %here, "the worker is on the desktop and connected");

    // The capture hooks go on their own thread with a message loop, which
    // is what `Capture::start` arranges. It inherits this process's desktop,
    // which is the one the service attached us to -- the whole point.
    let capture = match Capture::start() {
        Ok((capture, seen)) => {
            let mut back = to_service.share()?;
            std::thread::Builder::new()
                .name("smkvm-worker-capture".into())
                .spawn(move || {
                    while let Ok(event) = seen.recv() {
                        if say(&mut back, &FromWorker::Saw(mirror(event))).is_err() {
                            return;
                        }
                    }
                })
                .context("starting the worker's capture pump")?;
            Some(capture)
        }
        Err(e) => {
            // A desktop that will not take hooks is still a desktop that
            // takes injection, which is the half that matters on a consent
            // prompt: what is typed there is typed from the other machine.
            tracing::warn!("the worker cannot watch this desktop's input: {e}");
            None
        }
    };

    let mut input = WindowsInput::new();
    loop {
        let told: ToWorker = match read_frame(&mut from_service) {
            Ok(told) => told,
            // The service has gone, or said something this build does not
            // understand. Either way this process stops rather than
            // guessing: a wrong guess here types into a consent prompt.
            Err(e) => {
                tracing::info!("the worker is done: {e}");
                break;
            }
        };
        let outcome = match told {
            ToWorker::MoveTo { x, y } => input.move_to(x, y),
            ToWorker::Button { button, down } => input.button(button, down),
            ToWorker::Wheel(scroll) => input.wheel(scroll),
            ToWorker::Key { key, down } => input.key(key, down),
            ToWorker::Flush => input.flush(),
            ToWorker::HideCursor => input.hide_cursor(),
            ToWorker::ShowCursor => input.show_cursor(),
            ToWorker::TellMonitors => match input.monitors() {
                Ok(monitors) => {
                    say(&mut to_service, &FromWorker::Monitors(monitors))?;
                    Ok(())
                }
                Err(e) => Err(e),
            },
            ToWorker::Swallow(swallow) => {
                if let Some(capture) = &capture {
                    capture.set_swallow(swallow);
                }
                Ok(())
            }
            ToWorker::Stop => break,
        };
        if let Err(e) = outcome {
            // Reported rather than fatal. The service already knows what to
            // do with a refused injection, and it is the same thing the
            // daemon does with one of its own.
            say(&mut to_service, &FromWorker::Refused(e.to_string()))?;
        }
    }

    // Nothing may be left held down on a desktop this process is leaving,
    // and nothing else will run after this to let go: the worker is
    // replaced by another process, not resumed.
    let _ = input.show_cursor();
    for key in smkvm_proto::Key::MODIFIERS {
        let _ = input.key(key, false);
    }
    for button in [
        smkvm_proto::MouseButton::Left,
        smkvm_proto::MouseButton::Middle,
        smkvm_proto::MouseButton::Right,
    ] {
        let _ = input.button(button, false);
    }
    let _ = input.flush();
    Ok(())
}

fn say<W: std::io::Write>(to: &mut W, message: &FromWorker) -> Result<()> {
    let bytes = frame(message)?;
    to.write_all(&bytes).context("writing to the service")?;
    Ok(())
}

fn mirror(event: Captured) -> Saw {
    match event {
        Captured::PointerAt { x, y } => Saw::PointerAt { x, y },
        Captured::PointerBy { dx, dy } => Saw::PointerBy { dx, dy },
        Captured::Button { button, down } => Saw::Button { button, down },
        Captured::Wheel(scroll) => Saw::Wheel(scroll),
        Captured::Key { key, down, repeat } => Saw::Key { key, down, repeat },
    }
}
