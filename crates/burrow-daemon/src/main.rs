//! `burrow`: the daemon binary.

#![forbid(unsafe_code)]

use std::io::IsTerminal;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use burrow_daemon::{Config, DEFAULT_LISTEN, Daemon, daemon, home, paseo_import, paseo_lock, seed};
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
/// `0.2.0 (<commit>)` for a release, `0.2.0-internal-<commit>` between
/// releases (build.rs).
const VERSION: &str = env!("CRUSTEO_VERSION");

/// What `burrow --help` ends with: how to drive a running Burrow.
const DRIVING: &str = "\
Driving Burrow:
  The commands above work on a running Burrow's agents: this host's (found
  through Paseo's daemon lock, then --listen, then the default) or another's
  with --host <host[:port]> (port 6767 when left out) or PASEO_HOST, which
  agents Burrow starts already have. --host ssh://[user@]host goes through
  ssh, with your keys and ~/.ssh/config, to the daemon on that host's
  loopback (?daemonPort=N when it is not 6767). Name an agent by its id, a
  unique prefix of it, or its name.

    burrow ls                          agents (-a adds archived ones)
    burrow ls --label billing          those whose workspace has that label
    burrow ls --fleet                  every host's, through this host's Reef
    burrow inspect <agent>             one agent's model, state, labels…
    burrow send <agent> \"<prompt>\"     a prompt; waits for the reply
    burrow run --provider claude \"<task>\"  start an agent; waits for it
    burrow wait <agent>                until its turn ends; its last message
    burrow logs <agent>                what it did (--tail N: the last N)
    burrow rename <agent> \"<name>\"     rename an agent
    burrow label <agent> key=value     set labels
    burrow stop <agent>                interrupt its turn
    burrow archive <agent>             stop it if it runs, then archive it
    burrow status                      what is running, and whether it answers

  An agent mid-turn: `send` steers the prompt into its turn where the
  provider can, else queues it behind the turn. --queue always waits behind
  it; --interrupt cuts the turn off.

  Paseo's CLI does everything else (new agents, permissions, terminals,
  schedules…) against Burrow as against Paseo: `paseo --host <host:port>`, or
  plain `paseo` on Paseo's port, where Burrow holds Paseo's daemon lock.

Logs:
  Burrow logs to standard output. Run it under a service manager that keeps
  that output (a systemd unit, launchd, an init script writing a file) rather
  than from a terminal: once the terminal closes, the log is lost.";

/// A daemon that speaks the Paseo WebSocket protocol.
#[derive(Debug, Parser)]
#[command(name = "burrow", version = VERSION, after_help = DRIVING)]
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
    /// OpenCode's CLI, run as `<bin> acp`. Defaults to `opencode` on PATH.
    #[arg(long)]
    opencode_bin: Option<PathBuf>,
    /// Do not offer OpenCode even when it is installed.
    #[arg(long)]
    no_opencode: bool,
    /// Google's Gemini CLI, run as `<bin> --acp`. Defaults to `gemini` on PATH.
    #[arg(long)]
    gemini_bin: Option<PathBuf>,
    /// Do not offer Gemini even when it is installed.
    #[arg(long)]
    no_gemini: bool,
    /// Do not hold Paseo's daemon lock (`$PASEO_HOME/paseo.pid`, default
    /// `~/.paseo/paseo.pid`), by which Paseo's CLI finds a daemon without
    /// `--host`. Burrow holds it only when it listens on Paseo's port (its
    /// `config.json`'s, else 6767), and never takes it from a running Paseo.
    #[arg(long, env = "BURROW_NO_PASEO_LOCK")]
    no_paseo_lock: bool,
    /// Where push notifications to Paseo's apps are sent. Defaults to
    /// Expo's push service, where the apps register.
    #[arg(long, env = "BURROW_PUSH_URL")]
    push_url: Option<String>,
    /// Send no push notifications to Paseo's apps.
    #[arg(long, env = "BURROW_NO_PUSH")]
    no_push: bool,
    /// Serve the agent tools (MCP) to a client with no token, as Burrow did
    /// before 0.2.0, for a script not yet changed to send the token in
    /// `<home>/mcp-token`. Agents always send their own.
    #[arg(long, env = "BURROW_MCP_TOKENLESS")]
    mcp_tokenless: bool,
}

/// How long Burrow waits for its agents' CLIs to exit when it stops.
const STOP_AGENTS_WITHIN: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Debug, clap::Subcommand)]
enum Subcommand {
    /// Ask the running Burrow what it is and what it runs: version, pid,
    /// providers and agents. It says hello as a client does, so a Burrow
    /// whose port answers but whose sessions fail shows as failing. Exits
    /// 1 when nothing answers.
    Status {
        #[command(flatten)]
        target: Target,
    },
    /// List the agents, most recently updated first.
    Ls {
        /// Archived agents and background workspaces' too.
        #[arg(short, long)]
        all: bool,
        /// Only agents with this label: a workspace label's name (as the
        /// app shows it), or `key=value` for the agent's own. Several must
        /// all match.
        #[arg(long = "label", value_name = "NAME|KEY=VALUE")]
        labels: Vec<String>,
        /// Every host's agents, through this host's Reef (with messaging
        /// between hosts turned on there), each with its host.
        #[arg(long)]
        fleet: bool,
        /// Print the agents' snapshots as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Show one agent: model, state, cwd, labels, attention.
    Inspect {
        /// The agent: its id, a unique prefix of it, or its name.
        agent: String,
        /// Print its snapshot as JSON.
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Send an agent a prompt and wait for its reply (exit 2: it waits on a
    /// permission; 3: still working at --timeout).
    Send {
        /// The agent: its id, a unique prefix of it, or its name.
        agent: String,
        /// The prompt. Or --prompt-file.
        prompt: Option<String>,
        /// Read the prompt from a file (`-` for standard input).
        #[arg(long, conflicts_with = "prompt")]
        prompt_file: Option<PathBuf>,
        /// Return once the agent has it, without waiting for the reply.
        #[arg(long)]
        no_wait: bool,
        /// Stop waiting after this many seconds.
        #[arg(long)]
        timeout: Option<u64>,
        /// If it is mid-turn, wait behind the turn even where it could be
        /// steered. (Unset: steer where its provider can, else wait.)
        #[arg(long, conflicts_with_all = ["interrupt", "steer"])]
        queue: bool,
        /// If it is mid-turn, cut the turn off and run this instead.
        #[arg(long, conflicts_with = "steer")]
        interrupt: bool,
        /// If it is mid-turn, steer into the turn; interrupt it where the
        /// provider cannot be steered (Paseo's steer).
        #[arg(long)]
        steer: bool,
        #[command(flatten)]
        target: Target,
    },
    /// Start an agent on a task and wait for it to finish (exit 2: it waits
    /// on a permission; 3: still working at --wait-timeout). From an agent,
    /// the new one is its child, in its workspace.
    Run {
        /// The task. Or --prompt-file.
        prompt: Option<String>,
        /// Read the task from a file (`-` for standard input).
        #[arg(long, conflicts_with = "prompt")]
        prompt_file: Option<PathBuf>,
        /// The provider, or provider/model (claude, codex/gpt-5.4, …).
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: Option<String>,
        /// A thinking option id (the provider's models list them).
        #[arg(long)]
        thinking: Option<String>,
        /// A provider mode (plan, default, bypass, …).
        #[arg(long)]
        mode: Option<String>,
        /// Its name.
        #[arg(long, visible_alias = "name")]
        title: Option<String>,
        /// Where it works (default: here).
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// An existing workspace to run in, by id.
        #[arg(long)]
        workspace: Option<String>,
        /// `key=value` for its environment; several allowed.
        #[arg(long)]
        env: Vec<String>,
        /// `key=value` labels; several allowed.
        #[arg(long)]
        label: Vec<String>,
        /// Return once it has started.
        #[arg(long, short = 'd', visible_alias = "background")]
        no_wait: bool,
        /// Stop waiting after this long: 90 (seconds), 30s, 5m, 1h.
        #[arg(long, value_parser = parse_duration)]
        wait_timeout: Option<std::time::Duration>,
        #[command(flatten)]
        target: Target,
    },
    /// Wait for an agent's turn to end, and print its last message (exit 2:
    /// it waits on a permission; 3: still working at --timeout).
    Wait {
        /// The agent: its id, a unique prefix of it, or its name.
        agent: String,
        /// Stop waiting after this long: 90 (seconds), 30s, 5m, 1h.
        #[arg(long, value_parser = parse_duration)]
        timeout: Option<std::time::Duration>,
        #[command(flatten)]
        target: Target,
    },
    /// What an agent did: its messages, tool calls and errors.
    Logs {
        /// The agent: its id, a unique prefix of it, or its name.
        agent: String,
        /// Only the last N entries.
        #[arg(long)]
        tail: Option<usize>,
        #[command(flatten)]
        target: Target,
    },
    /// Rename an agent.
    Rename {
        /// The agent: its id, a unique prefix of it, or its name.
        agent: String,
        /// Its new name.
        name: String,
        #[command(flatten)]
        target: Target,
    },
    /// Set labels on an agent (`key=value`, several or comma-separated).
    Label {
        /// The agent: its id, a unique prefix of it, or its name.
        agent: String,
        /// `key=value` pairs.
        #[arg(required = true)]
        labels: Vec<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Interrupt an agent's turn.
    Stop {
        /// The agent: its id, a unique prefix of it, or its name.
        agent: String,
        #[command(flatten)]
        target: Target,
    },
    /// Archive an agent, stopping it first if it is running.
    Archive {
        /// The agent: its id, a unique prefix of it, or its name.
        agent: String,
        #[command(flatten)]
        target: Target,
    },
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
    /// Speak MCP on stdio for an agent that takes only stdio servers,
    /// forwarding to Burrow's agent tools; without them, a server with no
    /// tools. Burrow hands agents this command itself; it is not for
    /// running by hand.
    McpBridge {
        /// The agent tools' URL (`http://<burrow>/mcp`). Without it,
        /// the bridge is a server with no tools.
        #[arg(long, env = "BURROW_MCP_URL")]
        url: Option<String>,
        /// The token to send. An agent's comes in the environment. Without
        /// one, the bridge sends the token Burrow keeps in `<home>/mcp-token`,
        /// that of a client that is not an agent.
        #[arg(long, env = "BURROW_MCP_TOKEN", hide_env_values = true)]
        token: Option<String>,
        /// Burrow's state directory, where `mcp-token` is. Defaults to
        /// $BURROW_HOME, then ~/.burrow.
        #[arg(long)]
        home: Option<PathBuf>,
    },
}

/// Which Burrow a command talks to.
#[derive(Debug, clap::Args)]
struct Target {
    /// The Burrow to talk to (`host` or `host:port`; port 6767 when left
    /// out), or `ssh://[user@]host[:port][?daemonPort=N]` through ssh.
    /// Defaults to this host's: the address in Paseo's daemon lock, then
    /// --listen or $BURROW_LISTEN, then 127.0.0.1:6797.
    #[arg(long, env = "PASEO_HOST")]
    host: Option<String>,
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

/// The `host:port` a command talks to: `--host` (or PASEO_HOST), else this
/// host's Burrow through Paseo's daemon lock, then `--listen`, then the
/// default.
fn endpoint(target: &Target, listen: &[SocketAddr]) -> String {
    let paseo_home = || paseo_lock::paseo_home(std::env::var_os("PASEO_HOME"), &user_home());
    let address = (target.host.clone())
        .filter(|host| !host.trim().is_empty())
        .or_else(|| paseo_lock::running_listen(&paseo_home()))
        .or_else(|| listen.first().map(SocketAddr::to_string))
        .unwrap_or_else(|| DEFAULT_LISTEN.to_string());
    burrow_daemon::client::endpoint(&address)
}

/// A wait limit: plain seconds, or a number with `s`, `m` or `h`.
fn parse_duration(given: &str) -> Result<std::time::Duration, String> {
    let given = given.trim();
    let (number, unit) = match given.char_indices().last() {
        Some((at, unit)) if unit.is_ascii_alphabetic() => (&given[..at], unit),
        _ => (given, 's'),
    };
    let number: f64 = number
        .trim()
        .parse()
        .map_err(|_| format!("not a duration: {given} (try 90, 30s, 5m or 1h)"))?;
    let seconds = match unit {
        's' => number,
        'm' => number * 60.,
        'h' => number * 3600.,
        _ => return Err(format!("not a duration: {given} (try 90, 30s, 5m or 1h)")),
    };
    if !seconds.is_finite() || seconds <= 0. {
        return Err(format!("a duration must be more than nothing: {given}"));
    }
    Ok(std::time::Duration::from_secs_f64(seconds))
}

/// The prompt `burrow send` was given, inline or from a file (`-`: stdin).
fn prompt_text(prompt: Option<String>, file: Option<PathBuf>) -> Result<String, String> {
    let text = match (prompt, file) {
        (Some(prompt), _) => prompt,
        (None, Some(path)) if path.as_os_str() == "-" => std::io::read_to_string(std::io::stdin())
            .map_err(|err| format!("reading the prompt: {err}"))?,
        (None, Some(path)) => std::fs::read_to_string(&path)
            .map_err(|err| format!("reading {}: {err}", path.display()))?,
        (None, None) => return Err("give a prompt, or --prompt-file".into()),
    };
    if text.trim().is_empty() {
        return Err("the prompt is empty".into());
    }
    Ok(text)
}

/// Runs one of the commands that talk to a running Burrow, prints what it
/// says, and exits with its code.
async fn command(command: Subcommand, listen: &[SocketAddr]) -> ! {
    use burrow_daemon::client::Client;
    use burrow_daemon::commands::{self, Failed};
    let (name, target) = match &command {
        Subcommand::Status { target } => ("status", target),
        Subcommand::Ls { target, .. } => ("ls", target),
        Subcommand::Inspect { target, .. } => ("inspect", target),
        Subcommand::Send { target, .. } => ("send", target),
        Subcommand::Run { target, .. } => ("run", target),
        Subcommand::Wait { target, .. } => ("wait", target),
        Subcommand::Logs { target, .. } => ("logs", target),
        Subcommand::Rename { target, .. } => ("rename", target),
        Subcommand::Label { target, .. } => ("label", target),
        Subcommand::Stop { target, .. } => ("stop", target),
        Subcommand::Archive { target, .. } => ("archive", target),
        Subcommand::ImportPaseo { .. } | Subcommand::McpBridge { .. } => {
            unreachable!("not a command that talks to a running Burrow")
        }
    };
    let endpoint = endpoint(target, listen);
    let outcome: Result<String, Failed> = async {
        if let Subcommand::Status { .. } = command {
            return Ok(burrow_daemon::status::fetch(&endpoint).await?.render());
        }
        let mut client = Client::connect(&endpoint).await?;
        let agents = commands::agents(&mut client).await?;
        let found = |key: &str| commands::find(&agents, key).cloned().map_err(Failed::from);
        // An agent acting on its own Burrow says who it is, as Paseo's CLI
        // does: a prompt shows as from it, an agent it starts is its child.
        let own = std::env::var("PASEO_HOST")
            .is_ok_and(|host| burrow_daemon::client::endpoint(&host) == endpoint);
        let caller = std::env::var("PASEO_AGENT_ID")
            .ok()
            .filter(|id| own && !id.is_empty());
        let said = match command {
            Subcommand::Ls {
                all,
                json,
                labels,
                fleet: true,
                ..
            } => {
                let filter = commands::LabelFilter::parse(&labels)?;
                let (agents, missing) = commands::fleet(&mut client, all, &filter).await?;
                let mut out = if json {
                    serde_json::to_string_pretty(&agents).unwrap_or_default() + "\n"
                } else if agents.is_empty() && !filter.is_empty() {
                    filter.none_shown()
                } else {
                    commands::list(&agents, all, time::OffsetDateTime::now_utc())
                };
                if !missing.is_empty() && !json {
                    out.push_str(&format!("Not listed: {}\n", missing.join("; ")));
                }
                Ok(out)
            }
            Subcommand::Ls {
                all, json, labels, ..
            } => {
                let filter = commands::LabelFilter::parse(&labels)?;
                let workspaces = if filter.workspace.is_empty() {
                    Default::default()
                } else {
                    commands::workspace_labels(&mut client).await?
                };
                let agents: Vec<serde_json::Value> = (agents.iter())
                    .filter(|a| filter.keeps(a, &workspaces))
                    .cloned()
                    .collect();
                let shown: Vec<&serde_json::Value> = (agents.iter())
                    .filter(|a| all || a["archivedAt"].is_null())
                    .collect();
                Ok(if json {
                    serde_json::to_string_pretty(&shown).unwrap_or_default() + "\n"
                } else if shown.is_empty() && !filter.is_empty() {
                    filter.none_shown()
                } else {
                    commands::list(&agents, all, time::OffsetDateTime::now_utc())
                })
            }
            Subcommand::Inspect { agent, json, .. } => found(&agent).map(|agent| {
                if json {
                    serde_json::to_string_pretty(&agent).unwrap_or_default() + "\n"
                } else {
                    commands::inspect(&agent)
                }
            }),
            Subcommand::Send {
                agent,
                prompt,
                prompt_file,
                no_wait,
                timeout,
                queue,
                interrupt,
                steer,
                ..
            } => {
                let agent = found(&agent)?;
                let prompt = prompt_text(prompt, prompt_file)?;
                let behavior = match (queue, interrupt, steer) {
                    (true, _, _) => "queue",
                    (_, true, _) => "interrupt",
                    (_, _, true) => "steer",
                    _ => "auto",
                };
                let wait = (!no_wait).then(|| timeout.map(std::time::Duration::from_secs));
                commands::send(
                    &mut client,
                    &agent,
                    &prompt,
                    caller.as_deref(),
                    behavior,
                    wait,
                )
                .await
            }
            Subcommand::Run {
                prompt,
                prompt_file,
                provider,
                model,
                thinking,
                mode,
                title,
                cwd,
                workspace,
                env,
                label,
                no_wait,
                wait_timeout,
                ..
            } => {
                let cwd = match cwd {
                    Some(cwd) => cwd,
                    None => std::env::current_dir()
                        .map_err(|err| format!("finding this directory: {err}"))?,
                };
                let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
                let run = commands::Run {
                    prompt: prompt_text(prompt, prompt_file)?,
                    provider,
                    model,
                    thinking,
                    mode,
                    title,
                    cwd: cwd.display().to_string(),
                    workspace,
                    env: commands::parse_pairs("--env", &env)?,
                    labels: commands::parse_pairs("--label", &label)?,
                    caller: caller.clone(),
                };
                commands::run(&mut client, &run, (!no_wait).then_some(wait_timeout)).await
            }
            Subcommand::Wait { agent, timeout, .. } => {
                let agent = found(&agent)?;
                commands::wait(&mut client, &agent, timeout).await
            }
            Subcommand::Logs { agent, tail, .. } => {
                let agent = found(&agent)?;
                commands::logs(&mut client, &agent, tail).await
            }
            Subcommand::Rename { agent, name, .. } => {
                let agent = found(&agent)?;
                commands::update(&mut client, &agent, Some(&name), &Default::default()).await
            }
            Subcommand::Label { agent, labels, .. } => {
                let agent = found(&agent)?;
                let labels = commands::parse_labels(&labels)?;
                commands::update(&mut client, &agent, None, &labels).await
            }
            Subcommand::Stop { agent, .. } => {
                let agent = found(&agent)?;
                commands::stop(&mut client, &agent).await
            }
            Subcommand::Archive { agent, .. } => {
                let agent = found(&agent)?;
                commands::archive(&mut client, &agent).await
            }
            _ => unreachable!("status returned above"),
        };
        client.close().await;
        said
    }
    .await;
    match outcome {
        Ok(text) => {
            // A reader that stops early (`burrow ls | head`) is not a failure.
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
            std::process::exit(0);
        }
        Err(Failed { message, code }) => {
            eprintln!("burrow {name}: {}", message.trim_end());
            std::process::exit(code);
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    // Before logging starts: stdout is the bridge's protocol.
    if let Some(Subcommand::McpBridge {
        url,
        token,
        home: given,
    }) = &args.command
    {
        // The home is only looked at, never moved from its old name: this
        // runs inside an agent's CLI.
        let state = given.clone().or_else(home::existing_home);
        let server = burrow_daemon::mcp::bridge::Server::resolve(
            url.clone(),
            token.clone(),
            state.as_deref(),
        );
        return tokio::task::spawn_blocking(move || burrow_daemon::mcp::bridge::run(server))
            .await?
            .context("bridging the agent tools");
    }
    // The commands that talk to a running Burrow print only what they say.
    match args.command {
        None | Some(Subcommand::ImportPaseo { .. }) | Some(Subcommand::McpBridge { .. }) => {}
        Some(other) => command(other, &args.listen).await,
    }
    // Read before logging starts, so a deprecated name is reported after.
    let log = env::resolve("BURROW_LOG", |key| std::env::var_os(key));
    let filter = log
        .as_ref()
        .and_then(|(value, _)| value.to_str())
        .and_then(|value| EnvFilter::try_new(value).ok())
        // rmcp logs every tool call's handshake at info.
        .unwrap_or_else(|| EnvFilter::new("info,rmcp=warn"));
    // A log line that cannot be written is dropped. tracing-subscriber would
    // otherwise report it on stderr with `eprintln!`, which panics when stderr
    // is gone too, as when Burrow was started from a terminal since closed:
    // every connection then died on its first log line. Colours only on a
    // terminal, so a log file reads plainly.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(std::io::stdout().is_terminal())
        .log_internal_errors(false)
        .init();
    if let Some((_, env::Source::Legacy(old))) = &log {
        env::warn_deprecated(old, "BURROW_LOG");
    }
    let state_home = args.home.clone().or_else(home::default_home);
    let discoveries = discoveries(&args, state_home.as_deref());
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
    let home = state_home;
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
        push_url: (!args.no_push)
            .then(|| (args.push_url).unwrap_or_else(|| burrow_daemon::push::EXPO_PUSH_URL.into())),
        mcp_tokenless: args.mcp_tokenless,
    };
    let daemon = Daemon::open(config).context("opening the state directory")?;
    if args.echo_provider {
        daemon
            .agents
            .register(std::sync::Arc::new(EchoProvider::default()));
    }
    if args.seed_demo && daemon.read(|store| store.projects.is_empty()) {
        let agent = seed::demo(&daemon);
        tracing::info!(agent, "seeded the demo directory");
    }
    // Providers are looked for while Burrow listens: some take many seconds
    // to say what they offer, and a client (or an upgrade script waiting for
    // the port) should not wait for them. Each is pushed to subscribers of
    // `providers_snapshot_update` once found; a request that needs one not
    // found yet waits for it.
    burrow_daemon::discovery::start(&daemon, discoveries).await;
    let running = burrow_daemon::start(daemon.clone())
        .await
        .with_context(|| format!("listening on {listen}"))?;
    let paseo_lock = (!args.no_paseo_lock)
        .then(|| {
            let home = paseo_lock::paseo_home(std::env::var_os("PASEO_HOME"), &user_home());
            paseo_lock::take_for(&home, &daemon, running.local_addr.port())
        })
        .flatten();
    let restart = tokio::select! {
        _ = stopped() => false,
        _ = daemon.restart.notified() => true,
    };
    tracing::info!(restart, "shutting down");
    running.shutdown().await;
    daemon.agents.stop_all(STOP_AGENTS_WITHIN).await;
    drop(paseo_lock);
    daemon.close().context("saving state")?;
    if restart {
        return Err(start_again());
    }
    Ok(())
}

/// Starts this program again in this process, with the arguments and
/// environment it was started with, so whatever runs it as a service (systemd,
/// launchd, an init script) still sees the same process. A program replaced
/// by an upgrade while it ran is started as the new one. Returns only on
/// failure.
#[cfg(unix)]
fn start_again() -> anyhow::Error {
    use std::os::unix::process::CommandExt as _;
    let program = match std::env::current_exe() {
        Ok(exe) => match exe.to_str().and_then(|e| e.strip_suffix(" (deleted)")) {
            Some(live) => PathBuf::from(live),
            None => exe,
        },
        Err(error) => return anyhow::anyhow!("restarting: cannot find this program: {error}"),
    };
    tracing::info!(program = %program.display(), "starting again");
    let error = std::process::Command::new(&program)
        .args(std::env::args_os().skip(1))
        .exec();
    anyhow::anyhow!("restarting {}: {error}", program.display())
}

#[cfg(not(unix))]
fn start_again() -> anyhow::Error {
    anyhow::anyhow!("restarting is done by this host's service manager")
}

/// How to find each provider not turned off, at startup and whenever its
/// CLI changes ([`burrow_daemon::discovery`]).
fn discoveries(args: &Args, home: Option<&std::path::Path>) -> Vec<Discovery> {
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
        // agy loads the agent tools only from a plugin in its own folder.
        let plugin = user_home().join(".gemini/config/plugins/burrow");
        found.push(Discovery::cli(
            id,
            cli,
            move || match AntigravityProvider::detect(binary.as_deref()) {
                Ok(agy) => {
                    tracing::info!(version = agy.version(), "offering Antigravity");
                    let agy = agy.with_plugin_dir(plugin.clone());
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
        // The extension that hands sessions the agent tools lives here.
        let extensions = home.map(|home| home.join("pi"));
        found.push(Discovery::cli("pi", cli, move || {
            let Some(mut pi) = PiProvider::detect(binary.as_deref()) else {
                tracing::info!("pi was not found; not offering it");
                return None;
            };
            if let Some(dir) = &extensions {
                pi = pi.with_extension_dir(dir.clone());
            }
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
    type AcpConfig = fn(Option<&std::path::Path>) -> burrow_provider_acp::AgentConfig;
    let acp: [(bool, Option<PathBuf>, &str, AcpConfig); 3] = [
        (
            args.no_cursor,
            args.cursor_bin.clone(),
            "cursor-agent",
            burrow_provider_acp::cursor,
        ),
        (
            args.no_opencode,
            args.opencode_bin.clone(),
            "opencode",
            burrow_provider_acp::opencode,
        ),
        (
            args.no_gemini,
            args.gemini_bin.clone(),
            "gemini",
            burrow_provider_acp::gemini,
        ),
    ];
    // OpenCode is built with Bun, which unpacks a library at each launch and
    // leaves it: those go under Burrow's home, where its probes clear them.
    let bun_tmpdir = home.map(|home| home.join("tmp").join("bun"));
    for (off, binary, default, config) in acp {
        if off {
            continue;
        }
        let cli = program(&binary, default);
        let id = config(binary.as_deref()).id;
        let bun_tmpdir = bun_tmpdir.clone().filter(|_| id == "opencode");
        found.push(Discovery::cli(id, cli, move || {
            let mut config = config(binary.as_deref());
            if let Some(dir) = &bun_tmpdir {
                config = burrow_provider_acp::with_bun_tmpdir(config, dir);
            }
            let label = config.label.clone();
            match AcpProvider::detect(config) {
                Some(agent) => {
                    tracing::info!(version = ?agent.version(), "offering {label}");
                    // Its models and modes come from a probe session, as in Paseo.
                    agent.refresh_in_background();
                    Some(Arc::new(agent) as Arc<dyn Provider>)
                }
                None => {
                    tracing::info!("{label}'s CLI was not found; not offering it");
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
