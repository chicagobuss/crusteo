//! `burrow`: the daemon binary.

#![forbid(unsafe_code)]

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use burrow_daemon::{Config, DEFAULT_LISTEN, Daemon, daemon, home, paseo_import, seed};
use burrow_provider::echo::EchoProvider;
use burrow_provider::{Discovery, Provider, env};
use burrow_provider_acp::AcpProvider;
use burrow_provider_antigravity::AntigravityProvider;
use burrow_provider_claude::ClaudeProvider;
use burrow_provider_codex::CodexProvider;
use burrow_provider_pi::PiProvider;
use clap::Parser;
use tracing_subscriber::EnvFilter;

/// How long startup waits for pi's model list.
const PI_CATALOG_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The release version and the commit built, as --version shows them.
const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("CRUSTEO_COMMIT"), ")");

/// A daemon that speaks the Paseo WebSocket protocol.
#[derive(Debug, Parser)]
#[command(name = "burrow", version = VERSION)]
struct Args {
    #[command(subcommand)]
    command: Option<Subcommand>,
    /// Address to listen on. Serves `ws://<listen>/ws`. Defaults to
    /// 127.0.0.1:6797. Given more than once (or comma-separated), the
    /// first must bind at start and the others are bound once they can be,
    /// such as a VPN's address that comes up after Burrow.
    #[arg(long, env = "BURROW_LISTEN", value_delimiter = ',')]
    listen: Vec<SocketAddr>,
    /// State directory, used as given. Defaults to $BURROW_HOME, then
    /// ~/.burrow (moved from ~/.carapace on first start).
    #[arg(long)]
    home: Option<PathBuf>,
    /// Override the daemon identity instead of reading `<home>/server-id`.
    #[arg(long)]
    server_id: Option<String>,
    /// Hostname reported in server_info. Defaults to the OS hostname.
    #[arg(long)]
    hostname: Option<String>,
    /// Version reported in server_info.
    #[arg(long)]
    advertise_version: Option<String>,
    /// Extra DNS names accepted in the Host header (repeatable).
    #[arg(long = "allow-host")]
    hostnames: Vec<String>,
    /// Extra origins accepted in the Origin header (repeatable).
    #[arg(long = "allow-origin")]
    allowed_origins: Vec<String>,
    /// Where git worktrees are made. Defaults to `<home>/worktrees`.
    #[arg(long, env = "BURROW_WORKTREES_ROOT")]
    worktrees_root: Option<PathBuf>,
    /// Start with a demo project, workspace and placeholder agent.
    #[arg(long)]
    seed_demo: bool,
    /// Offer the echo provider, which answers from the prompt text and runs no
    /// model (for trying clients out).
    #[arg(long)]
    echo_provider: bool,
    /// The Claude Code binary. Defaults to `claude` on PATH.
    #[arg(long)]
    claude_bin: Option<PathBuf>,
    /// Do not offer Claude even when it is installed.
    #[arg(long)]
    no_claude: bool,
    /// The Codex binary. Defaults to `codex` on PATH.
    #[arg(long)]
    codex_bin: Option<PathBuf>,
    /// Do not offer Codex even when it is installed.
    #[arg(long)]
    no_codex: bool,
    /// The Antigravity CLI. Defaults to `agy` on PATH.
    #[arg(long)]
    agy_bin: Option<PathBuf>,
    /// Do not offer Antigravity even when it is installed.
    #[arg(long)]
    no_agy: bool,
    /// The pi binary. Defaults to `pi` on PATH.
    #[arg(long)]
    pi_bin: Option<PathBuf>,
    /// Do not offer pi even when it is installed.
    #[arg(long)]
    no_pi: bool,
    /// The Cursor agent binary, run as `<bin> acp`. Defaults to
    /// `cursor-agent` on PATH.
    #[arg(long)]
    cursor_bin: Option<PathBuf>,
    /// Do not offer Cursor even when it is installed.
    #[arg(long)]
    no_cursor: bool,
}

#[derive(Debug, clap::Subcommand)]
enum Subcommand {
    /// Copy a Paseo home's agents (of every provider burrow runs), projects,
    /// workspaces and labels into the state directory, once. Stop burrow on
    /// that directory first.
    ImportPaseo {
        /// Paseo's home, which is only read.
        #[arg(long, default_value_os_t = user_home().join(".paseo"))]
        from: PathBuf,
        /// State directory, used as given. Defaults to $BURROW_HOME, then
        /// ~/.burrow (moved from ~/.carapace on first start).
        #[arg(long)]
        home: Option<PathBuf>,
        /// Print what would be imported and write nothing.
        #[arg(long)]
        dry_run: bool,
    },
}

fn user_home() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn import_paseo(
    from: PathBuf,
    home: Option<PathBuf>,
    codex: Option<PathBuf>,
    dry_run: bool,
) -> anyhow::Result<()> {
    let home = home
        .or_else(home::default_home)
        .context("no state directory; pass --home")?;
    let options = paseo_import::Options {
        from,
        home,
        claude_dir: burrow_provider_claude::history::config_dir(),
        pi_sessions: burrow_provider_pi::history::sessions_dir(&[]),
        codex,
        dry_run,
    };
    let report = paseo_import::run(&options).with_context(|| {
        format!(
            "importing {} into {}",
            options.from.display(),
            options.home.display()
        )
    })?;
    for line in &report.lines {
        println!("{line}");
    }
    println!("{}", report.summary(dry_run));
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Read before logging starts, so a deprecated name is reported after.
    let log = env::resolve("BURROW_LOG", |key| std::env::var_os(key));
    let filter = log
        .as_ref()
        .and_then(|(value, _)| value.to_str())
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
    if let Some((_, env::Source::Legacy(old))) = &log {
        env::warn_deprecated(old, "BURROW_LOG");
    }
    let args = Args::parse();
    let discoveries = discoveries(&args);
    if let Some(Subcommand::ImportPaseo {
        from,
        home,
        dry_run,
    }) = args.command
    {
        // Codex reads its threads back for the timelines.
        let codex = (!args.no_codex)
            .then(|| (args.codex_bin.clone()).unwrap_or_else(|| PathBuf::from("codex")));
        return import_paseo(from, home.or(args.home), codex, dry_run);
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        commit = env!("CRUSTEO_COMMIT"),
        "burrow starting"
    );
    // Clap reads the BURROW_ names; the CARAPACE_ ones still work for now.
    let mut listen = match args.listen {
        listen if !listen.is_empty() => listen,
        _ => match env::var_os("BURROW_LISTEN") {
            Some(value) => value
                .to_str()
                .and_then(|value| (value.split(',').map(|one| one.trim().parse().ok())).collect())
                .with_context(|| format!("CARAPACE_LISTEN is not an address: {value:?}"))?,
            None => vec![DEFAULT_LISTEN.parse().context("the default address")?],
        },
    };
    let also_listen = listen.split_off(1);
    let listen = listen[0];
    let worktrees_root = args
        .worktrees_root
        .or_else(|| env::var_os("BURROW_WORKTREES_ROOT").map(PathBuf::from));
    let home = args.home.or_else(home::default_home);
    let server_id = match (args.server_id, &home) {
        (Some(id), _) => id,
        (None, Some(home)) => home::server_id(home)
            .with_context(|| format!("reading the server id in {}", home.display()))?,
        (None, None) => anyhow::bail!("no state directory; pass --home or --server-id"),
    };
    let hostname = args
        .hostname
        .or_else(|| gethostname::gethostname().into_string().ok());
    let config = Config {
        listen,
        also_listen,
        home,
        server_id,
        hostname,
        version: args
            .advertise_version
            .unwrap_or_else(daemon::default_version),
        hostnames: args.hostnames,
        allowed_origins: args.allowed_origins,
        timing: daemon::Timing::default(),
        worktrees_root,
    };
    let daemon = Daemon::open(config).context("opening the state directory")?;
    burrow_daemon::discovery::start(&daemon, discoveries).await;
    if args.echo_provider {
        daemon
            .agents
            .register(std::sync::Arc::new(EchoProvider::default()));
    }
    if args.seed_demo && daemon.read(|store| store.projects.is_empty()) {
        let agent = seed::demo(&daemon);
        tracing::info!(agent, "seeded the demo directory");
    }
    // Clients that fetched the providers snapshot while a provider was still
    // loading get a `providers_snapshot_update` when it is done.
    burrow_daemon::agents::announce_when_loaded(&daemon);
    let running = burrow_daemon::start(daemon.clone())
        .await
        .with_context(|| format!("listening on {listen}"))?;
    stopped().await;
    tracing::info!("shutting down");
    running.shutdown().await;
    daemon.close();
    Ok(())
}

/// How to find each provider not turned off, at startup and whenever its
/// CLI changes ([`burrow_daemon::discovery`]).
fn discoveries(args: &Args) -> Vec<Discovery> {
    let program = |binary: &Option<PathBuf>, name: &str| {
        binary.clone().unwrap_or_else(|| PathBuf::from(name))
    };
    let mut found = Vec::new();
    if !args.no_claude {
        let binary = args.claude_bin.clone();
        let cli = program(&binary, "claude");
        found.push(Discovery::cli(
            "claude",
            cli,
            move || match ClaudeProvider::detect(binary.as_deref()) {
                Some(claude) => {
                    tracing::info!(version = ?claude.version(), "offering Claude Code");
                    Some(Arc::new(claude) as Arc<dyn Provider>)
                }
                None => {
                    tracing::warn!("Claude Code was not found; not offering it");
                    None
                }
            },
        ));
    }
    if !args.no_codex {
        let binary = args.codex_bin.clone();
        let cli = program(&binary, "codex");
        // The probe asks app-server for its models, which can take a moment.
        found.push(Discovery::cli(
            "codex",
            cli,
            move || match CodexProvider::detect(binary.as_deref()) {
                Some(codex) => {
                    tracing::info!(version = ?codex.version(), "offering Codex");
                    Some(Arc::new(codex) as Arc<dyn Provider>)
                }
                None => {
                    tracing::warn!("Codex was not found; not offering it");
                    None
                }
            },
        ));
    }
    if !args.no_agy {
        let binary = args.agy_bin.clone();
        let cli = program(&binary, "agy");
        let id = burrow_provider_antigravity::catalog::PROVIDER_ID;
        found.push(Discovery::cli(
            id,
            cli,
            move || match AntigravityProvider::detect(binary.as_deref()) {
                Ok(agy) => {
                    tracing::info!(version = agy.version(), "offering Antigravity");
                    Some(Arc::new(agy) as Arc<dyn Provider>)
                }
                Err(reason) => {
                    tracing::warn!(%reason, "not offering Antigravity");
                    None
                }
            },
        ));
    }
    if !args.no_pi {
        let binary = args.pi_bin.clone();
        let cli = program(&binary, "pi");
        found.push(Discovery::cli("pi", cli, move || {
            let Some(pi) = PiProvider::detect(binary.as_deref()) else {
                tracing::info!("pi was not found; not offering it");
                return None;
            };
            tracing::info!(version = ?pi.version(), "offering pi");
            // Its models come from pi itself. Wait a little for them, so the
            // snapshot has them, then let it finish.
            let handle = tokio::runtime::Handle::current();
            let reader = pi.clone();
            let mut catalog = handle.spawn(async move { reader.refresh_catalog().await });
            match handle.block_on(tokio::time::timeout(PI_CATALOG_WAIT, &mut catalog)) {
                Ok(Ok(Ok(count))) => tracing::info!(count, "pi models read"),
                Ok(Ok(Err(error))) => tracing::warn!(%error, "could not read pi's models"),
                Ok(Err(_)) => {}
                Err(_) => tracing::info!("pi's models are still loading"),
            }
            Some(Arc::new(pi) as Arc<dyn Provider>)
        }));
    }
    if !args.no_cursor {
        let binary = args.cursor_bin.clone();
        let cli = program(&binary, "cursor-agent");
        let id = burrow_provider_acp::cursor(binary.as_deref()).id;
        found.push(Discovery::cli(id, cli, move || {
            match AcpProvider::detect(burrow_provider_acp::cursor(binary.as_deref())) {
                Some(cursor) => {
                    tracing::info!(version = ?cursor.version(), "offering Cursor");
                    // Its models and modes come from a probe session, as in Paseo.
                    cursor.refresh_in_background();
                    Some(Arc::new(cursor) as Arc<dyn Provider>)
                }
                None => {
                    tracing::info!("Cursor's agent was not found; not offering it");
                    None
                }
            }
        }));
    }
    found
}

/// Ctrl-C, or SIGTERM where there is one (service managers send it).
async fn stopped() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut term) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}
