//! The `smkvm` command.

mod client;
mod clipboard;
mod hello;
mod platform;
mod secure;
mod server;
mod service;
mod transfer;

// Where things live is the daemon's agreement with anything else that reads
// the same files, so it belongs with the configuration rather than in here.
use smkvm_config::paths;

use std::io::Write as _;
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use smkvm_config::status::{MachineState, Status};
use smkvm_config::Config;
use smkvm_input::Monitors;
use smkvm_layout::Layout;
use smkvm_net::identity::Identity;
use smkvm_net::pairing::Pairing;
use smkvm_net::trust::Trust;
use smkvm_proto::Role;
use tokio::net::TcpListener;
use tracing::info;

/// Past this the log is rolled over, so a machine left running for months
/// does not fill its disk with pointer crossings.
const LOG_ROLLOVER: u64 = 4 * 1024 * 1024;

#[derive(Parser)]
#[command(
    name = "smkvm",
    version,
    about = "Share one keyboard and mouse across several machines"
)]
struct Cli {
    /// Read configuration from here instead of the usual place.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    /// Say more about what is happening.
    #[arg(short, long, global = true)]
    verbose: bool,
    /// Write the log here instead of to the terminal.
    #[arg(long, global = true, value_name = "FILE")]
    log_file: Option<PathBuf>,
    /// Started at login by `smkvm service`, with nobody at a terminal: log
    /// to the usual file even if there seems to be a console.
    #[arg(long, global = true, hide = true)]
    unattended: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum RoleArg {
    /// This machine owns the keyboard and mouse.
    Server,
    /// This machine receives the cursor from another.
    Client,
}

#[derive(Subcommand)]
enum Command {
    /// Write a starting configuration for this machine.
    Init {
        /// Whether this machine shares its keyboard and mouse or receives them.
        #[arg(long, value_enum)]
        role: RoleArg,
        /// For a client: the server, as `host` or `host:port`.
        #[arg(long)]
        server: Option<String>,
        /// For a server: an address to accept connections on. May be repeated.
        /// Without any, only this machine itself can connect.
        #[arg(long)]
        listen: Vec<String>,
        /// What this machine is called. Defaults to its hostname.
        #[arg(long)]
        name: Option<String>,
        /// Replace a configuration that is already there.
        #[arg(long)]
        force: bool,
    },
    /// Introduce this machine to another, once.
    Pair {
        /// The machine to reach out to, as `host` or `host:port`. Omit to wait
        /// for one to reach out instead.
        host: Option<String>,
        /// Accept without asking. Only safe where the code has been compared
        /// by other means.
        #[arg(long)]
        yes: bool,
    },
    /// Do whatever the configuration says this machine does: serve or connect.
    Run,
    /// Own the keyboard and mouse, and pass them to the other machines.
    Serve,
    /// Receive the cursor from the machine that owns it.
    Connect {
        /// Override the server address from the configuration.
        host: Option<String>,
    },
    /// Say what the running daemon is connected to, and where things are.
    Status,
    /// Start at login, and be started and stopped by hand.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// List the machines this one has been paired with.
    Devices,
    /// Forget a machine.
    Forget { name_or_id: String },
    /// Print this machine's displays, and stop.
    Monitors,
    /// How the service control manager starts this on Windows. Not a
    /// command to run by hand: it hands the process to the manager, which
    /// answers nothing when there is no manager asking.
    #[command(hide = true)]
    ServiceMain,
    /// The arm the service puts on a desktop, started by the service with
    /// the pipe it should report to. Not a command to run by hand.
    #[command(hide = true)]
    DesktopWorker {
        #[arg(long)]
        pipe: String,
    },
    /// The half the service runs as the person at the desk, so that
    /// what they copy can be seen at all. Started by the service with
    /// the pipe it should report to. Not a command to run by hand.
    #[command(hide = true)]
    ClipboardReader {
        #[arg(long)]
        pipe: String,
        /// Somewhere plain to write what it is doing, handed to it by
        /// the service because it cannot work out a writable path for
        /// itself before it has said anything.
        #[arg(long)]
        trace_to: Option<std::path::PathBuf>,
    },
    /// Look at this session's clipboard and say what can be seen,
    /// and -- as the system account -- what two other observers see.
    /// For deciding one design question; not a command to run by
    /// hand once that is decided.
    #[command(hide = true)]
    ClipboardProbe {
        /// Where a child leaves what it saw, for its parent to print.
        #[arg(long)]
        report_to: Option<std::path::PathBuf>,
    },
    /// Write a starting configuration, migrating an existing Barrier setup.
    Import {
        /// Barrier's settings file. Defaults to where it usually lives.
        #[arg(long)]
        from: Option<PathBuf>,
        /// A server-side barrier.conf to fold in as well.
        #[arg(long)]
        server_config: Option<PathBuf>,
        /// Write the result rather than printing it.
        #[arg(long)]
        write: bool,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Register `smkvm run` to start when you log in: on Windows a scheduled
    /// task in the desktop session with highest privileges, on Linux a login
    /// item. Run from an administrator prompt on Windows.
    Install {
        /// Windows: the account whose login starts it, when that is not
        /// the account registering it (as `DOMAIN\name` or `name`).
        /// With --system it instead names the account whose
        /// configuration, identity and paired machines are carried into
        /// the machine-wide directory; the default there is whoever is
        /// logged in at the screen, which is usually not the account
        /// installing over ssh.
        #[arg(long)]
        user: Option<String>,
        /// Windows: register without highest privileges. Windows will then
        /// refuse input into any window running as administrator.
        #[arg(long)]
        limited: bool,
        /// Windows: register a service running as the system account
        /// instead of a task in your session. Only this reaches the
        /// desktop a UAC prompt, the lock screen or Ctrl+Alt+Del is on,
        /// because nothing in your own session may go there. Everything
        /// else works the same either way.
        #[arg(long)]
        system: bool,
    },
    /// Stop starting at login, and stop now.
    Uninstall,
    /// Start now, the way a login would.
    Start,
    /// Stop now. On a server this gives the keyboard and mouse back.
    Stop,
    /// Say whether it is set to start at login, and whether it is running.
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Before the log is opened, because the log is a path like any other
    // and `start_logging` resolves it. Done inside `serve()` -- several
    // frames later -- it left the service writing its log into the
    // system profile while its configuration, identity and report were
    // all machine-wide: one path in one scope and the rest in the other,
    // with `smkvm status` printing the wrong one. No risk in that, but
    // the log is the single artefact that turned the last outage into
    // one look instead of an afternoon, so it is worth its own line at
    // the top of the program.
    if matches!(
        cli.command,
        Command::ServiceMain | Command::DesktopWorker { .. }
    ) {
        // Deliberately not the reader. It runs as the person at the
        // desk and reads no configuration at all -- it is handed a
        // pipe name and nothing else -- so pointing it at the
        // machine-wide files would give it a scope it has no use for
        // and no right to.
        paths::use_machine_scope();
    }
    // The reader is the one command for which a log that cannot be
    // opened is not a reason to stop.
    //
    // Every other command here either has a console to complain to or
    // is started by something that will notice. The reader has
    // neither: no console, no terminal, and a parent that can only
    // see a pipe. When its log could not be opened -- which happened,
    // because it was being given the service's environment and
    // pointed at a profile it cannot write -- it exited before saying
    // anything, anywhere, and all the service learned was that the
    // pipe had broken. A process whose first act can kill it silently
    // is the shape this file has spent days removing, and this was
    // the newest instance of it.
    //
    // So for the reader the complaint is carried instead of raised,
    // and goes down the pipe as soon as there is one.
    let no_log = match start_logging(cli.verbose, cli.log_file.clone(), cli.unattended) {
        Ok(()) => None,
        Err(e) if matches!(cli.command, Command::ClipboardReader { .. }) => Some(format!("{e:#}")),
        Err(e) => return Err(e),
    };
    log_panics_too();

    let result = match cli.command {
        Command::Init {
            role,
            server,
            listen,
            name,
            force,
        } => init(cli.config, role, server, listen, name, force),
        Command::Monitors => monitors(),
        Command::Status => status(cli.config),
        Command::Service { action } => match action {
            ServiceAction::Install {
                user,
                limited,
                system,
            } => service::install(user, limited, system),
            ServiceAction::Uninstall => service::uninstall(),
            ServiceAction::Start => service::start(),
            ServiceAction::Stop => service::stop(),
            ServiceAction::Status => service::status(),
        },
        Command::ServiceMain => service_main(),
        Command::DesktopWorker { pipe } => desktop_worker(&pipe),
        Command::ClipboardProbe { report_to } => clipboard_probe(report_to),
        Command::ClipboardReader { pipe, trace_to } => clipboard_reader(&pipe, no_log, trace_to),
        Command::Devices => devices(),
        Command::Forget { name_or_id } => forget(&name_or_id),
        Command::Import {
            from,
            server_config,
            write,
        } => import(from, server_config, write),
        Command::Pair { host, yes } => block_on(pair(host, yes)),
        Command::Run => block_on(run(cli.config)),
        Command::Serve => block_on(serve(cli.config)),
        Command::Connect { host } => block_on(connect(cli.config, host)),
    };

    // Whatever went wrong has to reach the log as well. Started by a scheduled
    // task or a service there is no console, so an error reported only by
    // returning it goes nowhere and the program merely appears to do nothing.
    if let Err(e) = &result {
        tracing::error!("{e:#}");
    }
    result
}

/// Send the log somewhere it will actually be read.
///
/// Started from a terminal it goes to the terminal. Started any other way --
/// by a scheduled task, a service, anything without a console -- there is
/// nowhere for it to go, so it goes to a file instead. A program that logs
/// into the void is one that cannot be diagnosed at all.
///
/// The login task says `--unattended` rather than being left to guess: a
/// scheduled task on Windows gets a console of its own, headless or not,
/// and stderr looks like a terminal from inside it. Two deployments went
/// undiagnosable that way, their logs written to a console nobody could see.
fn start_logging(verbose: bool, explicit: Option<PathBuf>, unattended: bool) -> Result<()> {
    use std::io::IsTerminal as _;

    // Written down beats passed in, for a process nothing can pass
    // anything to. See `choose_filter`.
    let from_file = std::fs::read_to_string(paths::log_level_file()).ok();
    let filter = tracing_subscriber::EnvFilter::new(choose_filter(
        std::env::var("SMKVM_LOG").ok().as_deref(),
        from_file.as_deref(),
        verbose,
    ));
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);

    let path = match explicit {
        Some(path) => Some(path),
        None if !unattended && std::io::stderr().is_terminal() => None,
        None => Some(paths::log_file()),
    };
    match path {
        None => builder.init(),
        Some(path) => {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)
                    .with_context(|| format!("making {}", dir.display()))?;
            }
            roll_over(&path);
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("opening the log at {}", path.display()))?;
            builder
                .with_ansi(false)
                .with_writer(move || file.try_clone().expect("the log file can be shared"))
                .init();
            eprintln!("logging to {}", path.display());
        }
    }
    Ok(())
}

/// Which log filter to use, given everything that might say.
///
/// In order: the environment, then a file beside this machine's other
/// files, then `--verbose`, then the default. The file is here for the
/// service, which has no command line to be given a flag on and no
/// terminal to set a variable in; the documented `Environment` value
/// under the service's registry key works only when it is written as
/// `REG_MULTI_SZ`, and does nothing at all -- with no error anywhere --
/// when it is written as `REG_SZ`. That is exactly the kind of silence
/// this program keeps losing days to, so there is a way that has
/// nothing to get wrong.
fn choose_filter(from_env: Option<&str>, from_file: Option<&str>, verbose: bool) -> String {
    for said in [from_env, from_file].into_iter().flatten() {
        let said = said.trim();
        if !said.is_empty() {
            return said.to_string();
        }
    }
    if verbose { "debug" } else { "info" }.to_string()
}

/// Make a thread that dies say so.
///
/// A panic on a spawned thread prints to stderr and returns; a service
/// has no stderr anybody reads, so the thread simply stops and nothing
/// anywhere mentions it. The minding thread stopping that way would look
/// exactly like the minding thread deciding to do nothing -- and telling
/// those two apart by reading a log was the thing that could not be done
/// on a real machine.
fn log_panics_too() {
    let already = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        tracing::error!(
            "a thread has died: {panic}. Whatever it was minding is no longer being \
             minded, and this process will not notice by itself"
        );
        already(panic);
    }));
}

/// Keep the log from growing without bound: past a certain size the current
/// one becomes `.1` and a fresh one is started. One generation back is kept,
/// because the evidence for a fault is usually in the log that was just
/// closed, not the one that was just opened.
fn roll_over(path: &std::path::Path) {
    let Ok(meta) = std::fs::metadata(path) else {
        return;
    };
    if meta.len() < LOG_ROLLOVER {
        return;
    }
    let mut older = path.as_os_str().to_owned();
    older.push(".1");
    let _ = std::fs::rename(path, older);
}

/// Whether something has asked the daemon to stop, and a way to be woken
/// when it does.
///
/// The daemon has always stopped on Ctrl+C, which is the only way to ask
/// when a person started it. A service has no console and no Ctrl+C: the
/// manager delivers a stop to a handler on a thread of its own, and
/// without somewhere for that to land the daemon goes on running until the
/// manager's patience runs out and the process is killed -- which is a
/// service that cannot be stopped, and, on a server, a keyboard that is
/// not given back until the machine is reset.
///
/// So there are two ways to ask and one place they arrive. [`stopping`]
/// replaces `tokio::signal::ctrl_c()` at every point the daemon used to
/// wait on it, and waits on either.
static ASKED_TO_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static STOP_ASKED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// Ask the daemon to stop. Safe to call from any thread, including one
/// that knows nothing about the runtime -- which the service's control
/// handler is.
/// The only caller is the Windows service's control handler, so on a
/// Linux build this is compiled and never reached. It stays here rather
/// than behind `cfg(windows)` because the waiting half below is shared,
/// and splitting the pair would leave the two halves able to drift.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn ask_to_stop() {
    ASKED_TO_STOP.store(true, std::sync::atomic::Ordering::SeqCst);
    STOP_ASKED.notify_waiters();
}

/// Resolves when the daemon has been asked to stop, by whichever means.
pub(crate) async fn stopping() {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = asked_to_stop() => {}
    }
}

async fn asked_to_stop() {
    loop {
        // The waiter is registered before the flag is read, so an ask
        // that lands between the two wakes this rather than being missed.
        let waiting = STOP_ASKED.notified();
        if ASKED_TO_STOP.load(std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        waiting.await;
    }
}

fn block_on<F: std::future::Future<Output = Result<()>>>(f: F) -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("starting the runtime")?
        .block_on(f)
}

fn init(
    path: Option<PathBuf>,
    role: RoleArg,
    server: Option<String>,
    listen: Vec<String>,
    name: Option<String>,
    force: bool,
) -> Result<()> {
    let path = path.unwrap_or_else(paths::config_file);
    if path.exists() && !force {
        bail!(
            "{} already exists. Edit it, or pass --force to start over.",
            path.display()
        );
    }
    let role = match role {
        RoleArg::Server => Role::Server,
        RoleArg::Client => Role::Client,
    };
    if role == Role::Client && server.is_none() {
        bail!("a client needs to know its server: pass --server <host>");
    }
    let mut config = Config::fresh(name.unwrap_or_else(paths::default_name), role);
    config.network.server = server;
    config.network.listen = listen;
    if role == Role::Server && config.network.listen.is_empty() {
        eprintln!(
            "note: no --listen address given, so only this machine can connect. Add the \
             address other machines reach this one at to network.listen when you are ready."
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("making {}", dir.display()))?;
    }
    std::fs::write(&path, config.to_toml()?)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("wrote {}", path.display());
    println!();
    println!("Next:");
    println!("  1. On each pair of machines, run `smkvm pair <other>` on one and `smkvm pair` on the other.");
    match role {
        Role::Server => println!("  2. Run `smkvm run` here, and `smkvm run` on each client."),
        Role::Client => println!("  2. Run `smkvm run` here once the server is running."),
    }
    println!("  3. Open smkvm-gui to arrange the screens; the daemon picks up the change at once.");
    println!("  4. `smkvm service install` makes it start at login (from an administrator prompt on Windows).");
    Ok(())
}

/// Hand the process to the service control manager, which then calls back
/// into the service and does not return until it has stopped.
fn service_main() -> Result<()> {
    #[cfg(windows)]
    {
        secure::windows::scm::run_as_service()
    }
    #[cfg(not(windows))]
    {
        bail!("services are a Windows arrangement; on Linux `smkvm service install` adds a login item")
    }
}

/// Be the arm on a desktop. Started by the service and by nothing else.
fn desktop_worker(pipe: &str) -> Result<()> {
    #[cfg(windows)]
    {
        secure::windows::worker::run(pipe)
    }
    #[cfg(not(windows))]
    {
        let _ = pipe;
        bail!("the desktop worker is a Windows arrangement")
    }
}

/// Read the person's clipboard for the service, as the person.
fn clipboard_reader(
    pipe: &str,
    no_log: Option<String>,
    trace_to: Option<std::path::PathBuf>,
) -> Result<()> {
    #[cfg(windows)]
    {
        secure::windows::reader::run(pipe, no_log, trace_to)
    }
    #[cfg(not(windows))]
    {
        let _ = (pipe, no_log, trace_to);
        bail!("the clipboard reader is a Windows arrangement")
    }
}

/// Take the two measurements that decide how the clipboard gets at
/// what the person copied.
fn clipboard_probe(report_to: Option<std::path::PathBuf>) -> Result<()> {
    #[cfg(windows)]
    {
        secure::windows::probe::probe(report_to)
    }
    #[cfg(not(windows))]
    {
        let _ = report_to;
        bail!("the clipboard probe answers a question that only arises on Windows")
    }
}

/// Run the daemon, from inside the service.
///
/// The same `run` a person gets from `smkvm run`, reached the same way.
/// Everything that makes the service different from a daemon started at
/// login has already happened by the time this is called: the arm is set,
/// and a worker is being minded on whichever desktop has the input.
#[cfg(windows)]
pub(crate) fn run_daemon_for_service() -> Result<()> {
    block_on(run(None))
}

fn monitors() -> Result<()> {
    let mut injector = platform::injector()?;
    let monitors = injector.monitors().context("reading displays")?;
    println!("{} display(s):", monitors.len());
    for m in &monitors {
        println!(
            "  {:<48} {:>5} x {:<5} at {:>6},{:<6}{}",
            m.id.as_str(),
            m.local.w,
            m.local.h,
            m.local.x,
            m.local.y,
            if m.primary { "  primary" } else { "" }
        );
    }
    Ok(())
}

fn status(config_path: Option<PathBuf>) -> Result<()> {
    let config_path = config_path.unwrap_or_else(paths::config_file);
    println!("configuration  {}", config_path.display());
    println!("log            {}", paths::log_file().display());
    println!("status         {}", paths::status_file().display());
    report_on_the_machine_key();
    report_on_the_clipboard();
    println!();

    // Either scope may have written it: the service writes machine-wide
    // and this command is run by the person at the desk.
    let status_path = paths::status_file_to_read();
    let report = Status::load(&status_path)?;
    let Some(report) = report else {
        println!("smkvm is not running here (no report has been written).");
        return Ok(());
    };
    if !report.is_current() {
        println!(
            "smkvm is not running here. Its last report is {} s old and says nothing about now.",
            report.age().as_secs()
        );
        return Ok(());
    }
    let role = match report.role {
        Role::Server => "sharing its keyboard and mouse",
        Role::Client => "receiving the cursor",
    };
    println!(
        "{} is {}{}, reported {} s ago.",
        report.name,
        role,
        report
            .pid
            .map(|pid| format!(" (pid {pid})"))
            .unwrap_or_default(),
        report.age().as_secs()
    );
    if let Some(active) = report.active() {
        println!("The cursor is on {}.", active.name);
    }
    println!();
    for machine in &report.machines {
        let state = match machine.state {
            MachineState::Connected => "connected",
            MachineState::Suspended => "connected, not taking input",
            MachineState::Away => "away",
        };
        println!("  {:<24} {}", machine.name, state);
        for m in &machine.monitor {
            let r = m.rect();
            println!(
                "      {:<40} {:>5} x {:<5} at {:>6},{:<6}{}",
                m.label.as_deref().unwrap_or(&m.id),
                r.w,
                r.h,
                r.x,
                r.y,
                if m.primary { "  primary" } else { "" }
            );
        }
    }
    Ok(())
}

/// Say, every time somebody asks how things are, whether this account can
/// read the machine-wide key.
///
/// The service checks its key at startup and logs `KEY PRIVATE`,
/// `KEY READABLE` or `KEY UNPROVEN`. That is one line, weeks ago, in a
/// file nobody re-reads -- so a key that could not be proven private
/// would quietly stay unproven for as long as the machine ran.
///
/// This does not echo that verdict; it measures again, and by the most
/// direct means there is. `type %ProgramData%\smkvm\device.toml` from an
/// ordinary account is the check that settles the question on a real
/// machine, and it needs no access list to be parsed and no agreement
/// with `icacls` about anything. So that is what this does: it opens the
/// file. Whether the open succeeds is not an opinion.
///
/// The one thing it cannot know is whether the account asking is an
/// administrator, who may legitimately read it. So it reports what it
/// found and who it would be a fault for, rather than pronouncing.
fn report_on_the_machine_key() {
    if !cfg!(windows) {
        return;
    }
    let key = paths::machine_state_dir().join(paths::IDENTITY_NAME);
    if !key.exists() {
        return;
    }
    match std::fs::File::open(&key) {
        Ok(_) => println!(
            "machine key     {} -- READABLE BY THIS ACCOUNT.\n\
             \x20               If you are not running as an administrator this is a \
             fault: that key is what the\n\
             \x20               other machines trust this one by, and anyone holding a \
             copy can be handed\n\
             \x20               the cursor and the keystrokes that follow it. Reinstall \
             with `smkvm service\n\
             \x20               install --system` from an administrator prompt, and pair \
             this machine afresh.",
            key.display()
        ),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => println!(
            "machine key     {} -- not readable by this account, which is right",
            key.display()
        ),
        Err(e) => println!(
            "machine key     {} -- could not be opened to find out ({e})",
            key.display()
        ),
    }
}

/// Say whether anything is holding this machine's clipboard open.
///
/// Only one process in a session may have the clipboard open at a
/// time, so a window left in there stops *everything* copying and
/// pasting -- not only this program. That happened, held by this
/// program's own worker, and from the outside it looked like the
/// machine's clipboard had simply broken.
///
/// Reported here for the same reason the key is: a person whose copy
/// and paste has stopped should be able to find out in one command
/// whether we are the reason, without reading a log or knowing that
/// this program has a worker at all.
fn report_on_the_clipboard() {
    #[cfg(windows)]
    {
        // Printed from the session this command was run in, which is
        // the only session whose answer means anything: the clipboard
        // belongs to a window station, and a service asking from
        // session 0 would be describing a different clipboard
        // altogether.
        println!("clipboard       what this session's clipboard holds, right now:");
        println!(
            "\x20               (reading it is a paste: an owner that promised its \
             formats will be"
        );
        println!("\x20                asked to produce them, and this line is that request)");
        for line in smkvm_clipboard::platform::windows::verdict() {
            println!("\x20               {line}");
        }
    }
}

fn load_identity() -> Result<Identity> {
    let path = paths::identity_file();
    Identity::load_or_create(&path)
        .with_context(|| format!("reading this machine's identity from {}", path.display()))
}

fn load_trust() -> Result<Trust> {
    let path = paths::peers_file();
    Trust::load(&path).with_context(|| format!("reading paired machines from {}", path.display()))
}

fn devices() -> Result<()> {
    let identity = load_identity()?;
    println!(
        "this machine: {}  ({})",
        paths::default_name(),
        identity.id()
    );
    let trust = load_trust()?;
    let peers: Vec<_> = trust.peers().collect();
    if peers.is_empty() {
        println!("\nno machines paired yet. Run `smkvm pair` on both ends.");
        return Ok(());
    }
    println!("\npaired with:");
    for peer in peers {
        println!("  {:<24} {}", peer.name, peer.id);
    }
    Ok(())
}

fn forget(name_or_id: &str) -> Result<()> {
    let mut trust = load_trust()?;
    let target = trust
        .peers()
        .find(|p| p.name == name_or_id || p.id.to_hex().starts_with(name_or_id))
        .map(|p| p.id);
    let Some(id) = target else {
        bail!("no paired machine called {name_or_id}");
    };
    let peer = trust.remove(id).expect("just found it");
    trust.save(&paths::peers_file())?;
    println!(
        "forgot {} ({}). It must be paired again to connect.",
        peer.name, peer.id
    );
    Ok(())
}

fn import(from: Option<PathBuf>, server_config: Option<PathBuf>, write: bool) -> Result<()> {
    use smkvm_config::barrier::{Import, ServerConfig};

    let from = from.unwrap_or_else(default_barrier_settings);
    let text = std::fs::read_to_string(&from)
        .with_context(|| format!("reading Barrier's settings from {}", from.display()))?;
    let mut import = Import::from_qt_settings(&text);
    if let Some(path) = &server_config {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        import.merge_server_config(&ServerConfig::parse(&text));
    }

    eprintln!("Read {}", from.display());
    eprintln!("\nNot carried over:");
    for note in &import.notes {
        eprintln!("  - {note}");
    }
    eprintln!();

    let toml = Config::from_barrier(&import).to_toml()?;
    if !write {
        print!("{toml}");
        eprintln!("\n(nothing written; pass --write to save it)");
        return Ok(());
    }
    let path = paths::config_file();
    if path.exists() {
        bail!("{} already exists; move it aside first", path.display());
    }
    std::fs::create_dir_all(paths::config_dir())?;
    let mut file = std::fs::File::create(&path)?;
    file.write_all(toml.as_bytes())?;
    println!("wrote {}", path.display());
    Ok(())
}

fn default_barrier_settings() -> PathBuf {
    if cfg!(windows) {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("Debauchee/Barrier.conf")
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".config/Debauchee/Barrier.conf")
    }
}

fn load_config(path: Option<PathBuf>) -> Result<(Config, PathBuf)> {
    let path = path.unwrap_or_else(paths::config_file);
    if !path.exists() {
        // The advice has to match who is asking. Told to run `smkvm init`,
        // a service running as the system account would write a fourth
        // configuration into the system profile and still not find this
        // one -- and that is not a guess about what somebody might do, it
        // is what the message said on the first real installation, under a
        // path reading `C:\WINDOWS\system32\config\systemprofile\...`
        // which tells the reader almost nothing about what went wrong.
        if paths::scope() == paths::Scope::Machine {
            bail!(
                "no configuration at {}. This is the service, which reads the machine's \
                 own files rather than any person's profile, so `smkvm init` is not the \
                 answer -- it would write one more configuration somewhere else again. \
                 `smkvm service install --system` carries an existing configuration, \
                 identity and paired-machines list across from the account that runs it; \
                 run that from an administrator prompt on an account that already has \
                 them, or put the three files in {} by hand.",
                path.display(),
                paths::machine_config_dir().display()
            );
        }
        bail!(
            "no configuration at {}. Run `smkvm init --role server` or `smkvm init --role \
             client --server <host>` to write one, or `smkvm import` to migrate an existing \
             Barrier setup.",
            path.display()
        );
    }
    Ok((Config::load(&path)?, path))
}

/// Refuse to start a second daemon beside one that is running.
///
/// Two copies on one machine fight over the link, and the one that loses is
/// left holding whatever it did last -- a pointer out of the way, keys down.
/// The daemon's own status report is the evidence: a report younger than its
/// refresh interval was written by something that is still there -- unless
/// the process it names has gone, in which case it was left behind by a
/// daemon that was stopped moments ago, and waiting out the interval would
/// only make every restart take a quarter of a minute.
fn ensure_not_running() -> Result<()> {
    // Both places, because the daemon this must not start beside may be a
    // service, which reports machine-wide.
    let path = paths::status_file_to_read();
    if let Some(report) = Status::current(&path) {
        if let Some(pid) = report.pid {
            if process_alive(pid) == Some(false) {
                tracing::info!(
                    pid,
                    "the last report was left behind by a daemon that has gone"
                );
                return Ok(());
            }
        }
        bail!(
            "smkvm already seems to be running here{} -- its report at {} was written {} s ago. \
             Stop it first. If it is not running, wait {} s or delete that file.",
            report
                .pid
                .map(|pid| format!(" as pid {pid}"))
                .unwrap_or_default(),
            path.display(),
            report.age().as_secs(),
            Status::FRESH_FOR.as_secs()
        );
    }
    Ok(())
}

/// Whether a process with this id exists, or `None` where that cannot be asked.
///
/// A process id is reused eventually, so "alive" may name some other program
/// -- which then costs one refusal until the report ages out, exactly as
/// before. "Gone" is the answer that matters, and is not mistaken.
pub(crate) fn process_alive(pid: u32) -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        Some(std::path::Path::new(&format!("/proc/{pid}")).exists())
    }
    #[cfg(windows)]
    {
        use windows::Win32::Foundation::{CloseHandle, ERROR_INVALID_PARAMETER, STILL_ACTIVE};
        use windows::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        // SAFETY: asking for a handle by id; the handle is closed below.
        match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
            Ok(handle) => {
                let mut code = 0u32;
                // SAFETY: a valid handle and a place to put the code.
                let alive = unsafe { GetExitCodeProcess(handle, &mut code) }.is_ok()
                    && code == STILL_ACTIVE.0 as u32;
                // SAFETY: balanced against the open above.
                unsafe {
                    let _ = CloseHandle(handle);
                }
                Some(alive)
            }
            // No such process. Anything else -- most often access denied, for
            // a process belonging to somebody else -- means it exists.
            Err(e) if e.code() == ERROR_INVALID_PARAMETER.to_hresult() => Some(false),
            Err(_) => Some(true),
        }
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = pid;
        None
    }
}

async fn pair(host: Option<String>, yes: bool) -> Result<()> {
    let identity = load_identity()?;
    let mut trust = load_trust()?;
    let name = paths::default_name();
    println!("this machine: {name}  ({})", identity.id());

    let pairing = match host {
        Some(host) => {
            let address = with_default_port(&host);
            println!("reaching out to {address} ...");
            Pairing::initiate(&address, &identity, &name).await?
        }
        None => {
            let address = format!("0.0.0.0:{}", smkvm_config::DEFAULT_PORT);
            println!("waiting on {address} for a machine to pair with ...");
            let listener = TcpListener::bind(&address).await?;
            let (socket, from) = listener.accept().await?;
            println!("a machine at {from} is asking to pair");
            Pairing::accept(socket, &identity, &name).await?
        }
    };

    println!(
        "\n  the other machine calls itself: {}",
        pairing.peer_name()
    );
    println!("\n      confirmation code:  {}\n", pairing.code());
    println!("Check that the same code is showing on the other machine.");
    println!("If the two differ, something is relaying the connection: do not accept.");

    if !yes && !ask("Do the codes match?") {
        pairing.reject().await?;
        println!("not paired.");
        return Ok(());
    }

    let peer = pairing
        .confirm()
        .await
        .context("the other end did not accept")?;
    let name = peer.name.clone();
    trust.add(peer.public_key, &name);
    trust.save(&paths::peers_file())?;
    println!("paired with {name}.");
    Ok(())
}

fn ask(question: &str) -> bool {
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn with_default_port(host: &str) -> String {
    if host
        .rsplit(':')
        .next()
        .is_some_and(|p| p.parse::<u16>().is_ok())
        && host.matches(':').count() == 1
    {
        host.to_string()
    } else {
        format!("{host}:{}", smkvm_config::DEFAULT_PORT)
    }
}

/// Serve or connect, whichever the configuration says.
async fn run(config_path: Option<PathBuf>) -> Result<()> {
    let (config, _) = load_config(config_path.clone())?;
    match config.network.role {
        Role::Server => serve(config_path).await,
        Role::Client => connect(config_path, None).await,
    }
}

async fn serve(config_path: Option<PathBuf>) -> Result<()> {
    let (config, path) = load_config(config_path)?;
    if config.network.role != Role::Server {
        bail!("this machine's configuration says it is a client; change network.role to serve");
    }
    ensure_not_running()?;
    let identity = load_identity()?;
    let trust = load_trust()?;
    if trust.peers().count() == 0 {
        bail!("no machines are paired yet, so none could connect. Run `smkvm pair` first.");
    }
    info!(name = %config.identity.name, id = %identity.id().short(), "starting as the server");
    platform::report_rank();
    let layout = Layout::new(config.behavior.edge_overflow);
    server::run(identity, trust, config, path, layout).await
}

async fn connect(config_path: Option<PathBuf>, host: Option<String>) -> Result<()> {
    let (config, _) = load_config(config_path)?;
    ensure_not_running()?;
    let identity = load_identity()?;
    let trust = load_trust()?;

    let address = host
        .or_else(|| config.network.server.clone())
        .context("no server address: give one, or set network.server")?;
    let address = with_default_port(&address);

    // A client connects to exactly one machine, and must already know it: the
    // handshake is encrypted to that machine's key.
    let peer = client::choose_server(&trust, &config)?;
    info!(server = %peer.name, %address, "connecting");
    platform::report_rank();
    client::run(identity, peer, address, config).await
}

#[cfg(test)]
mod filter_tests {
    use super::choose_filter;

    #[test]
    fn the_environment_wins_because_it_is_the_most_deliberate() {
        assert_eq!(choose_filter(Some("trace"), Some("debug"), true), "trace");
    }

    #[test]
    fn a_file_beats_a_flag_a_service_can_never_be_given() {
        assert_eq!(choose_filter(None, Some("debug"), false), "debug");
        assert_eq!(
            choose_filter(None, Some("smkvm_cli=debug,info"), false),
            "smkvm_cli=debug,info"
        );
    }

    #[test]
    fn a_file_with_nothing_in_it_says_nothing() {
        // An empty or whitespace file is somebody having made the file
        // and not yet written the level, which must not turn logging
        // off or produce a filter that matches nothing.
        assert_eq!(choose_filter(None, Some(""), false), "info");
        assert_eq!(choose_filter(None, Some("  \n"), true), "debug");
        assert_eq!(choose_filter(None, None, false), "info");
    }

    #[test]
    fn the_trailing_newline_every_editor_adds_is_not_part_of_the_level() {
        assert_eq!(choose_filter(None, Some("debug\r\n"), false), "debug");
    }
}

#[cfg(test)]
mod process_tests {
    use super::process_alive;

    #[test]
    fn this_process_is_alive() {
        assert_eq!(process_alive(std::process::id()), Some(true));
    }

    #[test]
    fn a_process_that_has_exited_is_gone() {
        let mut child = std::process::Command::new(if cfg!(windows) { "cmd" } else { "true" })
            .args(if cfg!(windows) {
                &["/c", "exit"][..]
            } else {
                &[][..]
            })
            .spawn()
            .expect("a short-lived child");
        let pid = child.id();
        child.wait().expect("it finishes");
        assert_eq!(process_alive(pid), Some(false));
    }
}
