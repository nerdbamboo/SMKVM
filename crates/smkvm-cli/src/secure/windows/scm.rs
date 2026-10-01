//! The service: registering it, and being it.
//!
//! Two halves that share a name. The first runs from the person's
//! administrator prompt and tells the service control manager about this
//! binary. The second runs as LocalSystem at boot, minds a worker on
//! whichever desktop has the input, and runs the ordinary daemon around it.

#![allow(unsafe_code)]

use std::ffi::OsStr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_CALL_NOT_IMPLEMENTED, ERROR_SERVICE_SPECIFIC_ERROR, NO_ERROR,
};
use windows::Win32::System::Services::{
    CloseServiceHandle, OpenSCManagerW, OpenServiceW, QueryServiceConfigW, QueryServiceStatusEx,
    RegisterServiceCtrlHandlerExW, SetServiceStatus, StartServiceCtrlDispatcherW,
    QUERY_SERVICE_CONFIGW, SC_HANDLE, SC_MANAGER_CONNECT, SC_STATUS_PROCESS_INFO,
    SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_CONTROL_SHUTDOWN, SERVICE_CONTROL_STOP,
    SERVICE_QUERY_CONFIG, SERVICE_QUERY_STATUS, SERVICE_RUNNING, SERVICE_STATUS,
    SERVICE_STATUS_CURRENT_STATE, SERVICE_STATUS_HANDLE, SERVICE_STATUS_PROCESS, SERVICE_STOPPED,
    SERVICE_STOP_PENDING, SERVICE_TABLE_ENTRYW, SERVICE_WIN32_OWN_PROCESS,
};

use crate::secure::watch::{self, Seen, Step, Watch};
use crate::secure::windows::{link, pipe, readers, secret, store, token, Aligned};
use crate::secure::{acl, wire};

/// What the service is registered as. The scheduled task is `SMKVM`; this
/// is deliberately a different name, so that a machine with a leftover of
/// one and a fresh install of the other is a state the tools can see and
/// describe rather than one thing shadowing another.
pub const SERVICE_NAME: &str = "SMKVMSystem";

/// How long the manager is told to keep waiting while the service winds
/// down, and how often it is told again.
///
/// A stop report with no wait hint is a service the manager has no reason
/// to wait for, so it is treated as hung and killed -- which is what
/// happened here, and it is why stopping produced a timeout rather than a
/// stop.
const WIND_DOWN_HINT_MS: u32 = 8_000;

/// The service-specific code reported when the service could not do its
/// job. There is only one, because the distinctions are in the log and
/// nothing reads this but the manager, which only needs it to be
/// non-zero.
const FAILED_TO_SERVE: u32 = 1;

/// Is the service registered on this machine?
pub fn installed() -> bool {
    open_service(SERVICE_QUERY_STATUS).is_ok()
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
/// which is where anyone diagnosing it will look anyway. Reading the
/// service back does *not* go through `sc.exe`; see [`describe`].
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
        // that privilege the worker's token cannot be moved into the
        // session with the screen. It is also the account the Winlogon
        // desktop's own access list admits, which is the whole reason this
        // arrangement reaches a consent prompt at all.
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

// ---------------------------------------------------------------------------
// Reading the service back
// ---------------------------------------------------------------------------

/// A service control manager handle that is closed when it goes out of scope.
struct Service(SC_HANDLE);

impl Drop for Service {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: the handle is this value's own and came from the
            // manager, which says to close it this way.
            unsafe {
                let _ = CloseServiceHandle(self.0);
            }
        }
    }
}

fn open_service(access: u32) -> Result<(Service, Service)> {
    // SAFETY: null names mean this machine and the active database.
    let manager = unsafe { OpenSCManagerW(None, None, SC_MANAGER_CONNECT) }
        .context("opening the service control manager")?;
    let manager = Service(manager);
    // SAFETY: a valid manager handle and a null-terminated name.
    let service = unsafe { OpenServiceW(manager.0, &HSTRING::from(SERVICE_NAME), access) }
        .with_context(|| format!("opening the {SERVICE_NAME} service"))?;
    Ok((manager, Service(service)))
}

/// Say what is registered and how it is doing.
///
/// Asked of the manager rather than scraped out of `sc.exe`'s printing.
/// `sc.exe` is translated: on a Korean or Japanese Windows the lines do
/// not begin with `STATE` or `SERVICE_START_NAME`, so the scraping this
/// replaced fell through to a question mark on exactly the machines this
/// is deployed on -- in the status line somebody reads when they are
/// already trying to work out why nothing works.
pub fn describe() -> Result<String> {
    let Ok((_manager, service)) = open_service(SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG) else {
        return Ok("service: not installed".into());
    };

    let mut needed = 0u32;
    // SAFETY: asking for the size writes only to `needed`; the call fails
    // by design, which is how the size is learned.
    let _ = unsafe { QueryServiceConfigW(service.0, None, 0, &mut needed) };
    // Aligned, because a `QUERY_SERVICE_CONFIGW` is read back out of it
    // and it is full of pointers; a `Vec<u8>` is aligned to one byte, and
    // taking a reference to a structure inside one is undefined however
    // well the hardware copes.
    let mut buffer = Aligned::new(needed.max(8) as usize);
    // SAFETY: the buffer is at least `needed` bytes and aligned for the
    // structure the call writes into it, whose fields point into it.
    let account = unsafe {
        QueryServiceConfigW(
            service.0,
            Some(buffer.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW),
            buffer.len() as u32,
            &mut needed,
        )
        .ok()
        .and_then(|()| {
            let config = &*(buffer.as_ptr() as *const QUERY_SERVICE_CONFIGW);
            (!config.lpServiceStartName.is_null())
                .then(|| config.lpServiceStartName.to_string().ok())
                .flatten()
        })
    }
    .unwrap_or_else(|| "?".into());

    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0u32;
    // SAFETY: a buffer of exactly the size the call is told.
    let state = unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            Some(std::slice::from_raw_parts_mut(
                &mut status as *mut _ as *mut u8,
                std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
            )),
            &mut needed,
        )
    }
    .map(|()| match status.dwCurrentState {
        SERVICE_RUNNING => "running",
        SERVICE_STOPPED => "stopped",
        SERVICE_STOP_PENDING => "stopping",
        _ => "changing state",
    })
    .unwrap_or("?");

    Ok(format!("service: {state}, runs as {account}"))
}

/// Is the service registered *and* running?
///
/// Asked so that `smkvm service stop` does not give up on a daemon
/// somebody started by hand merely because a stopped service is also
/// registered.
pub fn running() -> bool {
    let Ok((_manager, service)) = open_service(SERVICE_QUERY_STATUS) else {
        return false;
    };
    let mut status = SERVICE_STATUS_PROCESS::default();
    let mut needed = 0u32;
    // SAFETY: a buffer of exactly the size the call is told.
    unsafe {
        QueryServiceStatusEx(
            service.0,
            SC_STATUS_PROCESS_INFO,
            Some(std::slice::from_raw_parts_mut(
                &mut status as *mut _ as *mut u8,
                std::mem::size_of::<SERVICE_STATUS_PROCESS>(),
            )),
            &mut needed,
        )
    }
    .is_ok()
        && status.dwCurrentState != SERVICE_STOPPED
}

// ---------------------------------------------------------------------------
// Being the service
// ---------------------------------------------------------------------------

/// Set by the control handler, read by the loop that minds the worker.
static STOPPING: AtomicBool = AtomicBool::new(false);

/// The manager's handle for reporting status.
///
/// An atomic rather than a `static mut`: it is written on the thread the
/// manager calls `service_main` on and read on whichever thread the
/// manager delivers a control to, which is a different one. The ordering
/// argument for the old `static mut` was true and is not the point -- two
/// threads touching a plain static without synchronisation is undefined
/// whatever the ordering happens to be.
static STATUS: AtomicIsize = AtomicIsize::new(0);

/// How many times the wind-down has reported progress, which the manager
/// uses to tell a service that is still going from one that has stopped
/// answering.
static CHECKPOINT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

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

fn report(state: SERVICE_STATUS_CURRENT_STATE, accept: u32, wait_hint_ms: u32) {
    report_with(state, accept, wait_hint_ms, NO_ERROR.0, 0);
}

/// Report, saying how it went.
///
/// The exit code is the whole of finding 2. `report` used to hard-code
/// `NO_ERROR` and `service_main` used to `exit(0)` whatever had happened,
/// so a service that could not start at all -- registered under the wrong
/// account, unable to find its own path, a thread that would not spawn --
/// looked to the manager exactly like one that was asked to stop and did.
/// `sc start` said success and the status said "stopped", with the reason
/// only in the log; and the `restart/60000` failure actions registered at
/// install were inert, because the manager applies those only to a
/// service that terminates with a code. The case that comment worries
/// about -- stopped at three in the morning and staying stopped -- was
/// exactly the case not covered.
fn report_with(
    state: SERVICE_STATUS_CURRENT_STATE,
    accept: u32,
    wait_hint_ms: u32,
    win32_code: u32,
    specific_code: u32,
) {
    let handle = STATUS.load(Ordering::Acquire);
    if handle == 0 {
        return;
    }
    let status = SERVICE_STATUS {
        dwServiceType: SERVICE_WIN32_OWN_PROCESS,
        dwCurrentState: state,
        dwControlsAccepted: accept,
        dwWin32ExitCode: win32_code,
        dwServiceSpecificExitCode: specific_code,
        dwCheckPoint: if wait_hint_ms == 0 {
            0
        } else {
            CHECKPOINT.fetch_add(1, Ordering::Relaxed) + 1
        },
        dwWaitHint: wait_hint_ms,
    };
    // SAFETY: the handle came from RegisterServiceCtrlHandlerExW on this
    // process's own service name, and the status is ours.
    unsafe {
        let _ = SetServiceStatus(
            SERVICE_STATUS_HANDLE(handle as *mut core::ffi::c_void),
            &status,
        );
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
            // Told to stop *and* told the daemon to stop. The flag alone
            // reaches only the thread minding the worker; the daemon is
            // inside its own loop, and without this the service sat there
            // until the manager's patience ran out and killed it.
            crate::ask_to_stop();
            report(SERVICE_STOP_PENDING, 0, WIND_DOWN_HINT_MS);
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
    STATUS.store(handle.0 as isize, Ordering::Release);
    report(
        SERVICE_RUNNING,
        SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN,
        0,
    );

    let failed = match serve() {
        Ok(()) => false,
        Err(e) => {
            tracing::error!("{e:#}");
            true
        }
    };

    if failed {
        // `ERROR_SERVICE_SPECIFIC_ERROR` with a specific code is how a
        // service says "I failed for a reason of my own" rather than
        // borrowing a system error number that would be read as
        // something it is not. What it failed at is in the log; what the
        // manager needs is only that it failed, so that the restart
        // actions apply.
        report_with(
            SERVICE_STOPPED,
            0,
            0,
            ERROR_SERVICE_SPECIFIC_ERROR.0,
            FAILED_TO_SERVE,
        );
    } else {
        report(SERVICE_STOPPED, 0, 0);
    }
    // Reported stopped exactly once, and then nothing: the first stop
    // report closes the manager's handle and a second can take the process
    // down with it. Exiting here is also what releases the worker -- the
    // pipe closes with the process, and the worker exits when it does, so
    // no process running as the system account is left on the logon
    // desktop.
    //
    // Non-zero when it failed, for the same reason as the status above:
    // the manager decides whether to apply the restart actions by the
    // process's exit code as well.
    std::process::exit(if failed { 1 } else { 0 });
}

/// Everything the service does, once it is a service.
fn serve() -> Result<()> {
    // The scope was set in `main`, before the log was opened, so that
    // the log lands machine-wide with everything else. Said here because
    // this is where somebody reading the log starts.
    //
    // Why there is a scope at all: this process runs as the system
    // account, whose `APPDATA` is the system profile -- a real directory
    // that resolves perfectly well and has none of this machine's files
    // in it. On the first real installation the daemon looked there,
    // found no configuration, and exited.
    debug_assert_eq!(
        smkvm_config::paths::scope(),
        smkvm_config::paths::Scope::Machine
    );
    tracing::info!(
        configuration = %smkvm_config::paths::config_file().display(),
        "reading this machine's files from the machine-wide directory, not a profile"
    );

    // Before the key is used for anything, and before the network link
    // exists. The install sets the access lists and confirms them, but
    // the install is not the only way a file gets into that directory:
    // an earlier version of this program left one there readable, and a
    // key generated later by this very process inherits whatever the
    // directory says. Somebody diagnosing this machine will check with
    // `type %ProgramData%\smkvm\device.toml` from an ordinary account;
    // this is the same check, made by the program, before there is
    // anything worth reading.
    //
    // A key that anyone can read is refused rather than warned about,
    // because carrying on means handshaking with a secret that is not
    // one. A list that cannot be *read* is only a warning: failing to
    // parse is not evidence of a fault -- this build's parser has been
    // wrong four separate ways -- and refusing to start over the
    // parser's own ignorance is the same shape as the outage this
    // hardware round began with. The log says `KEY PRIVATE`,
    // `KEY READABLE` or `KEY UNPROVEN`, so which of the three it was is
    // one glance rather than a reading.
    let key = smkvm_config::paths::identity_file();
    if key.exists() {
        store::check_key_before_using_it(&key)?;
    }
    token::enable_tcb_privilege()?;
    tracing::info!(
        "running as a service; workers will be started as the system account on whichever \
         desktop has the input, so a UAC prompt and the lock screen are reachable"
    );

    // Before the daemon starts, because `Sharing` resolves
    // `transfer.directory` once when it is built. Not fatal if it
    // cannot be found: files then land where they did before, which is
    // somewhere unhelpful rather than somewhere wrong, and the line
    // above says so.
    match token::console_session().map(token::home_in_session) {
        Some(Ok(home)) => {
            tracing::info!(home = %home.display(), "files sent to this machine will land under the person's own profile");
            smkvm_config::paths::use_home(home);
        }
        Some(Err(e)) => tracing::warn!(
            "could not find out where the person at the screen keeps their files, so \
             anything sent to this machine will land in the system profile where they \
             cannot see it: {e:#}"
        ),
        None => {}
    }

    let link = link::Link::new();
    link::use_worker(link.clone());

    let exe = std::env::current_exe().context("finding this program's own path")?;
    {
        let exe = exe.clone();
        std::thread::Builder::new()
            .name("smkvm-readers".into())
            .spawn(move || mind_readers(&exe))
            .context("starting the thread that minds the reader")?;
    }
    let minding = {
        let link = link.clone();
        std::thread::Builder::new()
            .name("smkvm-desktops".into())
            .spawn(move || mind_workers(&exe, link))
            .context("starting the thread that minds the worker")?
    };

    // From here it is the ordinary daemon, with the arm above standing in
    // for `SendInput`. Nothing in `run` knows a service is running it,
    // except that it now stops when asked as well as on Ctrl+C.
    let outcome = crate::run_daemon_for_service();
    STOPPING.store(true, Ordering::SeqCst);
    // Every wait inside this thread is bounded, so joining it is bounded
    // too. That was not true of the first draft, where it could not
    // return at all and the service could not be stopped.
    let _ = minding.join();
    outcome
}

/// Poll what the worker has told us, and keep a worker where the input is.
///
/// The service does not look at the input desktop itself, and cannot:
/// `OpenInputDesktop` is per window station and a service is in session
/// 0 on `Service-0x0-3e7$`, which is not where the screens are. The
/// first version did look, from out there, got nothing every time, and
/// treated nothing as "no news" -- so it held still for ever and never
/// started a worker, with not one line in the log to say so. That is
/// this loop's whole history and the reason it is shaped this way now:
/// a worker goes into the session first, on the desktop that always
/// exists, and from then on the worker is the one that can see.
/// How long to wait before trying to start a reader again after one
/// could not be started.
const READER_RETRY_AFTER: std::time::Duration = std::time::Duration::from_secs(5);

/// How many times a reader may fail to start for one session before
/// this stops trying.
///
/// There is no point retrying for ever. The failure that matters is
/// somebody else on the desktop winning the race to the pipe:
/// `client_is` catches it and nothing is read from the impostor, but
/// retrying every five seconds until logoff costs a warning and a
/// process started as the system account each time, and the rest of
/// this codebase does not leave unbounded loops. `watch` has
/// `GIVE_UP_AFTER` for the same shape.
///
/// Giving up is not silent, and what it costs is small and
/// recoverable: the clipboard stops going out from this machine
/// until the person logs out and in, and nothing else changes.
const READER_TRIES: u32 = 5;

/// Keep a reader running as whoever is at the screen, and nothing
/// running when nobody is.
///
/// Separate from minding the worker, because the two follow different
/// things. A worker follows the *desktop*, and is replaced whenever
/// the input moves to another one -- several times a minute while
/// somebody dismisses consent prompts. A reader follows the
/// *session*, which changes when a person logs on or off, and it has
/// no business on the desktop a consent prompt is on: there is
/// nothing to copy there.
///
/// Absence is the ordinary state here, not a failure. Between logoff
/// and logon there is nobody to be, so there is no reader, and the
/// service goes on running with its link and its reach into the
/// consent desktop intact. What it loses is the ability to say what
/// this machine has copied, because this machine has nobody to copy.
fn mind_readers(exe: &Path) {
    let mut serving: Option<u32> = None;
    // Latched, so that a machine sitting at the logon screen does not
    // write the same line four times a minute until somebody arrives.
    let mut said_nobody = false;
    // When to try again after a start that failed. A reader that
    // cannot be started is worth retrying -- the session may be
    // mid-logon -- but not four times a second.
    let mut not_before = Instant::now();
    // Counted per session, so logging out and in again is a fresh
    // start rather than a machine that has given up for ever.
    let mut failures = 0u32;
    let mut given_up_on: Option<u32> = None;

    while !STOPPING.load(Ordering::SeqCst) {
        std::thread::sleep(watch::LOOK_EVERY);

        let session = token::console_session();
        let here = readers::reader();

        match (session, &serving) {
            // Nobody at the screen. If a reader was running for
            // somebody who has gone, it goes with them.
            (None, _) => {
                if serving.take().is_some() {
                    readers::let_go("nobody is at the screen any more");
                }
                given_up_on = None;
                failures = 0;
                if !said_nobody {
                    tracing::info!(
                        "clipboard: nobody is logged in, so there is nothing to read here. \
                         Input is unaffected"
                    );
                    said_nobody = true;
                }
            }
            // Somebody is there and a reader is running for them.
            (Some(now), Some(was)) if now == *was && here.is_some() => {}
            // Somebody is there and either nothing is running, or
            // what is running belongs to a session that has gone.
            (Some(now), _) => {
                said_nobody = false;
                if serving.is_some() && serving != Some(now) {
                    readers::let_go("somebody else is at the screen now");
                    serving = None;
                }
                // A different session is a different question, so
                // whatever was given up on does not count against it.
                if given_up_on.is_some_and(|was| was != now) {
                    given_up_on = None;
                    failures = 0;
                }
                if given_up_on == Some(now) || Instant::now() < not_before {
                    continue;
                }
                match readers::start(exe, now) {
                    Ok(_) => {
                        serving = Some(now);
                        failures = 0;
                    }
                    Err(e) => {
                        failures += 1;
                        not_before = Instant::now() + READER_RETRY_AFTER;
                        if failures >= READER_TRIES {
                            given_up_on = Some(now);
                            tracing::warn!(
                                "clipboard: a reader could not be started in session \
                                 {now} after {failures} tries, the last because {e:#}. \
                                 Not trying again until somebody logs in again. What \
                                 this machine copies will not reach another until then; \
                                 the keyboard, the mouse and what other machines copy \
                                 are all unaffected"
                            );
                        } else {
                            tracing::warn!(
                                "clipboard: could not start a reader in session {now} \
                                 (try {failures} of {READER_TRIES}): {e:#}"
                            );
                        }
                    }
                }
            }
        }
    }
    readers::let_go("the service is stopping");
}

fn mind_workers(exe: &Path, link: Arc<link::Link>) {
    let mut watch = Watch::new();
    let mut running: Option<token::Started> = None;
    // Whether "nobody is logged in yet" has already been said once.
    let mut said_no_session = false;
    // And whether "nothing has told me where the input is" has. Latched
    // the same way, because a loop that silently does nothing is the
    // same failure as the silent refused injection this project started
    // with, and it has now cost a hardware round.
    let mut said_nobody_is_watching = false;

    while !STOPPING.load(Ordering::SeqCst) {
        std::thread::sleep(watch::LOOK_EVERY);

        if let Some(started) = &running {
            if started.gone() {
                tracing::warn!("the worker exited; another will be started");
                link.detach();
                // Timed, because a worker that says hello and dies at once
                // has to count against the desktop it was on. Without
                // that this loop starts a process as the system account
                // four times a second for the rest of the day.
                watch.worker_gone(Instant::now());
                running = None;
            }
        }

        // Only now, once what is true has been recorded. This check used
        // to sit at the top of the loop and `continue` past the liveness
        // check above, which meant that logging off while a worker was
        // running left `Reach` holding a worker and a matching desktop
        // for a screen that no longer had a session on it -- claiming
        // input was landing, and only unstuck when the next injection
        // timed out a second later. A stale reach claiming reach is the
        // whole subject of this module, so the wait goes after the
        // observing, not before it.
        if token::console_session().is_none() {
            if !said_no_session {
                tracing::info!(
                    "nobody is logged in at the screen yet, so there is no session to put \
                     a worker in. Waiting; this is not a failure"
                );
                said_no_session = true;
            }
            link.detach();
            continue;
        }
        if said_no_session {
            tracing::info!("a session is at the screen now");
            said_no_session = false;
        }

        let seen = match link.input_is_on() {
            Some(name) => {
                if said_nobody_is_watching {
                    tracing::info!(desktop = %name, "a worker is reporting where the input is");
                    said_nobody_is_watching = false;
                }
                Seen::Desktop(name)
            }
            None => {
                if !said_nobody_is_watching {
                    tracing::info!(
                        "nothing has said which desktop has the input yet. Only something \
                         inside the session can tell -- a service cannot see session 1's \
                         window station from session 0 -- so a worker is being put on the \
                         desktop that always exists, and it will say"
                    );
                    said_nobody_is_watching = true;
                }
                Seen::Unreadable
            }
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
                    tracing::info!(
                        leaving = watch.worker_on().unwrap_or("?"),
                        for_ = %desktop,
                        "the input moved, so the worker is replaced"
                    );
                    link.say(&wire::ToWorker::Stop);
                    link.detach();
                    old.kill();
                }
                match start_worker(exe, &on, &link) {
                    Ok((started, landed)) => {
                        tracing::info!(
                            desktop = %landed,
                            pid = started.pid,
                            "a worker is on the input desktop"
                        );
                        watch.worker_started(&landed, &desktop, Instant::now());
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

    // Said again on the way out, so the manager sees progress rather than
    // a service that reported STOP_PENDING once and went quiet. A rising
    // check point is the difference between "still winding down" and
    // "hung", and the manager kills the second.
    report(SERVICE_STOP_PENDING, 0, WIND_DOWN_HINT_MS);
    if let Some(old) = running {
        link.say(&wire::ToWorker::Stop);
        link.detach();
        old.kill();
    }
}

/// Make the pipe, start the worker on `on`, and wait for it to say hello.
///
/// The order is the security of it. The pipe is created and *proved to
/// exist and be ours* before the worker is started, because any
/// authenticated user may create a name in the pipe namespace: a worker
/// started first and then told a name could reach somebody else's pipe of
/// that name. The first draft spawned the making of the pipe onto a thread
/// and started the worker without waiting to see whether it had worked,
/// under a comment claiming exactly the property it did not have.
fn start_worker(exe: &Path, on: &str, link: &Arc<link::Link>) -> Result<(token::Started, String)> {
    let session = token::console_session().context("nobody is logged in at the screen yet")?;
    let user = token::system_token_in_session(session)?;
    // From the system's random number generator. A counter, which this
    // was, gives a name anything on the machine can create first.
    let name = acl::pipe_name(&secret::name_bytes()?);

    let listening = pipe::create(&name)?;

    // `--unattended` so the worker never decides where to log by asking
    // whether stderr looks like a terminal. A process started with no
    // console has no stderr worth the question, and this program has
    // twice lost a deployment to a log that went somewhere nobody could
    // read. The scheduled task learnt that; the worker should not have
    // to learn it again.
    let started = token::start_on_desktop(
        &user,
        exe,
        &format!("desktop-worker --unattended --pipe {name}"),
        on,
    )?;

    // Bounded. A worker that never arrives is a counted failure that
    // `watch` can give up on, not a thread parked for ever.
    if let Err(e) = pipe::accept(&listening, pipe::CONNECT_WITHIN) {
        started.kill();
        return Err(e);
    }

    let mut reading = match listening.share() {
        Ok(reading) => reading,
        Err(e) => {
            // Killed here as on every other early return. Dropping the
            // pipe would collect it too, since the worker exits when its
            // read fails, but relying on that makes this function's
            // cleanup depend on the worker's behaviour rather than on
            // this function.
            started.kill();
            return Err(e);
        }
    };

    // The hello is read and judged before the worker is spoken to at all:
    // `link.attach` sends an instruction, and sending one to a process
    // whose build has not been established is the thing the comment here
    // used to claim was not happening.
    let hello: wire::FromWorker = match wire::read_frame(&mut reading) {
        Ok(hello) => hello,
        Err(e) => {
            started.kill();
            return Err(anyhow::anyhow!("the worker said nothing: {e}"));
        }
    };
    let landed = match wire::welcome(hello) {
        Ok(landed) => landed,
        Err(e) => {
            started.kill();
            return Err(e.into());
        }
    };

    link.attach(listening, &landed);

    let reader = {
        let link = link.clone();
        std::thread::Builder::new()
            .name("smkvm-worker-reader".into())
            .spawn(move || {
                while let Ok(said) = wire::read_frame::<wire::FromWorker, _>(&mut reading) {
                    link.heard(said);
                }
            })
    };
    if let Err(e) = reader {
        // The pipe is already attached at this point, so without both of
        // these the worker survives, attached, while the caller records a
        // failure and holds nothing that could kill it.
        link.detach();
        started.kill();
        return Err(
            anyhow::Error::from(e).context("starting the thread that reads from the worker")
        );
    }

    Ok((started, landed))
}
