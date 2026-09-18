//! The `smkvm` command.

mod client;
mod clipboard;
mod hello;
mod platform;
mod server;

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
    /// List the machines this one has been paired with.
    Devices,
    /// Forget a machine.
    Forget { name_or_id: String },
    /// Print this machine's displays, and stop.
    Monitors,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    start_logging(cli.verbose, cli.log_file.clone())?;

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
fn start_logging(verbose: bool, explicit: Option<PathBuf>) -> Result<()> {
    use std::io::IsTerminal as _;

    let filter = tracing_subscriber::EnvFilter::try_from_env("SMKVM_LOG").unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(if verbose { "debug" } else { "info" })
    });
    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false);

    let path = match explicit {
        Some(path) => Some(path),
        None if std::io::stderr().is_terminal() => None,
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
    Ok(())
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
    println!();

    let status_path = paths::status_file();
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
/// refresh interval was written by something that is still there.
fn ensure_not_running() -> Result<()> {
    let path = paths::status_file();
    if let Some(report) = Status::current(&path) {
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
    client::run(identity, peer, address, config).await
}
