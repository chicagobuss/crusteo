//! Paseo's schedules (`schedule/*` requests, `paseo schedule`, `paseo
//! heartbeat`): a prompt sent on a cadence to an agent, or each time to a
//! new agent in a new workspace. As Paseo's `schedule/` service:
//!
//! - **Cadence:** every N ms, or a five-field cron (`*`, lists, ranges and
//!   steps; no names) matched minute by minute in an IANA time zone, UTC
//!   by default.
//! - **Runs:** every second, an active schedule whose `nextRunAt` has come
//!   claims that slot and moves its cursor to the next one after now, so a
//!   slot runs once and a long run never queues more; one run at a time per
//!   schedule. A slot missed while Burrow was down is skipped, and a run it
//!   was in the middle of is failed at the next start.
//! - **An agent target** gets the prompt as a system notice (a
//!   `<paseo-system>` envelope, a `heartbeat` row in its timeline), which
//!   steers a running turn; the run waits for the turn to end. An agent
//!   that is busy fails the run; one that is gone or archived completes the
//!   schedule, as does archiving it later.
//! - **A new-agent target** gets a new workspace for its `cwd` (or a new
//!   worktree of it), an agent labelled with the schedule and run, and the
//!   prompt; the workspace is archived after unless `archiveOnFinish` is
//!   false.
//! - `maxRuns` and `expiresAt` complete a schedule; a completed one stays
//!   listed until deleted.
//!
//! Each schedule is `<home>/schedules/<id>.json` in Paseo's format, so
//! `burrow import-paseo` copies Paseo's over as they are.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use burrow_protocol::envelope::{rpc_error, session};
use serde::de::{DeserializeOwned, Deserializer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::watch;
use tracing::{info, warn};

use crate::agent_requests::Late;
use crate::agents::{self, CreateAgent};
use crate::clock;
use crate::daemon::Daemon;

/// The requests this module answers.
pub const KINDS: &[&str] = &[
    "schedule/create",
    "schedule/list",
    "schedule/inspect",
    "schedule/logs",
    "schedule/pause",
    "schedule/resume",
    "schedule/delete",
    "schedule/run-once",
    "schedule/update",
];

/// The labels a new-agent run's agent carries.
pub const SCHEDULE_LABEL: &str = "paseo.schedule-id";
pub const RUN_LABEL: &str = "paseo.schedule-run";

/// `rpc_error`'s code for a schedule request that failed.
const FAILED: &str = "schedule_request_failed";

const TICK: Duration = Duration::from_secs(1);

// --- the records -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Cadence {
    Every {
        #[serde(rename = "everyMs")]
        every_ms: i64,
    },
    Cron {
        expression: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timezone: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Target {
    Agent {
        #[serde(rename = "agentId")]
        agent_id: String,
    },
    /// `{provider, cwd, modeId?, model?, thinkingOptionId?, archiveOnFinish?,
    /// isolation?, title?, ...}`, kept whole.
    NewAgent { config: Map<String, Value> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Active,
    Paused,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunStatus {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub scheduled_for: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: RunStatus,
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
}

/// `StoredScheduleSchema`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    pub name: Option<String>,
    pub prompt: String,
    pub cadence: Cadence,
    pub target: Target,
    pub status: Status,
    pub created_at: String,
    pub updated_at: String,
    pub next_run_at: Option<String>,
    pub last_run_at: Option<String>,
    pub paused_at: Option<String>,
    pub expires_at: Option<String>,
    pub max_runs: Option<u64>,
    pub runs: Vec<Run>,
}

impl Schedule {
    /// The schedule without its runs, as lists and most replies send it.
    pub fn summary(&self) -> Value {
        let mut value = serde_json::to_value(self).unwrap_or(Value::Null);
        if let Value::Object(map) = &mut value {
            map.remove("runs");
        }
        value
    }

    fn finished_runs(&self) -> u64 {
        self.runs
            .iter()
            .filter(|run| run.status != RunStatus::Running)
            .count() as u64
    }

    /// Expired, or out of runs.
    fn due_to_complete(&self, now: i64) -> bool {
        if (self.expires_at.as_deref())
            .and_then(clock::parse_iso_ms)
            .is_some_and(|at| at <= now)
        {
            return true;
        }
        self.max_runs.is_some_and(|max| self.finished_runs() >= max)
    }

    fn complete(&mut self, now: i64) {
        self.status = Status::Completed;
        self.next_run_at = None;
        self.paused_at = None;
        self.updated_at = clock::iso_from_ms(now);
    }

    fn next_ms(&self) -> Option<i64> {
        self.next_run_at.as_deref().and_then(clock::parse_iso_ms)
    }
}

// --- cadence -----------------------------------------------------------------

/// A parsed five-field cron expression, each field a set of values as bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cron {
    minute: u64,
    hour: u64,
    day_of_month: u64,
    month: u64,
    day_of_week: u64,
}

/// Paseo's `parseCronExpression`, with its messages.
pub fn parse_cron(expression: &str) -> Result<Cron, String> {
    let parts: Vec<&str> = expression.split_whitespace().collect();
    if parts.len() != 5 {
        return Err("Cron expressions must have 5 fields".into());
    }
    Ok(Cron {
        minute: field(parts[0], 0, 59, "minute")?,
        hour: field(parts[1], 0, 23, "hour")?,
        day_of_month: field(parts[2], 1, 31, "day-of-month")?,
        month: field(parts[3], 1, 12, "month")?,
        day_of_week: field(parts[4], 0, 6, "day-of-week")?,
    })
}

fn field(source: &str, min: u32, max: u32, name: &str) -> Result<u64, String> {
    let number = |text: &str| -> Option<u32> {
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    let mut allowed = 0u64;
    for part in source.trim().split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("Invalid cron {name} field"));
        }
        let (base, step) = match part.split_once('/') {
            None => (part, 1),
            Some((base, step)) => {
                // Written as it reads: "5", not "05".
                let text = step.trim();
                let step = number(text)
                    .filter(|n| *n > 0 && n.to_string() == text)
                    .ok_or_else(|| format!("Invalid cron {name} step"))?;
                (base, step)
            }
        };
        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((from, to)) = base.split_once('-') {
            match (number(from), number(to)) {
                (Some(from), Some(to)) if from <= to && from >= min && to <= max => (from, to),
                (Some(_), Some(_)) => return Err(format!("Invalid cron {name} range")),
                _ => return Err(format!("Invalid cron {name} value")),
            }
        } else {
            match number(base) {
                Some(value) if (min..=max).contains(&value) => (value, value),
                _ => return Err(format!("Invalid cron {name} value")),
            }
        };
        let mut value = start;
        while value <= end {
            allowed |= 1 << value;
            value += step;
        }
    }
    Ok(allowed)
}

fn time_zone(name: Option<&str>) -> Result<jiff::tz::TimeZone, String> {
    match name {
        None => Ok(jiff::tz::TimeZone::UTC),
        Some(name) => {
            jiff::tz::TimeZone::get(name).map_err(|_| format!("Invalid cron time zone: {name}"))
        }
    }
}

/// Paseo's `validateScheduleCadence`.
pub fn validate(cadence: &Cadence) -> Result<(), String> {
    match cadence {
        Cadence::Every { every_ms } if *every_ms <= 0 => {
            Err("everyMs must be a positive integer".into())
        }
        Cadence::Every { .. } => Ok(()),
        Cadence::Cron {
            expression,
            timezone,
        } => {
            parse_cron(expression)?;
            time_zone(timezone.as_deref()).map(|_| ())
        }
    }
}

/// Paseo's `computeNextRunAt`: `after` plus the interval, or the first
/// whole minute after `after` that the cron matches, in its time zone.
pub fn next_run(cadence: &Cadence, after: i64) -> Result<i64, String> {
    let (expression, timezone) = match cadence {
        Cadence::Every { every_ms } => return Ok(after + every_ms),
        Cadence::Cron {
            expression,
            timezone,
        } => (expression, timezone),
    };
    let cron = parse_cron(expression)?;
    let tz = time_zone(timezone.as_deref())?;
    let mut cursor = after - after.rem_euclid(60_000) + 60_000;
    for _ in 0..366 * 24 * 60 {
        let at = jiff::Timestamp::from_millisecond(cursor)
            .map_err(|err| err.to_string())?
            .to_zoned(tz.clone());
        let matches = |set: u64, value: i64| set & (1 << value) != 0;
        if matches(cron.minute, at.minute().into())
            && matches(cron.hour, at.hour().into())
            && matches(cron.day_of_month, at.day().into())
            && matches(cron.month, at.month().into())
            && matches(
                cron.day_of_week,
                at.weekday().to_sunday_zero_offset().into(),
            )
        {
            return Ok(cursor);
        }
        cursor += 60_000;
    }
    Err(format!(
        "Unable to compute next run time for cron expression: {expression}"
    ))
}

/// The first slot from `next` on that is after `now`.
fn advance_past(cadence: &Cadence, mut next: i64, now: i64) -> Result<i64, String> {
    while next <= now {
        next = next_run(cadence, next)?;
    }
    Ok(next)
}

// --- the store ---------------------------------------------------------------

/// Every schedule, kept in `<home>/schedules/` when there is a home.
pub struct Schedules {
    dir: Option<PathBuf>,
    by_id: Mutex<BTreeMap<String, Schedule>>,
    /// Schedules with a run under way.
    running: Mutex<HashSet<String>>,
}

impl Schedules {
    pub fn open(home: Option<&Path>) -> Self {
        let dir = home.map(|home| home.join("schedules"));
        let by_id = dir.as_deref().map(read_dir).unwrap_or_default();
        Self {
            dir,
            by_id: Mutex::new(by_id),
            running: Mutex::new(HashSet::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Schedule>> {
        self.by_id.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Every schedule, oldest first.
    pub fn list(&self) -> Vec<Schedule> {
        let mut all: Vec<Schedule> = self.lock().values().cloned().collect();
        all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        all
    }

    pub fn get(&self, id: &str) -> Option<Schedule> {
        self.lock().get(id).cloned()
    }

    pub(crate) fn get_or_not_found(&self, id: &str) -> Result<Schedule, String> {
        self.get(id).ok_or_else(|| not_found(id))
    }

    fn insert(&self, schedule: Schedule) -> Schedule {
        let mut all = self.lock();
        save(self.dir.as_deref(), &schedule);
        all.insert(schedule.id.clone(), schedule.clone());
        schedule
    }

    /// Changes one schedule; saved when `change` succeeds and changed it.
    fn update(
        &self,
        id: &str,
        change: impl FnOnce(&mut Schedule) -> Result<(), String>,
    ) -> Result<Schedule, String> {
        let mut all = self.lock();
        let schedule = all.get_mut(id).ok_or_else(|| not_found(id))?;
        let mut changed = schedule.clone();
        change(&mut changed)?;
        if changed != *schedule {
            save(self.dir.as_deref(), &changed);
            *schedule = changed;
        }
        Ok(schedule.clone())
    }

    pub(crate) fn remove(&self, id: &str) {
        let mut all = self.lock();
        if all.remove(id).is_some()
            && let Some(dir) = &self.dir
            && file_safe(id)
        {
            let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        }
    }

    /// Marks a run of `id` as under way; false if one already is.
    fn reserve(&self, id: &str) -> bool {
        (self.running.lock().unwrap_or_else(|p| p.into_inner())).insert(id.to_string())
    }

    fn release(&self, id: &str) {
        (self.running.lock().unwrap_or_else(|p| p.into_inner())).remove(id);
    }

    fn is_running(&self, id: &str) -> bool {
        (self.running.lock().unwrap_or_else(|p| p.into_inner())).contains(id)
    }

    /// Paseo's `completeForAgent`: an archived agent's schedules end.
    pub fn complete_for_agent(&self, agent_id: &str) -> usize {
        let now = clock::now_ms();
        let ids: Vec<String> = (self.lock().values())
            .filter(|s| s.status != Status::Completed)
            .filter(|s| matches!(&s.target, Target::Agent { agent_id: a } if a == agent_id))
            .map(|s| s.id.clone())
            .collect();
        for id in &ids {
            let _ = self.update(id, |s| {
                s.complete(now);
                Ok(())
            });
        }
        ids.len()
    }
}

fn not_found(id: &str) -> String {
    format!("Schedule not found: {id}")
}

/// Ids are Paseo's eight hex digits; never let one name a path elsewhere.
fn file_safe(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The schedules in `dir`; files that are not one are skipped, as Paseo
/// skips them.
pub fn read_dir(dir: &Path) -> BTreeMap<String, Schedule> {
    let mut all = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return all;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Schedule>(&text).ok());
        match parsed {
            Some(schedule) => {
                all.insert(schedule.id.clone(), schedule);
            }
            None => warn!(path = %path.display(), "not a schedule; skipped"),
        }
    }
    all
}

fn save(dir: Option<&Path>, schedule: &Schedule) {
    let Some(dir) = dir else {
        return;
    };
    if let Err(err) = write(dir, schedule) {
        warn!(%err, id = schedule.id, "could not save a schedule");
    }
}

/// Writes `<dir>/<id>.json` through a temporary file.
pub fn write(dir: &Path, schedule: &Schedule) -> std::io::Result<()> {
    if !file_safe(&schedule.id) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("schedule id {:?} is unfit for a file name", schedule.id),
        ));
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{}.json", schedule.id));
    let temp = dir.join(format!("{}.json.tmp", schedule.id));
    std::fs::write(&temp, serde_json::to_string_pretty(schedule)? + "\n")?;
    std::fs::rename(&temp, path)
}

fn new_id(taken: &BTreeMap<String, Schedule>) -> String {
    loop {
        let id: String = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        if !taken.contains_key(&id) {
            return id;
        }
    }
}

// --- the state changes, as Paseo makes them ------------------------------------

/// Claims `schedule`'s due slot: a running run for it, and the cursor
/// moved to the next slot after now (none after a final run). `None` when
/// nothing is due.
fn claim(schedule: &mut Schedule, now: i64) -> Result<Option<String>, String> {
    let Some(next) = schedule.next_ms() else {
        return Ok(None);
    };
    if schedule.status != Status::Active || schedule.due_to_complete(now) || next > now {
        return Ok(None);
    }
    let scheduled_for = schedule.next_run_at.clone().unwrap_or_default();
    let last = schedule
        .max_runs
        .is_some_and(|max| schedule.finished_runs() + 1 >= max);
    schedule.next_run_at = if last {
        None
    } else {
        let after = next_run(&schedule.cadence, next)?;
        Some(clock::iso_from_ms(advance_past(
            &schedule.cadence,
            after,
            now,
        )?))
    };
    let run_id = uuid::Uuid::new_v4().to_string();
    schedule.updated_at = clock::iso_from_ms(now);
    schedule.runs.push(running(&run_id, scheduled_for, now));
    Ok(Some(run_id))
}

fn running(id: &str, scheduled_for: String, now: i64) -> Run {
    Run {
        id: id.to_string(),
        scheduled_for,
        started_at: clock::iso_from_ms(now),
        ended_at: None,
        status: RunStatus::Running,
        agent_id: None,
        workspace_id: None,
        output: None,
        error: None,
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub ok: bool,
    pub agent_id: Option<String>,
    pub output: Option<String>,
    pub error: Option<String>,
    /// The target is gone for good: the schedule completes.
    pub gone: bool,
}

/// Paseo's `finishRun`.
fn finish(schedule: &mut Schedule, run_id: &str, outcome: &Outcome, manual: bool, now: i64) {
    let stamp = clock::iso_from_ms(now);
    let mut scheduled_for = None;
    for run in schedule.runs.iter_mut().filter(|run| run.id == run_id) {
        run.status = if outcome.ok {
            RunStatus::Succeeded
        } else {
            RunStatus::Failed
        };
        run.ended_at = Some(stamp.clone());
        if outcome.agent_id.is_some() {
            run.agent_id = outcome.agent_id.clone();
        }
        run.output = outcome.output.clone();
        run.error = outcome.error.clone();
        scheduled_for = clock::parse_iso_ms(&run.scheduled_for);
    }
    schedule.last_run_at = Some(stamp.clone());
    schedule.updated_at = stamp;
    if outcome.gone {
        schedule.complete(now);
    } else if schedule.status == Status::Completed || manual {
        // Completed meanwhile, or a manual run: the cadence is left alone.
    } else if schedule.due_to_complete(now) {
        schedule.complete(now);
    } else if schedule.status == Status::Paused {
        schedule.next_run_at = None;
    } else {
        // A final slot has no successor until its run limit is raised; move
        // a cursor a long run left behind past now.
        let next = match (schedule.next_ms(), scheduled_for) {
            (Some(next), _) => Ok(next),
            (None, Some(slot)) => next_run(&schedule.cadence, slot),
            (None, None) => Ok(now),
        };
        match next.and_then(|next| advance_past(&schedule.cadence, next, now)) {
            Ok(next) => schedule.next_run_at = Some(clock::iso_from_ms(next)),
            Err(err) => warn!(id = schedule.id, %err, "no next run"),
        }
    }
}

/// At start: a run Burrow stopped in the middle of fails, and an overdue
/// cursor moves past now, so a missed slot is skipped. Returns the
/// workspaces of interrupted new-agent runs to archive.
fn recover(schedule: &mut Schedule, now: i64) -> Vec<String> {
    let mut archive = Vec::new();
    let keep_workspace = match &schedule.target {
        Target::NewAgent { config } => config["archiveOnFinish"] == false,
        Target::Agent { .. } => true,
    };
    let stamp = clock::iso_from_ms(now);
    let mut interrupted = false;
    for run in schedule
        .runs
        .iter_mut()
        .filter(|r| r.status == RunStatus::Running)
    {
        run.status = RunStatus::Failed;
        run.ended_at = Some(stamp.clone());
        run.error = Some("Daemon restarted before the scheduled run completed".into());
        if let Some(workspace) = &run.workspace_id
            && (run.agent_id.is_none() || !keep_workspace)
            && matches!(schedule.target, Target::NewAgent { .. })
        {
            archive.push(workspace.clone());
        }
        interrupted = true;
    }
    if interrupted {
        schedule.updated_at = stamp.clone();
        if schedule.due_to_complete(now) {
            schedule.complete(now);
        }
    }
    if schedule.status == Status::Active
        && let Some(next) = schedule.next_ms().filter(|next| *next <= now)
    {
        let moved = next_run(&schedule.cadence, next)
            .and_then(|after| advance_past(&schedule.cadence, after, now));
        if let Ok(moved) = moved {
            schedule.next_run_at = Some(clock::iso_from_ms(moved));
            schedule.updated_at = stamp;
        }
    }
    archive
}

// --- running them --------------------------------------------------------------

/// Starts the schedules: recovers what a stop interrupted, ends those of
/// agents that are gone, then looks for due runs every second.
pub async fn run_loop(daemon: Arc<Daemon>, mut shutdown: watch::Receiver<bool>) {
    start(&daemon).await;
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => due(&daemon),
            _ = shutdown.changed() => return,
        }
    }
}

async fn start(daemon: &Arc<Daemon>) {
    let now = clock::now_ms();
    let mut archive = Vec::new();
    for schedule in daemon.schedules.list() {
        let _ = daemon.schedules.update(&schedule.id, |s| {
            archive.extend(recover(s, now));
            // Paseo's `sweepOrphanedSchedules`: an agent deleted or
            // archived while Burrow was down never fires again.
            if let Target::Agent { agent_id } = &s.target
                && s.status != Status::Completed
                && !daemon.read(|store| store.agents.get(agent_id).is_some())
            {
                s.complete(now);
            }
            Ok(())
        });
    }
    for workspace in archive {
        if let Err(error) = crate::workspace_requests::archive_workspace(daemon, &workspace).await {
            warn!(workspace, %error, "an interrupted run's workspace was not archived");
        }
    }
    let count = daemon.schedules.list().len();
    if count > 0 {
        info!(count, "schedules loaded");
    }
}

/// One tick: completes what is due to, and starts each due run.
fn due(daemon: &Arc<Daemon>) {
    let now = clock::now_ms();
    for schedule in daemon.schedules.list() {
        if schedule.status != Status::Active || daemon.schedules.is_running(&schedule.id) {
            continue;
        }
        let Some(next) = schedule.next_ms() else {
            continue;
        };
        if schedule.due_to_complete(now) {
            let _ = daemon.schedules.update(&schedule.id, |s| {
                if s.status == Status::Active && s.due_to_complete(now) {
                    s.complete(now);
                }
                Ok(())
            });
            continue;
        }
        if next > now || !daemon.schedules.reserve(&schedule.id) {
            continue;
        }
        tokio::spawn(run_schedule(daemon.clone(), schedule.id, now, false));
    }
}

/// One run of a schedule already reserved: claims its slot (or, run by
/// hand, just adds a run), runs it, records how it went.
async fn run_schedule(daemon: Arc<Daemon>, id: String, now: i64, manual: bool) {
    let mut run_id = None;
    let claimed = daemon.schedules.update(&id, |s| {
        run_id = if manual {
            let id = uuid::Uuid::new_v4().to_string();
            s.updated_at = clock::iso_from_ms(now);
            s.runs.push(running(&id, clock::iso_from_ms(now), now));
            Some(id)
        } else {
            claim(s, now)?
        };
        Ok(())
    });
    match (claimed, run_id) {
        (Ok(schedule), Some(run_id)) => {
            info!(id, run = run_id, "schedule fired");
            let outcome = execute(&daemon, &schedule, &run_id).await;
            if let Some(error) = &outcome.error {
                warn!(id, run = run_id, error, "scheduled run failed");
            }
            let finished = clock::now_ms();
            let _ = daemon.schedules.update(&id, |s| {
                finish(s, &run_id, &outcome, manual, finished);
                Ok(())
            });
        }
        (Err(err), _) => warn!(id, %err, "schedule not run"),
        (Ok(_), None) => {}
    }
    daemon.schedules.release(&id);
}

fn failed(error: String, gone: bool) -> Outcome {
    Outcome {
        ok: false,
        agent_id: None,
        output: None,
        error: Some(error),
        gone,
    }
}

/// What the prompt is sent as, with the run's identity, as Paseo's
/// `buildScheduleFireBody`.
fn fire_body(schedule: &Schedule, run_id: &str) -> String {
    let heading = match &schedule.name {
        Some(name) => format!(
            "Schedule \"{name}\" fired (id={}, run={run_id}).",
            schedule.id
        ),
        None => format!("Schedule fired (id={}, run={run_id}).", schedule.id),
    };
    format!("{heading}\n{}", schedule.prompt)
}

async fn execute(daemon: &Arc<Daemon>, schedule: &Schedule, run_id: &str) -> Outcome {
    match &schedule.target {
        Target::Agent { agent_id } => to_agent(daemon, schedule, run_id, agent_id).await,
        Target::NewAgent { config } => to_new_agent(daemon, schedule, run_id, config).await,
    }
}

async fn to_agent(
    daemon: &Arc<Daemon>,
    schedule: &Schedule,
    run_id: &str,
    agent_id: &str,
) -> Outcome {
    let (agent, archived) = daemon.read(|store| {
        (
            store.agents.get(agent_id).cloned(),
            store.archived.contains_key(agent_id),
        )
    });
    let agent = match agent {
        Some(agent) => agent,
        None if archived => return failed(format!("Agent {agent_id} is archived"), true),
        None => return failed(format!("Agent {agent_id} no longer exists"), true),
    };
    let busy = agent.active_turn.is_some()
        || matches!(
            agent.status,
            burrow_protocol::directory::AgentStatus::Running
        );
    if busy {
        return failed(format!("Agent {agent_id} already has an active run"), false);
    }
    if let Err(error) = agents::send_system(daemon, agent_id, &fire_body(schedule, run_id)).await {
        return failed(error, false);
    }
    waited(
        agents::wait_for_finish(daemon, agent_id, None).await,
        agent_id,
    )
}

/// The run's end from `wait_for_finish`'s answer.
fn waited(answer: Value, agent_id: &str) -> Outcome {
    let last = (answer["lastMessage"].as_str())
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    let error = match answer["status"].as_str() {
        Some("permission") => Some(format!(
            "Scheduled agent {agent_id} is waiting for permission"
        )),
        Some("error") | Some("timeout") => Some(
            last.clone()
                .or_else(|| answer["error"].as_str().map(str::to_string))
                .unwrap_or_else(|| format!("Scheduled agent {agent_id} failed")),
        ),
        _ => None,
    };
    Outcome {
        ok: error.is_none(),
        agent_id: Some(agent_id.to_string()),
        output: if error.is_none() { last } else { None },
        error,
        gone: false,
    }
}

async fn to_new_agent(
    daemon: &Arc<Daemon>,
    schedule: &Schedule,
    run_id: &str,
    config: &Map<String, Value>,
) -> Outcome {
    let text = |key: &str| {
        (config.get(key).and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let cwd = text("cwd").unwrap_or_default();
    let path = Path::new(&cwd);
    if !path.is_dir() {
        let why = if path.exists() {
            format!("Working directory {cwd} is not a directory")
        } else {
            format!("Working directory {cwd} no longer exists")
        };
        return failed(why, true);
    }
    let worktree = config.get("isolation").and_then(Value::as_str) == Some("worktree");
    let made =
        crate::workspace_requests::workspace_for_run(daemon, &cwd, worktree, &schedule.prompt)
            .await;
    let (workspace_id, directory) = match made {
        Ok(made) => made,
        Err(error) => return failed(error, false),
    };
    let record = |agent: Option<&str>| {
        let _ = daemon.schedules.update(&schedule.id, |s| {
            for run in s.runs.iter_mut() {
                if run.id == run_id && run.status == RunStatus::Running {
                    run.workspace_id = Some(workspace_id.clone());
                    run.agent_id = agent.map(str::to_string);
                }
            }
            Ok(())
        });
    };
    record(None);
    let labels = BTreeMap::from([
        (SCHEDULE_LABEL.to_string(), schedule.id.clone()),
        (RUN_LABEL.to_string(), run_id.to_string()),
    ]);
    let request = CreateAgent {
        provider: text("provider").unwrap_or_default(),
        cwd: directory,
        model: text("model"),
        mode_id: text("modeId"),
        thinking_option_id: text("thinkingOptionId"),
        title: text("title"),
        workspace_id: Some(workspace_id.clone()),
        labels,
        initial_prompt: Some(schedule.prompt.clone()),
        client_message_id: None,
        images: Vec::new(),
        system_prompt: config
            .get("systemPrompt")
            .and_then(Value::as_str)
            .map(str::to_string),
        env: BTreeMap::new(),
        background: false,
        agent_id: None,
        caller_agent_id: None,
    };
    let outcome = match agents::create(daemon, request).await {
        Ok(agent) => {
            record(Some(&agent.id));
            waited(
                agents::wait_for_finish(daemon, &agent.id, None).await,
                &agent.id,
            )
        }
        Err(error) => failed(error, false),
    };
    let keep = outcome.agent_id.is_some() && config.get("archiveOnFinish") == Some(&false.into());
    if !keep
        && let Err(error) =
            crate::workspace_requests::archive_workspace(daemon, &workspace_id).await
    {
        warn!(workspace_id, %error, "a scheduled run's workspace was not archived");
    }
    outcome
}

// --- requests ------------------------------------------------------------------

/// `null` or a value, told apart from the field being left out.
fn double<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum CreateTarget {
    #[serde(rename = "self")]
    Myself {
        #[serde(rename = "agentId")]
        agent_id: String,
    },
    Agent {
        #[serde(rename = "agentId")]
        agent_id: String,
    },
    NewAgent {
        config: Map<String, Value>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateRequest {
    prompt: String,
    #[serde(default)]
    name: Option<String>,
    cadence: Cadence,
    target: CreateTarget,
    #[serde(default)]
    max_runs: Option<i64>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    run_on_create: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdRequest {
    schedule_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UpdateRequest {
    schedule_id: String,
    #[serde(default, deserialize_with = "double")]
    name: Option<Option<String>>,
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    cadence: Option<Cadence>,
    #[serde(default)]
    new_agent_config: Option<Map<String, Value>>,
    #[serde(default, deserialize_with = "double")]
    max_runs: Option<Option<i64>>,
    #[serde(default, deserialize_with = "double")]
    expires_at: Option<Option<String>>,
}

fn prompt_of(prompt: &str) -> Result<String, String> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err("Schedule prompt is required".into());
    }
    Ok(prompt.to_string())
}

fn name_of(name: Option<String>) -> Option<String> {
    name.map(|n| n.trim().to_string()).filter(|n| !n.is_empty())
}

fn max_runs_of(value: Option<i64>) -> Result<Option<u64>, String> {
    match value {
        None => Ok(None),
        Some(n) if n > 0 => Ok(Some(n as u64)),
        Some(_) => Err("maxRuns must be a positive integer".into()),
    }
}

fn check_new_agent(config: &Map<String, Value>) -> Result<(), String> {
    for key in ["provider", "cwd"] {
        if (config.get(key).and_then(Value::as_str)).is_none_or(|s| s.trim().is_empty()) {
            return Err(format!("new-agent target needs a {key}"));
        }
    }
    Ok(())
}

/// `schedule/create`. A `self` target (`paseo heartbeat`) is the agent.
fn create(daemon: &Daemon, request: CreateRequest) -> Result<Schedule, String> {
    let prompt = prompt_of(&request.prompt)?;
    validate(&request.cadence)?;
    let target = match request.target {
        CreateTarget::Myself { agent_id } | CreateTarget::Agent { agent_id } => {
            Target::Agent { agent_id }
        }
        CreateTarget::NewAgent { config } => {
            check_new_agent(&config)?;
            Target::NewAgent { config }
        }
    };
    let max_runs = max_runs_of(request.max_runs)?;
    let now = clock::now_ms();
    let run_now =
        (request.run_on_create).unwrap_or(matches!(request.cadence, Cadence::Every { .. }));
    let next = if run_now {
        now
    } else {
        next_run(&request.cadence, now)?
    };
    let id = new_id(&daemon.schedules.lock());
    let stamp = clock::iso_from_ms(now);
    Ok(daemon.schedules.insert(Schedule {
        id,
        name: name_of(request.name),
        prompt,
        cadence: request.cadence,
        target,
        status: Status::Active,
        created_at: stamp.clone(),
        updated_at: stamp,
        next_run_at: Some(clock::iso_from_ms(next)),
        last_run_at: None,
        paused_at: None,
        expires_at: request.expires_at,
        max_runs,
        runs: Vec::new(),
    }))
}

/// Paseo's `createOrReplace`, which the agent tools use: a named schedule
/// with the same target that has not completed is refreshed in place (an
/// agent that registers its heartbeat again does not get two), else one
/// is made.
pub(crate) fn create_or_replace(
    daemon: &Daemon,
    name: Option<String>,
    prompt: &str,
    cadence: Cadence,
    target: Target,
    max_runs: Option<u64>,
    expires_at: Option<String>,
) -> Result<Schedule, String> {
    let prompt = prompt_of(prompt)?;
    validate(&cadence)?;
    let name = name_of(name);
    let now = clock::now_ms();
    let next_after = |cadence: &Cadence| -> Result<i64, String> {
        match cadence {
            Cadence::Every { .. } => Ok(now),
            Cadence::Cron { .. } => next_run(cadence, now),
        }
    };
    let same = name.as_ref().and_then(|name| {
        (daemon.schedules.list().into_iter()).find(|s| {
            s.status != Status::Completed && s.name.as_ref() == Some(name) && s.target == target
        })
    });
    if let Some(existing) = same {
        return daemon.schedules.update(&existing.id, |s| {
            let cadence = merge_time_zone(&s.cadence, cadence);
            s.next_run_at = Some(clock::iso_from_ms(next_after(&cadence)?));
            s.cadence = cadence;
            s.prompt = prompt;
            s.status = Status::Active;
            s.paused_at = None;
            s.expires_at = expires_at;
            s.max_runs = max_runs;
            s.updated_at = clock::iso_from_ms(now);
            Ok(())
        });
    }
    let next = next_after(&cadence)?;
    let id = new_id(&daemon.schedules.lock());
    let stamp = clock::iso_from_ms(now);
    Ok(daemon.schedules.insert(Schedule {
        id,
        name,
        prompt,
        cadence,
        target,
        status: Status::Active,
        created_at: stamp.clone(),
        updated_at: stamp,
        next_run_at: Some(clock::iso_from_ms(next)),
        last_run_at: None,
        paused_at: None,
        expires_at,
        max_runs,
        runs: Vec::new(),
    }))
}

/// `schedule/update` from its request's JSON fields (`scheduleId`, …).
pub(crate) fn update_from(daemon: &Daemon, fields: Value) -> Result<Schedule, String> {
    let request: UpdateRequest = serde_json::from_value(fields).map_err(|e| e.to_string())?;
    update(daemon, request)
}

pub(crate) fn pause(daemon: &Daemon, id: &str) -> Result<Schedule, String> {
    daemon.schedules.update(id, |s| {
        match s.status {
            Status::Completed => return Err(format!("Schedule {id} is already completed")),
            Status::Paused => return Ok(()),
            Status::Active => {}
        }
        let stamp = clock::now_iso();
        s.status = Status::Paused;
        s.next_run_at = None;
        s.paused_at = Some(stamp.clone());
        s.updated_at = stamp;
        Ok(())
    })
}

pub(crate) fn resume(daemon: &Daemon, id: &str) -> Result<Schedule, String> {
    daemon.schedules.update(id, |s| {
        match s.status {
            Status::Completed => return Err(format!("Schedule {id} is already completed")),
            Status::Active => return Ok(()),
            Status::Paused => {}
        }
        let now = clock::now_ms();
        s.status = Status::Active;
        s.paused_at = None;
        s.next_run_at = Some(clock::iso_from_ms(next_run(&s.cadence, now)?));
        s.updated_at = clock::iso_from_ms(now);
        Ok(())
    })
}

/// A cron given without a time zone keeps the one it had.
fn merge_time_zone(current: &Cadence, next: Cadence) -> Cadence {
    match (current, next) {
        (
            Cadence::Cron {
                timezone: Some(zone),
                ..
            },
            Cadence::Cron {
                expression,
                timezone: None,
            },
        ) => Cadence::Cron {
            expression,
            timezone: Some(zone.clone()),
        },
        (_, next) => next,
    }
}

fn update(daemon: &Daemon, request: UpdateRequest) -> Result<Schedule, String> {
    let id = request.schedule_id.clone();
    daemon.schedules.update(&id, |s| {
        let now = clock::now_ms();
        if let Some(prompt) = &request.prompt {
            s.prompt = prompt_of(prompt)?;
        }
        if let Some(name) = request.name {
            s.name = name_of(name);
        }
        if let Some(cadence) = request.cadence {
            let cadence = merge_time_zone(&s.cadence, cadence);
            validate(&cadence)?;
            s.next_run_at = match s.status {
                Status::Active => Some(clock::iso_from_ms(next_run(&cadence, now)?)),
                _ => None,
            };
            s.cadence = cadence;
        }
        if let Some(patch) = request.new_agent_config {
            let Target::NewAgent { config } = &mut s.target else {
                return Err(
                    "new-agent config updates are only valid for new-agent target schedules".into(),
                );
            };
            patch_config(config, patch)?;
        }
        if let Some(max_runs) = request.max_runs {
            s.max_runs = max_runs_of(max_runs)?;
        }
        if let Some(expires_at) = request.expires_at {
            s.expires_at = expires_at;
        }
        s.updated_at = clock::iso_from_ms(now);
        Ok(())
    })
}

/// Paseo's `applyNewAgentConfig`.
fn patch_config(config: &mut Map<String, Value>, patch: Map<String, Value>) -> Result<(), String> {
    for (key, value) in patch {
        match key.as_str() {
            "provider" | "cwd" => {
                let text = value.as_str().map(str::trim).unwrap_or_default();
                if text.is_empty() {
                    return Err(format!("{key} cannot be empty"));
                }
                config.insert(key, text.into());
            }
            "model" | "modeId" | "thinkingOptionId" => {
                match value.as_str().map(str::trim).filter(|s| !s.is_empty()) {
                    Some(text) => {
                        config.insert(key, text.into());
                    }
                    None => {
                        config.remove(&key);
                    }
                }
            }
            "archiveOnFinish" | "isolation" => {
                config.insert(key, value);
            }
            _ => {}
        }
    }
    Ok(())
}

/// `schedule/run-once`: a run now, outside the cadence. Answers when it
/// has finished, with the schedule.
pub(crate) async fn run_once(daemon: &Arc<Daemon>, id: &str) -> Result<Schedule, String> {
    let schedule = daemon.schedules.get_or_not_found(id)?;
    if schedule.status == Status::Completed {
        return Err(format!("Schedule {id} is already completed"));
    }
    if !daemon.schedules.reserve(id) {
        return Err(format!("Schedule {id} is already running"));
    }
    run_schedule(daemon.clone(), id.to_string(), clock::now_ms(), true).await;
    daemon.schedules.get_or_not_found(id)
}

pub fn handle(
    daemon: &Arc<Daemon>,
    kind: &str,
    message: &Value,
    late: &Late,
) -> Option<Vec<Value>> {
    fn parse<T: DeserializeOwned>(message: &Value) -> Option<T> {
        T::deserialize(message).ok()
    }
    let request_id = message.get("requestId")?.as_str()?.to_string();
    let reply = |result: Result<Value, String>, response: &str| -> Vec<Value> {
        match result {
            Ok(mut payload) => {
                payload["requestId"] = request_id.clone().into();
                payload["error"] = Value::Null;
                vec![session(response, payload)]
            }
            Err(error) => vec![rpc_error(&request_id, Some(kind), &error, FAILED)],
        }
    };
    let id = || parse::<IdRequest>(message).map(|r| r.schedule_id);
    let frames = match kind {
        "schedule/create" => {
            let request = parse(message)?;
            reply(
                create(daemon, request).map(|s| json!({"schedule": s.summary()})),
                "schedule/create/response",
            )
        }
        "schedule/list" => {
            let all: Vec<Value> = daemon
                .schedules
                .list()
                .iter()
                .map(Schedule::summary)
                .collect();
            reply(Ok(json!({"schedules": all})), "schedule/list/response")
        }
        "schedule/inspect" => reply(
            (daemon.schedules.get_or_not_found(&id()?)).map(|s| json!({"schedule": s})),
            "schedule/inspect/response",
        ),
        "schedule/logs" => reply(
            daemon.schedules.get_or_not_found(&id()?).map(|s| {
                let mut runs = s.runs;
                runs.sort_by(|a, b| a.started_at.cmp(&b.started_at));
                json!({"runs": runs})
            }),
            "schedule/logs/response",
        ),
        "schedule/pause" => reply(
            pause(daemon, &id()?).map(|s| json!({"schedule": s.summary()})),
            "schedule/pause/response",
        ),
        "schedule/resume" => reply(
            resume(daemon, &id()?).map(|s| json!({"schedule": s.summary()})),
            "schedule/resume/response",
        ),
        "schedule/delete" => {
            let id = id()?;
            daemon.schedules.remove(&id);
            reply(Ok(json!({"scheduleId": id})), "schedule/delete/response")
        }
        "schedule/update" => reply(
            update(daemon, parse(message)?).map(|s| json!({"schedule": s})),
            "schedule/update/response",
        ),
        "schedule/run-once" => {
            let id = id()?;
            let (daemon, late, kind) = (daemon.clone(), late.clone(), kind.to_string());
            tokio::spawn(async move {
                let frame = match run_once(&daemon, &id).await {
                    Ok(s) => session(
                        "schedule/run-once/response",
                        json!({"requestId": request_id, "schedule": s, "error": null}),
                    ),
                    Err(error) => rpc_error(&request_id, Some(&kind), &error, FAILED),
                };
                let _ = late.send(frame);
            });
            Vec::new()
        }
        _ => return None,
    };
    Some(frames)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(stamp: &str) -> i64 {
        clock::parse_iso_ms(stamp).unwrap()
    }

    fn cron(expression: &str, timezone: Option<&str>) -> Cadence {
        Cadence::Cron {
            expression: expression.into(),
            timezone: timezone.map(str::to_string),
        }
    }

    #[test]
    fn cron_fields_parse_as_paseos_do() {
        assert!(parse_cron("*/15 9-17 * * 1-5").is_ok());
        assert!(parse_cron("0 9,17 * * *").is_ok());
        assert!(parse_cron("0 0 1,15 */2 0").is_ok());
        let error = |e: &str| parse_cron(e).unwrap_err();
        assert_eq!(error("* * * *"), "Cron expressions must have 5 fields");
        assert_eq!(error("60 * * * *"), "Invalid cron minute value");
        assert_eq!(error("* 5-2 * * *"), "Invalid cron hour range");
        assert_eq!(error("* * 0 * *"), "Invalid cron day-of-month value");
        assert_eq!(error("*/0 * * * *"), "Invalid cron minute step");
        assert_eq!(error("*/05 * * * *"), "Invalid cron minute step");
        assert_eq!(error("* * * JAN *"), "Invalid cron month value");
        assert_eq!(error("* * * * 7"), "Invalid cron day-of-week value");
        assert_eq!(error("1,,2 * * * *"), "Invalid cron minute field");
        assert_eq!(
            validate(&cron("0 9 * * *", Some("Mars/Olympus"))).unwrap_err(),
            "Invalid cron time zone: Mars/Olympus"
        );
    }

    #[test]
    fn the_next_run_is_the_next_matching_minute_in_its_time_zone() {
        let at = ms("2026-10-18T13:30:20.000Z");
        // 09:00 and 17:00 in Chicago, which is UTC-5 in October.
        let chicago = cron("0 9,17 * * *", Some("America/Chicago"));
        assert_eq!(next_run(&chicago, at), Ok(ms("2026-10-18T14:00:00.000Z")));
        assert_eq!(
            next_run(&chicago, ms("2026-10-18T14:00:00.000Z")),
            Ok(ms("2026-10-18T22:00:00.000Z"))
        );
        // After the clocks go back (1 November), 09:00 is 15:00 UTC.
        assert_eq!(
            next_run(&chicago, ms("2026-11-02T00:00:00.000Z")),
            Ok(ms("2026-11-02T15:00:00.000Z"))
        );
        // UTC without a zone; never the minute it starts in.
        assert_eq!(
            next_run(&cron("30 13 * * *", None), at),
            Ok(ms("2026-10-19T13:30:00.000Z"))
        );
        // Weekdays: 18 October 2026 is a Sunday.
        assert_eq!(
            next_run(&cron("0 8 * * 1", None), at),
            Ok(ms("2026-10-19T08:00:00.000Z"))
        );
        assert_eq!(
            next_run(&Cadence::Every { every_ms: 90_000 }, at),
            Ok(at + 90_000)
        );
        assert!(next_run(&cron("0 0 31 2 *", None), at).is_err(), "never");
    }

    fn schedule(cadence: Cadence, next: &str) -> Schedule {
        Schedule {
            id: "0a1b2c3d".into(),
            name: None,
            prompt: "check in".into(),
            cadence,
            target: Target::Agent {
                agent_id: "agent".into(),
            },
            status: Status::Active,
            created_at: "2026-10-18T00:00:00.000Z".into(),
            updated_at: "2026-10-18T00:00:00.000Z".into(),
            next_run_at: Some(next.into()),
            last_run_at: None,
            paused_at: None,
            expires_at: None,
            max_runs: None,
            runs: Vec::new(),
        }
    }

    fn ok() -> Outcome {
        Outcome {
            ok: true,
            agent_id: Some("agent".into()),
            output: Some("done".into()),
            error: None,
            gone: false,
        }
    }

    #[test]
    fn a_due_slot_runs_once_and_the_cursor_skips_to_after_now() {
        let mut s = schedule(cron("*/10 * * * *", None), "2026-10-18T10:00:00.000Z");
        // Picked up 25 minutes late: the 10:10 and 10:20 slots are skipped.
        let now = ms("2026-10-18T10:25:00.000Z");
        let run = claim(&mut s, now).unwrap().expect("due");
        assert_eq!(s.next_run_at.as_deref(), Some("2026-10-18T10:30:00.000Z"));
        assert_eq!(s.runs[0].scheduled_for, "2026-10-18T10:00:00.000Z");
        assert_eq!(s.runs[0].status, RunStatus::Running);
        assert_eq!(claim(&mut s, now).unwrap(), None, "not due again");
        finish(&mut s, &run, &ok(), false, now + 5_000);
        assert_eq!(s.runs[0].status, RunStatus::Succeeded);
        assert_eq!(s.runs[0].output.as_deref(), Some("done"));
        assert_eq!(s.next_run_at.as_deref(), Some("2026-10-18T10:30:00.000Z"));
        // A run that outlasts its next slot moves the cursor past it.
        let late = ms("2026-10-18T10:31:00.000Z");
        let run = claim(&mut s, late).unwrap().unwrap();
        finish(&mut s, &run, &ok(), false, ms("2026-10-18T10:52:00.000Z"));
        assert_eq!(s.next_run_at.as_deref(), Some("2026-10-18T11:00:00.000Z"));
    }

    #[test]
    fn run_limits_and_expiry_complete_a_schedule_and_a_gone_target_too() {
        let mut s = schedule(
            Cadence::Every { every_ms: 60_000 },
            "2026-10-18T10:00:00.000Z",
        );
        s.max_runs = Some(1);
        let now = ms("2026-10-18T10:00:01.000Z");
        let run = claim(&mut s, now).unwrap().unwrap();
        assert_eq!(s.next_run_at, None, "the last run has no next");
        finish(&mut s, &run, &ok(), false, now);
        assert_eq!(s.status, Status::Completed);

        let mut s = schedule(
            Cadence::Every { every_ms: 60_000 },
            "2026-10-18T10:00:00.000Z",
        );
        s.expires_at = Some("2026-10-18T09:00:00.000Z".into());
        assert_eq!(claim(&mut s, now).unwrap(), None);

        let mut s = schedule(
            Cadence::Every { every_ms: 60_000 },
            "2026-10-18T10:00:00.000Z",
        );
        let run = claim(&mut s, now).unwrap().unwrap();
        let gone = Outcome {
            gone: true,
            ..failed("Agent agent no longer exists".into(), true)
        };
        finish(&mut s, &run, &gone, false, now);
        assert_eq!(s.status, Status::Completed);
        assert_eq!(s.runs[0].status, RunStatus::Failed);
        assert_eq!(
            s.runs[0].error.as_deref(),
            Some("Agent agent no longer exists")
        );
    }

    #[test]
    fn a_run_cut_short_by_a_restart_fails_and_a_missed_slot_is_skipped() {
        let mut s = schedule(cron("0 * * * *", None), "2026-10-18T10:00:00.000Z");
        let run = claim(&mut s, ms("2026-10-18T10:00:02.000Z"))
            .unwrap()
            .unwrap();
        // Burrow comes back at 13:20.
        recover(&mut s, ms("2026-10-18T13:20:00.000Z"));
        let cut = s.runs.iter().find(|r| r.id == run).unwrap();
        assert_eq!(cut.status, RunStatus::Failed);
        assert_eq!(
            cut.error.as_deref(),
            Some("Daemon restarted before the scheduled run completed")
        );
        assert_eq!(s.next_run_at.as_deref(), Some("2026-10-18T14:00:00.000Z"));
    }

    #[test]
    fn paseos_files_read_back_as_they_were_written() {
        let paseo = r#"{
          "id": "4f2a9c1e", "name": "Release check-in", "prompt": "Daily delivery",
          "cadence": {"type": "cron", "expression": "0 9,17 * * *", "timezone": "America/Chicago"},
          "target": {"type": "agent", "agentId": "0ab50468-1876-4910-8207-5acf73a3f5fa"},
          "status": "paused", "createdAt": "2026-10-18T04:38:37.607Z",
          "updatedAt": "2026-10-19T05:53:43.528Z", "nextRunAt": null,
          "lastRunAt": "2026-10-18T22:00:52.530Z", "pausedAt": "2026-10-19T05:53:43.528Z",
          "expiresAt": "2026-11-09T04:38:37.590Z", "maxRuns": 44,
          "runs": [{"id": "r1", "scheduledFor": "2026-10-18T14:00:00.000Z",
                    "startedAt": "2026-10-18T14:00:00.410Z", "endedAt": "2026-10-18T14:03:12.000Z",
                    "status": "succeeded", "agentId": "0ab50468-1876-4910-8207-5acf73a3f5fa",
                    "output": "ok", "error": null}]
        }"#;
        let s: Schedule = serde_json::from_str(paseo).unwrap();
        assert_eq!(s.status, Status::Paused);
        let back = serde_json::to_value(&s).unwrap();
        let original: Value = serde_json::from_str(paseo).unwrap();
        assert_eq!(back, original);
        assert!(s.summary().get("runs").is_none());
        let new_agent: Target = serde_json::from_value(json!({"type": "new-agent",
            "config": {"provider": "claude", "cwd": "/tmp", "providerOptions": {"x": 1}}}))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&new_agent).unwrap()["config"]["providerOptions"]["x"],
            1
        );
    }
}
