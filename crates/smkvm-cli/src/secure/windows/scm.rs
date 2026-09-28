//! The service: registering it, and being it.
//!
//! Two halves that share a name. The first runs from the person's
//! administrator prompt and tells the service control manager about this
//! binary. The second runs as LocalSystem at boot, minds a worker on
//! whichever desktop has the input, and runs the ordinary daemon around it.

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_CALL_NOT_IMPLEMENTED, NO_ERROR};
use windows::Win32::System::Services::{
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_RUNNING, SERVICE_STATUS, SERVICE_STATUS_HANDLE, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

use crate::secure::watch::{self, Seen, Step, Watch};
use crate::secure::windows::{link, pipe, token};
use crate::secure::{acl, wire};

/// What the service is registered as. The scheduled task is `SMKVM`; this
/// is deliberately a different name, so that a machine with a leftover of
/// one and a fresh install of the other is a state the tools can see and
/// describe rather than one thing shadowing another.
pub const SERVICE_NAME: &str = "SMKVMSystem";

/// Is the service registered on this machine?
pub fn installed() -> bool {
    std::process::Command::new("sc.exe")
        .args(["query", SERVICE_NAME])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn sc(arguments: &[&OsStr]) -> Result<String> {
    let out = std::process::Command::new("sc.exe")
        .args(arguments)
        .output()
        .context("running sc.exe")?;
    let said = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !out.status.success() {
        bail!("sc.exe refused: {said}");
    }
    Ok(said)
}

/// Register the service.
///
/// `sc.exe` rather than `CreateServiceW`, for the same reason the scheduled
/// task goes through PowerShell: this runs once, by hand, with a person
/// watching, and what it did is then legible in `sc qc smkvmsystem` --
/// which is where anyone diagnosing it will look anyway.
pub fn install(exe: &Path) -> Result<()> {
    // Quoted, because a path with a space in it that is not quoted is the
    // unquoted-service-path hole, and this service runs as LocalSystem.
    let binary = format!("\"{}\" service-main", exe.display());
    // Every `type=`, `start=` and `obj=` has a space after the equals sign
    // and none before. sc.exe parses the value as a separate argument, and
    // without the space the whole thing fails with a syntax message that
    // does not say which argument it means.
    sc(&[
        OsStr::new("create"),
        OsStr::new(SERVICE_NAME),
        OsStr::new("binPath="),
        OsStr::new(&binary),
        OsStr::new("DisplayName="),
        OsStr::new("SMKVM (system)"),
        OsStr::new("type="),
        OsStr::new("own"),
        OsStr::new("start="),
        OsStr::new("auto"),
        // LocalSystem, and not for grandeur: it is the one account whose
        // token is granted SeTcbPrivilege by default policy, and without
        // that privilege `WTSQueryUserToken` refuses, and without that
        // there is no way to start anything in the person's session.
        OsStr::new("obj="),
        OsStr::new("LocalSystem"),
    ])?;
    sc(&[
        OsStr::new("description"),
        OsStr::new(SERVICE_NAME),
        OsStr::new(
            "Shares one keyboard and mouse across several machines, including the desktop a \
             UAC prompt or the lock screen is on.",
        ),
    ])?;
    // Restarted on failure, the way the task is. A daemon that stopped at
    // three in the morning and stayed stopped is one that is not there when
    // it is wanted.
    sc(&[
        OsStr::new("failure"),
        OsStr::new(SERVICE_NAME),
        OsStr::new("reset="),
        OsStr::new("86400"),
        OsStr::new("actions="),
        OsStr::new("restart/60000/restart/60000/restart/60000"),
    ])?;
    println!(
        "{SERVICE_NAME} will start at boot as LocalSystem and put a worker on whichever \
         desktop has the input, including the one a UAC prompt is on."
    );
    println!("Start it now with `smkvm service start`.");
    Ok(())
}

pub fn uninstall() -> Result<()> {
    let _ = sc(&[OsStr::new("stop"), OsStr::new(SERVICE_NAME)]);
    sc(&[OsStr::new("delete"), OsStr::new(SERVICE_NAME)])?;
    println!("{SERVICE_NAME} no longer starts at boot.");
    Ok(())
}

pub fn start() -> Result<()> {
    sc(&[OsStr::new("start"), OsStr::new(SERVICE_NAME)])
        .context("starting the service; is it installed? `smkvm service install --system`")?;
    println!("started.");
    Ok(())
}

pub fn stop() -> Result<()> {
    sc(&[OsStr::new("stop"), OsStr::new(SERVICE_NAME)])?;
    println!("stopped.");
    Ok(())
}

pub fn describe() -> Result<String> {
    if !installed() {
        return Ok("service: not installed".into());
    }
    let said = sc(&[OsStr::new("qc"), OsStr::new(SERVICE_NAME)])?;
    let account = said
        .lines()
        .find_map(|line| line.trim().strip_prefix("SERVICE_START_NAME"))
        .and_then(|rest| rest.split(':').nth(1))
        .unwrap_or(" ?")
        .trim()
        .to_string();
    let state = sc(&[OsStr::new("query"), OsStr::new(SERVICE_NAME)])?;
    let state = state
        .lines()
        .find_map(|line| line.trim().strip_prefix("STATE"))
        .and_then(|rest| rest.split_whitespace().last())
        .unwrap_or("?")
        .to_string();
    Ok(format!("service: {state}, runs as {account}"))
}

// ---------------------------------------------------------------------------
// Being the service
// ---------------------------------------------------------------------------

/// Set by the control handler, read by the loop that minds the worker.
static STOPPING: AtomicBool = AtomicBool::new(false);

static mut STATUS: Option<SERVICE_STATUS_HANDLE> = None;

/// Hand this process to the service control manager.
///
/// Blocks until the service has stopped; the manager calls
/// [`service_main`] on a thread of its own. It has to be called within
/// thirty seconds of the process starting or the manager gives up on it,
/// which is why `main` does nothing before reaching here but parse its
/// arguments and open its log.
pub fn run_as_service() -> Result<()> {
    let name = HSTRING::from(SERVICE_NAME);
    let table = [
        SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR(name.as_ptr() as *mut u16),
            lpServiceProc: Some(service_main),
        },
        // The manager reads until an all-null entry. Without this it keeps
        // reading past the end of the array.
        SERVICE_TABLE_ENTRYW::default(),
    ];
    // SAFETY: the table outlives the call, which blocks, and ends with the
    // sentinel the manager expects.
    unsafe { StartServiceCtrlDispatcherW(table.as_ptr()) }.context(
        "handing this process to the service control manager. `service-main` is how the \
         manager starts smkvm and is not a command to run by hand",
    )?;
    Ok(())
}

fn report(state: windows::Win32::System::Services::SERVICE_STATUS_CURRENT_STATE, accept: u32) {
    // SAFETY: written once in service_main before any control can arrive,
    // and only read afterwards.
    let Some(handle) = (unsafe { STATUS }) else {
        return;
    };
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accept,
        dwWin32ExitCode: NO_ERROR.0,
        ..Default::default()
    };
    // SAFETY: a handle from RegisterServiceCtrlHandlerExW and a status we own.
    unsafe {
        let _ = SetServiceStatus(handle, &status);
    }
}

unsafe extern "system" fn control(
    code: u32,
    _kind: u32,
    _data: *mut core::ffi::c_void,
    _context: *mut core::ffi::c_void,
) -> u32 {
    match code {
        SERVICE_CONTROL_STOP | SERVICE_CONTROL_SHUTDOWN => {
            STOPPING.store(true, Ordering::SeqCst);
            report(SERVICE_STOP_PENDING, 0);
            NO_ERROR.0
        }
        _ => ERROR_CALL_NOT_IMPLEMENTED.0,
    }
}

unsafe extern "system" fn service_main(_argc: u32, _argv: *mut PWSTR) {
    let name = HSTRING::from(SERVICE_NAME);
    // Registered before anything else, because it is what returns the
    // handle every status report needs, and a report before it would be
    // setting some other service's status.
    let Ok(handle) = RegisterServiceCtrlHandlerExW(PCWSTR(name.as_ptr()), Some(control), None)
    else {
        return;
    };
    STATUS = Some(handle);
    report(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
    );

    if let Err(e) = serve() {
        tracing::error!("{e:#}");
    }

    report(SERVICE_STOPPED, 0);
    // Reported stopped exactly once, and then nothing: the first stop
    // report closes the manager's handle and a second can take the process
    // down with it. Exiting here is also what releases the worker -- the
    // pipe closes with the process, and the worker exits when it does, so
    // no SYSTEM process is left on the logon desktop.
    std::process::exit(0);
}

/// Everything the service does, once it is a service.
fn serve() -> Result<()> {
    token::enable_tcb_privilege()?;
    tracing::info!(
        "running as a service; workers will be started on whichever desktop has the input, \
         so a UAC prompt and the lock screen are reachable"
    );

    let link = link::Link::new();
    link::use_worker(link.clone());

    let exe = std::env::current_exe().context("finding this program's own path")?;
    let minding = {
        let link = link.clone();
        std::thread::Builder::new()
            .name("smkvm-desktops".into())
            .spawn(move || mind_workers(&exe, link))
            .context("starting the thread that minds the worker")?
    };

    // From here it is the ordinary daemon, with the arm above standing in
    // for `SendInput`. Nothing in `run` knows a service is running it.
    let outcome = crate::run_daemon_for_service();
    STOPPING.store(true, Ordering::SeqCst);
    let _ = minding.join();
    outcome
}

/// Poll the input desktop and keep a worker on it.
fn mind_workers(exe: &Path, link: Arc<link::Link>) {
    let mut watch = Watch::new();
    let mut running: Option<token::Started> = None;
    let mut run = 0u64;

    while !STOPPING.load(Ordering::SeqCst) {
        std::thread::sleep(watch::LOOK_EVERY);

        if let Some(started) = &running {
            if started.gone() {
                tracing::warn!("the worker exited; another will be started");
                link.detach();
                watch.worker_gone();
                running = None;
            }
        }

        let seen = match smkvm_input::platform::windows::desktop::current() {
            smkvm_input::platform::windows::desktop::InputDesktop::Ours => {
                match smkvm_input::platform::windows::desktop::ours() {
                    Some(name) => Seen::Desktop(name),
                    None => Seen::Unreadable,
                }
            }
            smkvm_input::platform::windows::desktop::InputDesktop::Elsewhere(name) => {
                Seen::Desktop(name)
            }
            smkvm_input::platform::windows::desktop::InputDesktop::OutOfReach => Seen::Unreadable,
        };

        match watch.saw(seen) {
            Step::Stay => {}
            Step::GiveUp { desktop, tries } => tracing::error!(
                desktop,
                tries,
                "a worker cannot be started on this desktop, so input will not reach it. \
                 The cursor is handed back while it has the input, as it was before there \
                 was a service at all"
            ),
            Step::Move { desktop, on } => {
                // The one leaving is told to let go before it is replaced,
                // so nothing is left held down on the desktop it was on.
                if let Some(old) = running.take() {
                    link.say(&wire::ToWorker::Stop);
                    link.detach();
                    old.kill();
                }
                run += 1;
                match start_worker(exe, &on, run, &link) {
                    Ok((started, landed)) => {
                        tracing::info!(
                            desktop = %landed,
                            pid = started.pid,
                            was = watch.worker_on().unwrap_or("nothing"),
                            "a worker is on the input desktop"
                        );
                        watch.worker_started(&landed);
                        running = Some(started);
                    }
                    Err(e) => {
                        tracing::warn!("could not put a worker on {desktop}: {e:#}");
                        watch.worker_failed(&desktop);
                    }
                }
            }
        }
    }

    if let Some(old) = running {
        link.say(&wire::ToWorker::Stop);
        link.detach();
        old.kill();
    }
}

/// Make the pipe, start the worker on `on`, and wait for it to say hello.
fn start_worker(
    exe: &Path,
    on: &str,
    run: u64,
    link: &Arc<link::Link>,
) -> Result<(token::Started, String)> {
    let session = token::console_session().context("nobody is logged in at the screen yet")?;
    let user = token::session_token(session)?;
    let name = acl::pipe_name(run);

    // The pipe is made before the worker is started, so there is no moment
    // in which the name exists and this process is not the one holding it.
    // `FILE_FLAG_FIRST_PIPE_INSTANCE` then makes a name somebody else got
    // to first a refusal rather than a conversation with them.
    let listening = std::thread::Builder::new()
        .name("smkvm-pipe".into())
        .spawn({
            let name = name.clone();
            move || pipe::serve(&name)
        })
        .context("starting the thread that waits for the worker")?;

    let started =
        token::start_on_desktop(&user, exe, &format!("desktop-worker --pipe {name}"), on)?;

    let served = listening
        .join()
        .map_err(|_| anyhow::anyhow!("the thread waiting for the worker died"))??;
    let mut reading = served.share()?;
    link.attach(served);

    // The first frame says which desktop the worker actually landed on and
    // that it is the same build. Anything else and it is not talked to.
    let hello: wire::FromWorker =
        wire::read_frame(&mut reading).context("the worker said nothing")?;
    let landed = match hello {
        wire::FromWorker::Ready { protocol, desktop } if protocol == wire::WORKER_PROTOCOL => {
            desktop
        }
        wire::FromWorker::Ready { protocol, .. } => {
            link.detach();
            started.kill();
            bail!(
                "the worker speaks protocol {protocol} and this speaks {}: two halves of one \
                 installation are different builds. Reinstall from one binary",
                wire::WORKER_PROTOCOL
            );
        }
        other => {
            link.detach();
            started.kill();
            bail!("the worker said {other:?} before saying hello");
        }
    };

    let link = link.clone();
    std::thread::Builder::new()
        .name("smkvm-worker-reader".into())
        .spawn(move || {
            while let Ok(said) = wire::read_frame::<wire::FromWorker, _>(&mut reading) {
                link.heard(said);
            }
        })
        .context("starting the thread that reads from the worker")?;

    Ok((started, landed))
}
