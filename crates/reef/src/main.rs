//! `reef`: keeps a Paseo fleet current and serves phones.
//!
//! `reef <hosts.json>` connects to every host in the file.
//! `reef --demo` serves a made-up fleet from fake daemons inside
//! the process instead, contacting nothing, to try Skiff.

use std::time::Duration;

use fleet::HostConfig;
use reef::config::{self, Device};
use reef::{ReefConfig, fake, legacy};
use serde_json::json;

#[tokio::main]
async fn main() {
    let args = match config::parse_args(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(problem) => {
            if !problem.is_empty() {
                eprintln!("reef: {problem}\n");
            }
            eprint!("{}", config::USAGE);
            std::process::exit(2);
        }
    };
    if args.version {
        println!("reef {}", reef::VERSION);
        return;
    }
    if let Err(problem) = run(args).await {
        eprintln!("reef: {problem}");
        std::process::exit(1);
    }
}

async fn run(args: config::Args) -> Result<(), String> {
    let data_dir = args.data_dir();
    // The demo keeps nothing on disk.
    if !args.demo {
        for line in legacy::migrate(&args)? {
            println!("reef: {line}");
        }
    }
    let (hosts, client_id) = if args.demo {
        (demo_fleet().await?, paseo_client::generate_client_id())
    } else {
        let path = args.hosts_file.as_deref().ok_or("no hosts file given")?;
        (config::read_hosts(path)?, config::client_id(&data_dir)?)
    };
    let devices = match &args.token {
        // An open Reef asks no phone for a token.
        None if args.open => Vec::new(),
        Some(token) => vec![Device {
            name: "command line".into(),
            token: token.clone(),
        }],
        None if args.demo => vec![Device {
            name: "demo".into(),
            token: config::new_token()?,
        }],
        None => {
            let (devices, made) = config::devices(&data_dir)?;
            if let Some(device) = made {
                println!("New device token for \"{}\": {}", device.name, device.token);
                println!(
                    "It is kept in {}.",
                    config::tokens_path(&data_dir).display()
                );
            } else {
                println!(
                    "{} device token(s) in {}.",
                    devices.len(),
                    config::tokens_path(&data_dir).display()
                );
            }
            devices
        }
    };
    if args.open {
        eprintln!(
            "reef: open: no device token needed; anyone who can reach {} can act on agents.",
            args.bind.ip()
        );
    } else if !args.bind.ip().is_loopback() {
        eprintln!(
            "reef: listening on {}: any device on that network with a token can act on agents.",
            args.bind.ip()
        );
    }
    let reef_config = ReefConfig {
        open: args.open,
        bind: args.bind,
        // The demo keeps its settings in memory.
        data_dir: (!args.demo).then(|| data_dir.clone()),
        hosts_file: args.hosts_file.clone().filter(|_| !args.demo),
        host_names: args.host_names.clone(),
        usage: args.usage.clone(),
        ..ReefConfig::new(hosts, client_id, devices.clone())
    };
    let names: Vec<String> = reef_config.hosts.iter().map(|h| h.name.clone()).collect();
    let handle = reef::start(reef_config)
        .await
        .map_err(|err| format!("cannot listen on {}: {err}", args.bind))?;
    println!("reef {} listening on http://{}", reef::VERSION, handle.addr);
    println!("Hosts: {}", names.join(", "));
    if args.open {
        println!("Open http://{} to use it.", handle.addr);
    } else if args.demo || args.token.is_some() {
        println!(
            "Open http://{}/#token={} on this machine to try it.",
            handle.addr, devices[0].token
        );
    }
    handle.wait().await;
    Ok(())
}

/// Made-up accounts' usage, as three of the demo daemons report it. One
/// account is on two, so it shows once; one failed its latest read.
fn demo_usage(daemon: &fake::FakeDaemon, prefix: &str) {
    const HOUR: i64 = 3_600;
    let shared = fake::usage_report(
        "codex:team",
        "codex",
        "team@example.com",
        "Pro",
        &[
            ("five_hour", "5h", 64., 2 * HOUR),
            ("seven_day", "Weekly", 88., 30 * HOUR),
        ],
    );
    let reports = match prefix {
        "a" => vec![
            fake::usage_report(
                "claude:work",
                "claude",
                "sam@work.example",
                "Max",
                &[
                    ("five_hour", "5h", 22., 3 * HOUR),
                    ("seven_day", "Weekly", 41., 4 * 24 * HOUR),
                ],
            ),
            shared,
        ],
        "b" => vec![
            fake::usage_report(
                "claude:home",
                "claude",
                "sam@home.example",
                "Pro",
                &[
                    ("five_hour", "5h", 7., HOUR),
                    ("seven_day", "Weekly", 63., 2 * 24 * HOUR),
                ],
            ),
            fake::usage_report(
                "gemini:home",
                "gemini",
                "sam@home.example",
                "AI Pro",
                &[("daily", "Daily", 12., 9 * HOUR)],
            ),
            shared,
        ],
        // An account whose latest read failed, as Paseo 0.12 reports it:
        // its last good numbers, from seven minutes before.
        "c" => vec![fake::usage_failure(
            &fake::usage_report(
                "codex:personal",
                "codex",
                "sam@home.example",
                "Plus",
                &[
                    ("five_hour", "5h", 35., 4 * HOUR),
                    ("seven_day", "Weekly", 52., 3 * 24 * HOUR),
                ],
            ),
            "Rate limited",
            Some(7 * 60),
        )],
        _ => return,
    };
    daemon.set_usage_reports(reports);
}

/// Two fake daemons with the test fleet, and a little activity: every
/// few seconds the running agent streams a sentence.
async fn demo_fleet() -> Result<Vec<HostConfig>, String> {
    let mut hosts = Vec::new();
    let mut daemons = Vec::new();
    // Four hosts and about ninety agents, enough to show grouping and a
    // board of columns.
    for (name, prefix, many) in [
        ("demo-a", "a", 14),
        ("demo-b", "b", 22),
        ("demo-c", "c", 26),
        ("demo-d", "d", 20),
    ] {
        let daemon = fake::FakeDaemon::start(name)
            .await
            .map_err(|err| format!("demo daemon: {err}"))?;
        if prefix == "a" {
            // A Paseo 0.12 host, with a background workspace.
            daemon.set_background_workspaces(true);
            fake::seed(&daemon, prefix);
            fake::seed_agent_messages(&daemon, prefix);
            fake::seed_background(&daemon, prefix);
        }
        fake::seed_many(&daemon, prefix, many);
        demo_usage(&daemon, prefix);
        hosts.push(HostConfig {
            id: reef::config::host_id(name),
            name: name.into(),
            endpoint: Some(daemon.endpoint()),
            password: None,
        });
        daemons.push(daemon);
    }
    let daemon = daemons[0].clone();
    // Now and then an agent elsewhere runs a turn and replies, so the
    // newest-first list moves.
    let others = daemons[1..].to_vec();
    tokio::spawn(async move {
        let mut n = 0usize;
        loop {
            tokio::time::sleep(Duration::from_secs(9)).await;
            n += 1;
            let daemon = &others[n % others.len()];
            let prefix = ["b", "c", "d"][n % others.len()];
            let agent = format!("agt_{prefix}_m{}", (n * 5) % 12);
            daemon.start_turn(&agent, &fleet::clock::now_iso());
            tokio::time::sleep(Duration::from_secs(4)).await;
            let reply = [
                "Checked again: still green.",
                "Pushed the follow-up fix.",
                "Waiting on review now.",
            ][n % 3];
            daemon.finish_turn(&agent, &fleet::clock::now_iso(), reply);
        }
    });
    tokio::spawn(async move {
        let words =
            "Reading the contributing notes, then the build steps, then the first-issue labels. ";
        let mut n = 0u64;
        // A turn of a few words every half minute, so turn-finished
        // notifications show too.
        loop {
            tokio::time::sleep(Duration::from_secs(3)).await;
            n += 1;
            let at = fleet::clock::now_iso();
            match n % 10 {
                0 => daemon.start_turn("agt_a_run", &at),
                9 => daemon.finish_turn("agt_a_run", &at, "Finished another pass over the guide."),
                _ => {
                    let word = words.split(' ').nth((n % 12) as usize).unwrap_or("and");
                    daemon.stream_row(
                        "agt_a_run",
                        &daemon.current_turn("agt_a_run"),
                        &at,
                        json!({"type": "assistant_message", "text": format!("{word} "),
                               "messageId": format!("demo-reply-{}", n / 10)}),
                    );
                }
            }
        }
    });
    Ok(hosts)
}
