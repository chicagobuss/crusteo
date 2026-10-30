//! Providers found again while the daemon runs.
//!
//! Coding agents' CLIs are upgraded all the time, often by themselves, and
//! new models come with them. Each CLI's `--version` is read every
//! [`CHECK_EVERY`]; when it changed (or a CLI turned up), the provider is
//! found again with its models and replaces the old one, and clients get a
//! `providers_snapshot_update`. Every [`PROBE_EVERY`], and when a client asks
//! (`refresh_providers_snapshot_request`), providers are probed again even
//! when the CLI is the same: an account can be offered a new model without an
//! upgrade.
//!
//! New sessions use the new provider. A running agent keeps its CLI until
//! it is idle and gets its next prompt, which restarts the CLI on the same
//! provider session ([`crate::agents::send`]).

use std::sync::Arc;
use std::time::Duration;

use burrow_provider::Discovery;
use tracing::{info, warn};

use crate::agents::publish_providers;
use crate::daemon::Daemon;

/// How often each CLI's `--version` is read.
pub const CHECK_EVERY: Duration = Duration::from_secs(5 * 60);
/// How often providers are probed again with the CLI unchanged.
pub const PROBE_EVERY: Duration = Duration::from_secs(60 * 60);
/// How often providers still finding their models are looked at.
const LOADING_POLL: Duration = Duration::from_millis(250);

/// One provider's discovery, and what its CLI's `--version` said last.
pub struct Watched {
    discovery: Arc<Discovery>,
    stamp: Option<String>,
}

/// Finds each provider now and offers those there, then keeps looking at
/// them for as long as the daemon runs.
pub async fn start(daemon: &Arc<Daemon>, discoveries: Vec<Discovery>) {
    let mut watched = Vec::new();
    for discovery in discoveries {
        let discovery = Arc::new(discovery);
        let (stamp, found) = blocking(&discovery, true).await;
        if let Some(provider) = found {
            daemon.agents.register(provider);
        }
        watched.push(Watched { discovery, stamp });
    }
    *daemon.agents.watched.lock().await = watched;
    let daemon = Arc::downgrade(daemon);
    tokio::spawn(async move {
        let probe_every = (PROBE_EVERY.as_secs() / CHECK_EVERY.as_secs()).max(1);
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(CHECK_EVERY).await;
            let Some(daemon) = daemon.upgrade() else {
                return;
            };
            ticks += 1;
            recheck(&daemon, None, ticks.is_multiple_of(probe_every)).await;
        }
    });
}

/// Each provider looked for: its id, its CLI as given, and what the CLI's
/// `--version` said last (`None`: it would not run).
pub async fn watched(daemon: &Daemon) -> Vec<(String, Option<std::path::PathBuf>, Option<String>)> {
    (daemon.agents.watched.lock().await.iter())
        .map(|w| {
            (
                w.discovery.id.clone(),
                w.discovery.program.clone(),
                w.stamp.clone(),
            )
        })
        .collect()
}

/// Looks at the providers (`only` those named), finding again those whose
/// CLI changed, or all of them when `probe`. Publishes the providers
/// snapshot when what they offer changed, or always when `announce`.
pub async fn recheck(daemon: &Arc<Daemon>, only: Option<&[String]>, probe: bool) {
    recheck_announcing(daemon, only, probe, false).await;
}

/// `refresh_providers_snapshot_request`: probes the providers named, or
/// all, and pushes the snapshot once they are done, changed or not.
pub async fn refresh(daemon: &Arc<Daemon>, only: Option<&[String]>) {
    recheck_announcing(daemon, only, true, true).await;
}

async fn recheck_announcing(
    daemon: &Arc<Daemon>,
    only: Option<&[String]>,
    probe: bool,
    announce: bool,
) {
    // Held throughout, so two rechecks never find the same CLI at once.
    let mut watched = daemon.agents.watched.lock().await;
    let before = daemon.agents.infos();
    for entry in watched
        .iter_mut()
        .filter(|w| only.is_none_or(|ids| ids.contains(&w.discovery.id)))
    {
        let id = entry.discovery.id.clone();
        let (stamp, _) = blocking(&entry.discovery, false).await;
        let changed = stamp != entry.stamp;
        match &stamp {
            None if changed => {
                warn!(
                    provider = id,
                    "its CLI no longer runs; still offering what it had"
                )
            }
            None => {}
            Some(_) if !changed && !probe => {}
            Some(now) => {
                if changed {
                    info!(provider = id, was = ?entry.stamp, now, "its CLI changed; finding it again");
                }
                match blocking(&entry.discovery, true).await.1 {
                    Some(provider) => daemon.agents.register(provider),
                    None => warn!(
                        provider = id,
                        "could not find it again; still offering what it had"
                    ),
                }
            }
        }
        entry.stamp = stamp;
    }
    drop(watched);
    while daemon.agents.any_loading() {
        tokio::time::sleep(LOADING_POLL).await;
    }
    if announce || daemon.agents.infos() != before {
        publish_providers(daemon);
    }
}

/// The discovery's stamp and, when `find`, the provider, off the runtime's
/// threads: both run the CLI.
async fn blocking(
    discovery: &Arc<Discovery>,
    find: bool,
) -> (Option<String>, Option<Arc<dyn burrow_provider::Provider>>) {
    let discovery = discovery.clone();
    tokio::task::spawn_blocking(move || {
        let stamp = (discovery.stamp)();
        let found = (find && stamp.is_some())
            .then(|| (discovery.find)())
            .flatten();
        (stamp, found)
    })
    .await
    .unwrap_or((None, None))
}
