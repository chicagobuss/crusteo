//! Reef as the fleet's recorder: it follows every agent, turns each new
//! timeline entry into a `fleet-event/1` record, scrubs it, and hands it to
//! each configured sink (`record.json`).
//!
//! Burrow's timeline is the buffer. Each sink has a cursor per agent that
//! moves only when the sink takes a batch, so after a sink, Tracker or Reef
//! itself was down, recording resumes from Burrow where it stopped. The
//! connection side (fetching pages, following agents) is in
//! `upstream/recording.rs`; this is the state and the decisions.

pub mod config;
pub mod event;
pub mod redact;
pub mod sink;

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use config::{Begin, Config, SinkConfig};
use event::{Context, Record};
use redact::Chain;
use sink::{Batch, Outcome};

/// An agent: host name and agent id.
pub type Key = (String, String);

/// While an agent's turn runs, it is read at most this often.
const EVERY: Duration = Duration::from_secs(3);
/// After a failed read, how long before the next.
const AFTER_ERROR: Duration = Duration::from_secs(10);
/// Reads in flight across the fleet, so a Reef starting with thousands of
/// agents to catch up asks a few at a time.
pub const READS: usize = 8;
/// Records a sink may have waiting before Reef drops them and reads them
/// from Burrow again once the sink catches up.
const QUEUE_MAX: usize = 50_000;
/// Records a sink may have waiting before a long backlog stops being read on
/// until the sink has taken some: history is read as fast as the sinks take
/// it, not faster, so reading it never fills the queue that drops it again.
const HIGH_WATER: usize = QUEUE_MAX / 2;
/// How long the rest of a backlog waits when a sink is past [`HIGH_WATER`].
const BEHIND_FOR: Duration = Duration::from_secs(1);
/// Agents asked whether they were deleted, at once.
pub const CHECKS: usize = 4;
/// After a check that could not be made, how long before the next.
const CHECK_AGAIN: Duration = Duration::from_secs(30);
/// An agent found archived is not asked about again for this long. A
/// daemon says nothing when an archived agent is deleted, so it is looked at
/// again now and then, and always when Reef starts.
const ARCHIVED_FOR: Duration = Duration::from_secs(3600);

/// Where in an agent's timeline a sink got to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub epoch: String,
    pub seq: u64,
}

/// A read to make: from a cursor, or the tail, to start from now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Read {
    After(Cursor),
    Tail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Saved {
    host: String,
    agent: String,
    #[serde(flatten)]
    cursor: Cursor,
    /// The agent's `updatedAt` when the sink caught up; a Reef starting
    /// again reads only agents that changed since.
    #[serde(default)]
    updated: Option<String>,
}

enum Item {
    Record(serde_json::Value),
    /// Everything before this in the queue takes the agent to `cursor`.
    Mark(Key, Cursor, Option<String>),
    /// A `forget` record. Once the sink takes it, the sink has no cursor for
    /// the agent. Counts and travels as a record.
    Forget(Key, serde_json::Value),
}

impl Item {
    /// The record this item sends, if it sends one.
    fn record(&self) -> Option<&serde_json::Value> {
        match self {
            Item::Record(value) | Item::Forget(_, value) => Some(value),
            Item::Mark(..) => None,
        }
    }
}

/// Agents that left a daemon's active list, and are asked about: archived
/// (nothing to say), or deleted (each sink is told to forget it)? A daemon
/// sends the same `remove` for both, so Reef asks for the agent by id: an
/// archived one is still there, a deleted one is not.
#[derive(Default)]
struct Checks {
    /// In the order they will be asked about.
    waiting: VecDeque<Key>,
    /// What is in `waiting`.
    queued: HashSet<Key>,
    /// Asked, and no answer yet.
    asking: HashSet<Key>,
    /// Removed again while being asked about: the answer may be old.
    again: HashSet<Key>,
    /// After a check that could not be made: not before.
    retry_at: HashMap<Key, Instant>,
    /// How many checks in a row the daemon failed to answer, so a daemon
    /// that cannot is asked less and less, and is complained of once.
    failures: HashMap<Key, u32>,
    /// Agents found archived, and when.
    archived: HashMap<Key, Instant>,
}

struct Sink {
    config: SinkConfig,
    chain: Chain,
    /// Which worker this is; answers from an older one are dropped.
    generation: u64,
    tx: mpsc::Sender<Batch>,
    cursors: HashMap<Key, (Cursor, Option<String>)>,
    queue: VecDeque<Item>,
    records: usize,
    inflight: Option<(u64, Vec<Item>)>,
    next_batch: u64,
    retry_at: Option<Instant>,
    last_error: Option<String>,
    save_due: bool,
}

impl Sink {
    fn wants(&self, record: &Record, context: &Context) -> bool {
        let config = &self.config;
        (config.kinds.is_empty() || config.kinds.contains(&record.kind))
            && (config.hosts.is_empty() || config.hosts.contains(&context.host))
            && (config.labels.is_empty()
                || context
                    .labels()
                    .any(|label| config.labels.iter().any(|l| l == label)))
    }
}

/// The turn an agent is in, as far as Reef has read, for its
/// `turn_finished` record.
#[derive(Default)]
struct Turn {
    id: String,
    prompt: Option<String>,
    reply: Option<String>,
    tools: BTreeMap<String, u32>,
    seen: HashSet<u64>,
    started: Value,
    ended: Value,
    last_seq: u64,
    /// Reef read it from its prompt; false when it began before Reef did.
    whole: bool,
}

#[derive(Default)]
struct Follow {
    dirty: bool,
    /// In the queue of agents waiting for a read.
    queued: bool,
    urgent: bool,
    fetching: Option<Read>,
    last_read: Option<Instant>,
    not_before: Option<Instant>,
    /// How far Reef has read, this run.
    through: Option<Cursor>,
    turn: Option<Turn>,
    /// Reading the agent's history again from its beginning, with no turn in
    /// progress, for the sinks named: those starting from the start. The
    /// other sinks are owed no summary of an old turn by it.
    backfill: HashSet<String>,
}

pub struct Recorder {
    path: Option<PathBuf>,
    data_dir: PathBuf,
    seen: Option<(SystemTime, u64)>,
    pub config: Config,
    chain: Chain,
    sinks: Vec<Sink>,
    agents: HashMap<Key, Follow>,
    /// Agents owed a read, oldest first.
    waiting: VecDeque<Key>,
    pub reads: usize,
    next_generation: u64,
    apply: mpsc::UnboundedSender<crate::upstream::Apply>,
    checks: Checks,
}

impl Recorder {
    /// A recorder reading `record.json` in `data_dir`; with none, records
    /// nothing until configured (tests configure it with [`Self::set`]).
    pub fn new(
        data_dir: Option<&Path>,
        apply: mpsc::UnboundedSender<crate::upstream::Apply>,
    ) -> Self {
        Self {
            path: data_dir.map(|dir| dir.join(config::FILE)),
            data_dir: data_dir.map(Path::to_path_buf).unwrap_or_default(),
            seen: None,
            config: Config::default(),
            chain: Chain::new(&Default::default()).expect("the default chain"),
            sinks: Vec::new(),
            agents: HashMap::new(),
            waiting: VecDeque::new(),
            reads: 0,
            next_generation: 0,
            apply,
            checks: Checks::default(),
        }
    }

    /// Whether Reef follows every agent: recording is on and has a sink.
    pub fn following(&self) -> bool {
        self.config.follow_all && !self.sinks.is_empty()
    }

    /// Reads `record.json` again if it changed; true when it applied new
    /// settings. A broken file keeps the settings Reef had.
    pub fn reload(&mut self) -> bool {
        let Some(path) = self.path.clone() else {
            return false;
        };
        let stamp = std::fs::metadata(&path).ok().map(|meta| {
            (
                meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                meta.len(),
            )
        });
        if stamp == self.seen {
            return false;
        }
        self.seen = stamp;
        match Config::load(&path) {
            Ok(config) => self.set(config.unwrap_or_default()),
            Err(err) => {
                eprintln!("reef: {err}; keeping the recording settings Reef had");
                false
            }
        }
    }

    /// Applies a config: sinks whose settings are unchanged keep running,
    /// changed ones start again, with the cursors they saved. False when
    /// the config is refused.
    pub fn set(&mut self, config: Config) -> bool {
        let chain = match Chain::new(&config.redact) {
            Ok(chain) => chain,
            Err(err) => {
                eprintln!("reef: record.json: {err}; keeping the recording settings Reef had");
                return false;
            }
        };
        let mut old: HashMap<String, Sink> = (self.sinks.drain(..))
            .map(|sink| (sink.config.name.clone(), sink))
            .collect();
        let mut restarted = false;
        for wanted in &config.sinks {
            let sink_chain = match chain.then(&wanted.redact) {
                Ok(chain) => chain,
                Err(err) => {
                    eprintln!("reef: record.json: sink {}: {err}; left out", wanted.name);
                    continue;
                }
            };
            match old.remove(&wanted.name) {
                Some(mut kept) if kept.config == *wanted => {
                    kept.chain = sink_chain;
                    self.sinks.push(kept);
                }
                previous => {
                    restarted |= previous.is_some();
                    let sink = self.start(wanted.clone(), sink_chain);
                    self.sinks.push(sink);
                }
            }
        }
        // A sink stopped or started again lost what it had in flight: read
        // from the sinks' cursors again.
        if restarted || !old.is_empty() {
            let keys: Vec<Key> = self.agents.keys().cloned().collect();
            for key in keys {
                self.agents.get_mut(&key).expect("an agent").through = None;
                self.mark(key);
            }
        }
        self.chain = chain;
        self.config = config;
        for sink in &self.sinks {
            eprintln!(
                "reef: recording to {} ({})",
                sink.config.name, sink.chain.version
            );
        }
        true
    }

    fn start(&mut self, config: SinkConfig, chain: Chain) -> Sink {
        self.next_generation += 1;
        let generation = self.next_generation;
        let name = config.name.clone();
        let apply = self.apply.clone();
        let tx = sink::spawn(&name, &config.kind, self.data_dir.clone(), {
            let name = name.clone();
            move |batch, outcome| {
                let name = name.clone();
                let _ = apply.send(Box::new(move |up| {
                    up.record.on_outcome(&name, generation, batch, outcome)
                }));
            }
        });
        let cursors = self.load_cursors(&name);
        Sink {
            config,
            chain,
            generation,
            tx,
            cursors,
            queue: VecDeque::new(),
            records: 0,
            inflight: None,
            next_batch: 1,
            retry_at: None,
            last_error: None,
            save_due: false,
        }
    }

    fn cursor_file(&self, name: &str) -> PathBuf {
        self.data_dir.join("record").join(format!("{name}.json"))
    }

    fn load_cursors(&self, name: &str) -> HashMap<Key, (Cursor, Option<String>)> {
        if self.path.is_none() {
            return HashMap::new();
        }
        let saved: Vec<Saved> = std::fs::read(self.cursor_file(name))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        (saved.into_iter())
            .map(|s| ((s.host, s.agent), (s.cursor, s.updated)))
            .collect()
    }

    /// Writes the cursors sinks moved since the last save.
    pub fn save(&mut self) {
        if self.path.is_none() {
            return;
        }
        for index in 0..self.sinks.len() {
            if !std::mem::take(&mut self.sinks[index].save_due) {
                continue;
            }
            let sink = &self.sinks[index];
            let mut saved: Vec<Saved> = (sink.cursors.iter())
                .map(|((host, agent), (cursor, updated))| Saved {
                    host: host.clone(),
                    agent: agent.clone(),
                    cursor: cursor.clone(),
                    updated: updated.clone(),
                })
                .collect();
            saved.sort_by(|a, b| (&a.host, &a.agent).cmp(&(&b.host, &b.agent)));
            let path = self.cursor_file(&sink.config.name);
            let written = (|| {
                std::fs::create_dir_all(path.parent().expect("a parent"))?;
                let temporary = path.with_extension("json.tmp");
                std::fs::write(&temporary, serde_json::to_vec_pretty(&saved)?)?;
                std::fs::rename(&temporary, &path)
            })();
            if let Err(err) = written {
                eprintln!("reef: saving {}: {err}", path.display());
            }
        }
    }

    /// Whether an agent needs reading after Reef (re)connected to its host:
    /// a sink has never seen it, or it changed since a sink caught up.
    pub fn stale(&self, key: &Key, updated: Option<&str>) -> bool {
        self.sinks.iter().any(|sink| match sink.cursors.get(key) {
            None => true,
            Some((_, seen)) => seen.as_deref() != updated || updated.is_none(),
        })
    }

    /// Something happened to an agent: read it soon, or now when `urgent`
    /// (its turn ended).
    pub fn touch(&mut self, key: Key, urgent: bool) {
        if !self.following() {
            return;
        }
        self.agents.entry(key.clone()).or_default().urgent |= urgent;
        self.mark(key);
    }

    /// Owes the agent a read, in the queue of those waiting for one.
    fn mark(&mut self, key: Key) {
        let follow = self.agents.entry(key.clone()).or_default();
        follow.dirty = true;
        let urgent = follow.urgent;
        if !std::mem::replace(&mut follow.queued, true) {
            // An agent whose turn just ended is read ahead of a backlog of
            // history, which it would otherwise wait behind.
            if urgent {
                self.waiting.push_front(key);
            } else {
                self.waiting.push_back(key);
            }
        }
    }

    /// The reads to make now, at most `room` of them, from the front of
    /// the queue. Agents read too recently go to its back.
    pub fn due(&mut self, room: usize) -> Vec<(Key, Read)> {
        let now = Instant::now();
        let mut due = Vec::new();
        for _ in 0..self.waiting.len() {
            if due.len() >= room {
                break;
            }
            let Some(key) = self.waiting.pop_front() else {
                break;
            };
            let Some(follow) = self.agents.get_mut(&key) else {
                continue;
            };
            if !follow.dirty {
                follow.queued = false;
                continue;
            }
            let rested = follow.last_read.is_none_or(|at| now - at >= EVERY) || follow.urgent;
            let waited = follow.not_before.is_none_or(|at| now >= at);
            // A sink that starts from the start and is far behind is not
            // given more agents' histories to begin on: the ones it has are
            // read as it takes them. Other sinks, and agents it already
            // follows, are not held up by it.
            let backlogged = (self.sinks.iter()).any(|sink| {
                sink.config.from == Begin::Start
                    && sink.records >= HIGH_WATER
                    && !sink.cursors.contains_key(&key)
            });
            if follow.fetching.is_some() || !rested || !waited || backlogged {
                self.waiting.push_back(key);
                continue;
            }
            follow.queued = false;
            let read = match &follow.through {
                Some(cursor) => Read::After(cursor.clone()),
                None => (self.sinks.iter())
                    .filter_map(|sink| sink.cursors.get(&key).map(|(cursor, _)| cursor))
                    .min_by_key(|cursor| cursor.seq)
                    .map_or(Read::Tail, |cursor| Read::After(cursor.clone())),
            };
            follow.dirty = false;
            follow.urgent = false;
            follow.last_read = Some(now);
            follow.fetching = Some(read.clone());
            due.push((key, read));
        }
        self.reads += due.len();
        due
    }

    /// A page arrived for `key`. `running` is whether its turn still runs.
    /// Returns where to read on from when the page says there is more.
    pub fn on_page(
        &mut self,
        key: &Key,
        context: &Context,
        running: bool,
        updated: Option<String>,
        page: Result<Value, String>,
    ) -> Option<Cursor> {
        self.reads = self.reads.saturating_sub(1);
        let follow = self.agents.entry(key.clone()).or_default();
        let read = follow.fetching.take()?;
        let page = match page {
            Ok(page) => page,
            Err(err) => {
                eprintln!("reef: recording {}/{}: {err}", key.0, key.1);
                follow.not_before = Some(Instant::now() + AFTER_ERROR);
                self.mark(key.clone());
                return None;
            }
        };
        let epoch = page["epoch"].as_str().unwrap_or_default().to_string();
        let end = Cursor {
            epoch: epoch.clone(),
            seq: (page["endCursor"]["seq"].as_u64())
                .or(match &read {
                    Read::After(cursor) if cursor.epoch == epoch => Some(cursor.seq),
                    _ => None,
                })
                .or(page["window"]["maxSeq"].as_u64())
                .unwrap_or(0),
        };
        let more = page["hasNewer"].as_bool().unwrap_or(false);
        // Where the daemon's timeline begins and ends: a sink that starts
        // from the start has nothing before the first, one that starts from
        // now nothing before the last.
        let begin = Cursor {
            epoch: epoch.clone(),
            seq: (page["window"]["minSeq"].as_u64().unwrap_or(1)).saturating_sub(1),
        };
        let tail = Cursor {
            epoch: epoch.clone(),
            seq: page["window"]["maxSeq"].as_u64().unwrap_or(end.seq),
        };

        // A sink that reads from the start and has no cursor for this agent
        // is given one at the beginning, and this read, whatever it began
        // with, is over: the agent is read again from there, so the sink
        // gets its whole timeline. The cursor moves only as the sink takes
        // batches, so a Reef that stops partway reads on from where the sink
        // got to.
        let mut starting = Vec::new();
        for sink in &mut self.sinks {
            if sink.config.from == Begin::Start && !sink.cursors.contains_key(key) {
                sink.cursors.insert(key.clone(), (begin.clone(), None));
                sink.save_due = true;
                starting.push(sink.config.name.clone());
            }
        }
        let restart = !starting.is_empty();
        follow.through = Some(if restart { begin.clone() } else { end.clone() });

        // Starting from now, or starting over: no records, a sink that
        // starts from now starts at the end.
        let from = match &read {
            Read::After(from) if !restart => from.clone(),
            _ => {
                if restart {
                    // With no turn in progress, no sink but those starting
                    // has a summary owed it by the replay of an old turn
                    // (see `fresh` below). With one, any sink may.
                    if follow.turn.is_none() {
                        follow.backfill.extend(starting);
                    } else {
                        follow.backfill.clear();
                    }
                    follow.turn = None;
                }
                let now = if matches!(read, Read::Tail) {
                    &end
                } else {
                    &tail
                };
                for sink in &mut self.sinks {
                    if !sink.cursors.contains_key(key) {
                        sink.queue
                            .push_back(Item::Mark(key.clone(), now.clone(), updated.clone()));
                    }
                }
                return restart.then_some(begin);
            }
        };

        let mut records = Vec::new();
        if page["reset"] == true {
            let at = format!("{epoch}/gap/{}/{}", from.epoch, from.seq);
            let mut gap = event::base(context, &at, "gap", &None);
            gap["time"] = json!(fleet::clock::now_iso());
            gap["text"] =
                json!("The agent's timeline started again; records before this may be missing.");
            records.push(Record {
                seq_start: 0,
                seq_end: 0,
                kind: "gap".into(),
                turn: None,
                text: String::new(),
                value: gap,
            });
        }
        let entries = page["entries"].as_array().cloned().unwrap_or_default();
        let last = entries.len().saturating_sub(1);
        for (index, entry) in entries.iter().enumerate() {
            let Some(mut record) = event::from_entry(context, &epoch, entry) else {
                continue;
            };
            let growing =
                running && index == last && matches!(record.kind.as_str(), "reply" | "thought");
            let partial = growing || record.value["tool"]["status"] == "running";
            record.value["partial"] = json!(partial);
            if let Some(turn) = record.turn.clone() {
                if (follow.turn.as_ref()).is_some_and(|current| current.id != turn) {
                    let finished = follow.turn.take().expect("a turn");
                    records.push(turn_finished(context, &epoch, finished));
                }
                let current = follow.turn.get_or_insert_with(|| Turn {
                    id: turn,
                    whole: record.kind == "prompt",
                    started: record.value["time"].clone(),
                    ..Turn::default()
                });
                note(current, &record);
            }
            records.push(record);
        }
        if !running
            && !more
            && let Some(finished) = follow.turn.take()
        {
            records.push(turn_finished(context, &epoch, finished));
        }

        // Only the last page of a read says the agent is as the daemon
        // listed it: a sink that stops partway is still behind.
        let caught_up = if more { None } else { updated.clone() };
        let backfill = follow.backfill.clone();
        if !more {
            follow.backfill.clear();
        }
        for sink in &mut self.sinks {
            let Some((cursor, _)) = sink.cursors.get(key).cloned() else {
                // A sink new since the last read starts at the end too.
                sink.queue
                    .push_back(Item::Mark(key.clone(), tail.clone(), caught_up.clone()));
                continue;
            };
            let fresh = |record: &Record| match record.kind.as_str() {
                // A restart is told to every sink that follows the agent;
                // its id makes a repeat harmless.
                "gap" => true,
                // A turn's summary comes when the turn is seen to end, which
                // can be a read after the sink's cursor reached its last
                // entry (and no later), but never one the sink has passed.
                // Except while history is read again for sinks that start from
                // the start: the others have had every summary of it.
                "turn_finished" => {
                    let owed = backfill.is_empty() || backfill.contains(&sink.config.name);
                    cursor.epoch != epoch
                        || record.seq_end > cursor.seq
                        || (record.seq_end == cursor.seq && owed)
                }
                _ => cursor.epoch != epoch || record.seq_end > cursor.seq,
            };
            for record in &records {
                if !fresh(record) || !sink.wants(record, context) {
                    continue;
                }
                let mut value = record.value.clone();
                if sink.chain.apply(&mut value) {
                    sink.queue.push_back(Item::Record(value));
                    sink.records += 1;
                }
            }
            sink.queue
                .push_back(Item::Mark(key.clone(), end.clone(), caught_up.clone()));
        }
        self.trim();
        if more && self.sinks.iter().any(|sink| sink.records >= HIGH_WATER) {
            // The rest of a long backlog waits until a sink has taken some.
            let follow = self.agents.entry(key.clone()).or_default();
            follow.not_before = Some(Instant::now() + BEHIND_FOR);
            self.mark(key.clone());
            return None;
        }
        more.then_some(end)
    }

    /// A sink whose queue grew too long drops it; the agents in it are
    /// read again from that sink's cursors once it catches up.
    fn trim(&mut self) {
        for index in 0..self.sinks.len() {
            if self.sinks[index].records <= QUEUE_MAX {
                continue;
            }
            let sink = &mut self.sinks[index];
            eprintln!(
                "reef: sink {} is behind; reading its records again later",
                sink.config.name
            );
            // An agent deleted is not read again, and its `forget` stays.
            let mut keys = HashSet::new();
            let mut forgets = VecDeque::new();
            for item in sink.queue.drain(..) {
                match item {
                    Item::Mark(key, ..) => {
                        keys.insert(key);
                    }
                    Item::Record(_) => {}
                    forget @ Item::Forget(..) => forgets.push_back(forget),
                }
            }
            sink.records = forgets.len();
            sink.queue = forgets;
            for key in keys {
                self.agents.entry(key.clone()).or_default().through = None;
                self.mark(key);
            }
        }
    }

    /// Sends each sink its next batch, if it has none in flight.
    pub fn pump(&mut self) {
        let now = Instant::now();
        for sink in &mut self.sinks {
            if sink.inflight.is_some() || sink.retry_at.is_some_and(|at| now < at) {
                continue;
            }
            // Marks with no records ahead of them need no sink.
            while let Some(Item::Mark(..)) = sink.queue.front() {
                if let Some(Item::Mark(key, cursor, updated)) = sink.queue.pop_front() {
                    advance(&mut sink.cursors, key, cursor, updated);
                    sink.save_due = true;
                }
            }
            if sink.queue.is_empty() {
                continue;
            }
            let mut items = Vec::new();
            let mut records = Vec::new();
            while let Some(item) = sink.queue.front() {
                if item.record().is_some() && records.len() >= sink.config.batch.max(1) {
                    break;
                }
                let item = sink.queue.pop_front().expect("an item");
                if let Some(value) = item.record() {
                    records.push(value.clone());
                }
                items.push(item);
            }
            sink.records -= records.len();
            let id = sink.next_batch;
            sink.next_batch += 1;
            match sink.tx.try_send(Batch { id, records }) {
                Ok(()) => sink.inflight = Some((id, items)),
                Err(_) => {
                    // The worker is busy or gone; put it back and try later.
                    sink.records += items.iter().filter(|i| i.record().is_some()).count();
                    for item in items.into_iter().rev() {
                        sink.queue.push_front(item);
                    }
                    sink.retry_at = Some(now + Duration::from_secs(1));
                }
            }
        }
    }

    pub fn on_outcome(&mut self, name: &str, generation: u64, batch: u64, outcome: Outcome) {
        let Some(sink) = (self.sinks.iter_mut())
            .find(|sink| sink.config.name == name && sink.generation == generation)
        else {
            return;
        };
        if sink.inflight.as_ref().is_none_or(|(id, _)| *id != batch) {
            return;
        }
        let (_, items) = sink.inflight.take().expect("in flight");
        match outcome {
            Outcome::Done => {
                for item in items {
                    match item {
                        Item::Mark(key, cursor, updated) => {
                            advance(&mut sink.cursors, key, cursor, updated);
                        }
                        // Told, so no cursor is kept for it.
                        Item::Forget(key, _) => {
                            sink.cursors.remove(&key);
                        }
                        Item::Record(_) => {}
                    }
                }
                sink.save_due = true;
                sink.retry_at = None;
                if sink.last_error.take().is_some() {
                    eprintln!("reef: sink {name} is taking records again");
                }
            }
            Outcome::Retry(after, error) => {
                if sink.last_error.as_deref() != Some(error.as_str()) {
                    eprintln!("reef: sink {name}: {error}; trying again in {after:?}");
                }
                sink.last_error = Some(error);
                sink.retry_at = Some(Instant::now() + after);
                sink.records += items.iter().filter(|i| i.record().is_some()).count();
                for item in items.into_iter().rev() {
                    sink.queue.push_front(item);
                }
            }
        }
    }

    /// A read Reef could not make (the host went away): make it later.
    pub fn unread(&mut self, key: &Key) {
        self.reads = self.reads.saturating_sub(1);
        let Some(follow) = self.agents.get_mut(key) else {
            return;
        };
        follow.fetching = None;
        self.mark(key.clone());
    }

    /// Reads on from a page that said there is more.
    pub fn read_on(&mut self, key: &Key, cursor: &Cursor) {
        if let Some(follow) = self.agents.get_mut(key) {
            follow.fetching = Some(Read::After(cursor.clone()));
            self.reads += 1;
        }
    }

    /// An agent that is gone: nothing more to read for it.
    pub fn forget(&mut self, key: &Key) {
        self.agents.remove(key);
    }

    pub fn forget_host(&mut self, host: &str) {
        self.agents.retain(|key, _| key.0 != host);
        let checks = &mut self.checks;
        checks.waiting.retain(|key| key.0 != host);
        checks.queued.retain(|key| key.0 != host);
        checks.again.retain(|key| key.0 != host);
        checks.retry_at.retain(|key, _| key.0 != host);
        checks.failures.retain(|key, _| key.0 != host);
        checks.archived.retain(|key, _| key.0 != host);
    }

    // ---- deleted agents -------------------------------------------------

    /// Whether a sink has anything for the agent: a cursor, or a read of it
    /// waiting to become one. Only then is there anything to forget.
    fn follows(&self, key: &Key) -> bool {
        self.sinks.iter().any(|sink| {
            sink.cursors.contains_key(key)
                || (sink.queue.iter()).any(|item| matches!(item, Item::Mark(k, ..) if k == key))
        })
    }

    /// The agents of `host` any sink has a cursor for.
    pub fn followed(&self, host: &str) -> HashSet<Key> {
        let cursors = self.sinks.iter().flat_map(|sink| sink.cursors.keys());
        cursors.filter(|key| key.0 == host).cloned().collect()
    }

    /// An agent is no longer in a daemon's active list: it was archived or
    /// deleted, which the daemon's `remove` does not say. Asks the daemon by
    /// id, a few at a time ([`Self::checks_due`], [`Self::checked`]).
    /// `removed` is a daemon saying so now, over a daemon that was not
    /// listing it when Reef came to look: an agent found archived is then
    /// asked about again, and otherwise not for a while.
    pub fn suspect(&mut self, key: Key, removed: bool) {
        if !self.following() || !self.follows(&key) {
            return;
        }
        let checks = &mut self.checks;
        if removed {
            checks.archived.remove(&key);
        } else if (checks.archived.get(&key)).is_some_and(|at| at.elapsed() < ARCHIVED_FOR) {
            return;
        }
        if checks.queued.contains(&key) {
            return;
        }
        if checks.asking.contains(&key) {
            // What the daemon answers may be from before this.
            if removed {
                checks.again.insert(key);
            }
            return;
        }
        checks.queued.insert(key.clone());
        checks.waiting.push_back(key);
    }

    /// The agents to ask the daemons about now: few at a time, and not
    /// those whose last check failed a moment ago.
    pub fn checks_due(&mut self) -> Vec<Key> {
        let now = Instant::now();
        let room = CHECKS.saturating_sub(self.checks.asking.len());
        let checks = &mut self.checks;
        let mut due = Vec::new();
        for _ in 0..checks.waiting.len() {
            if due.len() >= room {
                break;
            }
            let Some(key) = checks.waiting.pop_front() else {
                break;
            };
            if checks.retry_at.get(&key).is_some_and(|at| now < *at) {
                checks.waiting.push_back(key);
                continue;
            }
            checks.queued.remove(&key);
            checks.retry_at.remove(&key);
            checks.asking.insert(key.clone());
            due.push(key);
        }
        due
    }

    /// The daemon's answer: whether the agent is `gone` (deleted), or is
    /// still there, archived. Or why it could not say; asked again later.
    pub fn checked(&mut self, key: &Key, answer: Result<bool, String>) {
        self.checks.asking.remove(key);
        let again = self.checks.again.remove(key);
        match answer {
            Ok(true) => self.deleted(key),
            Ok(false) => {
                self.checks.failures.remove(key);
                self.checks.archived.insert(key.clone(), Instant::now());
                if again {
                    self.requeue(key, None);
                }
            }
            Err(err) => {
                let failures = self.checks.failures.entry(key.clone()).or_default();
                *failures += 1;
                if *failures == 1 {
                    eprintln!(
                        "reef: recording {}/{}: asking whether it was deleted: {err}",
                        key.0, key.1
                    );
                }
                // 30 s, then twice as long each time, up to an hour.
                let pause = CHECK_AGAIN * 2u32.saturating_pow((*failures - 1).min(7));
                self.requeue(key, Some(pause.min(ARCHIVED_FOR)));
            }
        }
    }

    /// A check that could not be made (the host is not connected): later.
    pub fn unchecked(&mut self, key: &Key) {
        self.checks.asking.remove(key);
        self.requeue(key, Some(CHECK_AGAIN));
    }

    fn requeue(&mut self, key: &Key, after: Option<Duration>) {
        let checks = &mut self.checks;
        if let Some(after) = after {
            checks.retry_at.insert(key.clone(), Instant::now() + after);
        }
        if checks.queued.insert(key.clone()) {
            checks.waiting.push_back(key.clone());
        }
    }

    /// An agent the daemon says is gone for good: each sink that has a
    /// cursor for it is told to forget it, and drops the cursor once it has
    /// taken the record. Agents on hosts a sink does not take only lose the
    /// cursor.
    fn deleted(&mut self, key: &Key) {
        let (host, agent) = key;
        for sink in &mut self.sinks {
            let known = sink.cursors.contains_key(key)
                || (sink.queue.iter()).any(|item| matches!(item, Item::Mark(k, ..) if k == key));
            if !known {
                continue;
            }
            let taken = sink.config.hosts.is_empty() || sink.config.hosts.contains(host);
            let mut record = event::forget(host, agent);
            if taken && sink.chain.apply(&mut record) {
                sink.queue.push_back(Item::Forget(key.clone(), record));
                sink.records += 1;
            } else {
                sink.queue
                    .retain(|item| !matches!(item, Item::Mark(k, ..) if k == key));
                sink.cursors.remove(key);
                sink.save_due = true;
            }
        }
        self.checks.archived.remove(key);
        self.checks.failures.remove(key);
        self.agents.remove(key);
    }
}

/// Takes a sink to `cursor` on an agent. Within an epoch a cursor only goes
/// forward: a read that began behind it, for another sink that was behind,
/// says nothing new about this one, and must not put it back to be sent what
/// it has had.
fn advance(
    cursors: &mut HashMap<Key, (Cursor, Option<String>)>,
    key: Key,
    cursor: Cursor,
    updated: Option<String>,
) {
    match cursors.get(&key) {
        Some((old, _)) if old.epoch == cursor.epoch && old.seq > cursor.seq => {}
        _ => {
            cursors.insert(key, (cursor, updated));
        }
    }
}

/// Whether a daemon's answer to `fetch_agent_request` for `asked` says that
/// no such agent exists: it has none, or only one that is not it (a daemon
/// also matches a name, or the start of an id). An agent archived is
/// answered with the agent, so it is not gone. An error, or an answer that
/// is neither, says nothing about it.
pub fn is_gone(payload: &Value, asked: &str) -> Result<bool, String> {
    match &payload["error"] {
        Value::Null => {}
        Value::String(error) if !error.is_empty() => return Err(error.clone()),
        Value::String(_) => {}
        other => return Err(other.to_string()),
    }
    match payload.get("agent") {
        Some(Value::Null) => Ok(true),
        Some(agent) if agent.is_object() => Ok(agent["id"].as_str() != Some(asked)),
        _ => Err("the daemon's answer had no agent in it".into()),
    }
}

fn note(turn: &mut Turn, record: &Record) {
    let first = turn.seen.insert(record.seq_start);
    match record.kind.as_str() {
        "prompt" if turn.prompt.is_none() => turn.prompt = Some(record.text.clone()),
        "reply" if !record.text.trim().is_empty() => turn.reply = Some(record.text.clone()),
        "tool_call" if first => *turn.tools.entry(record.text.clone()).or_default() += 1,
        _ => {}
    }
    turn.ended = record.value["time"].clone();
    turn.last_seq = turn.last_seq.max(record.seq_end);
}

fn turn_finished(context: &Context, epoch: &str, turn: Turn) -> Record {
    let mut value = event::base(
        context,
        &format!("{epoch}/turn/{}", turn.id),
        "turn_finished",
        &Some(turn.id.clone()),
    );
    let reply = turn.reply.clone().unwrap_or_default();
    value["time"] = turn.ended.clone();
    value["started"] = turn.started.clone();
    value["key"] = json!(event::key(context.agent_id(), "turn_finished", &reply));
    value["prompt"] = json!(turn.prompt);
    value["reply"] = json!(turn.reply);
    value["tools"] = json!(turn.tools);
    value["whole"] = json!(turn.whole);
    Record {
        seq_start: turn.last_seq,
        seq_end: turn.last_seq,
        kind: "turn_finished".into(),
        turn: Some(turn.id),
        text: reply,
        value,
    }
}

#[cfg(test)]
mod tests {
    use std::ops::RangeInclusive;

    use super::config::SinkRedact;
    use super::*;

    const EPOCH: &str = "e1";

    fn key() -> Key {
        ("box".into(), "a1".into())
    }

    fn context() -> Context {
        Context {
            host: "box".into(),
            agent: json!({ "id": "a1", "title": "Worker" }),
            workspace: json!({ "id": "w1", "labels": [] }),
        }
    }

    fn sink(name: &str, from: Begin) -> SinkConfig {
        SinkConfig {
            name: name.into(),
            kind: config::SinkKind::Jsonl {
                dir: std::env::temp_dir().join(format!("reef-unit-{}-{name}", std::process::id())),
            },
            kinds: Vec::new(),
            hosts: Vec::new(),
            labels: Vec::new(),
            redact: SinkRedact::default(),
            batch: 200,
            from,
        }
    }

    /// A recorder with these sinks, following every agent. Nothing is sent
    /// anywhere: [`take`] stands in for a sink that takes what is queued.
    fn recorder(sinks: Vec<SinkConfig>) -> Recorder {
        let (apply, _) = mpsc::unbounded_channel();
        let mut recorder = Recorder::new(None, apply);
        let config = Config {
            follow_all: true,
            sinks,
            ..Config::default()
        };
        assert!(recorder.set(config));
        recorder
    }

    /// What the sink gets if it takes all that is queued for it now: the
    /// records in order, and its cursors afterwards as it keeps them.
    fn take(recorder: &mut Recorder, name: &str) -> Vec<Value> {
        let sink = (recorder.sinks.iter_mut())
            .find(|sink| sink.config.name == name)
            .expect("a sink");
        let mut records = Vec::new();
        for item in std::mem::take(&mut sink.queue) {
            match item {
                Item::Record(record) => records.push(record),
                Item::Forget(key, record) => {
                    records.push(record);
                    sink.cursors.remove(&key);
                }
                Item::Mark(key, cursor, updated) => {
                    advance(&mut sink.cursors, key, cursor, updated)
                }
            }
        }
        sink.records = 0;
        records
    }

    fn kinds(records: &[Value]) -> Vec<&str> {
        records
            .iter()
            .map(|r| r["kind"].as_str().unwrap())
            .collect()
    }

    fn cursor_of(recorder: &Recorder, name: &str) -> Option<(Cursor, Option<String>)> {
        let sink = recorder.sinks.iter().find(|s| s.config.name == name)?;
        sink.cursors.get(&key()).cloned()
    }

    fn at(seq: u64) -> Cursor {
        Cursor {
            epoch: EPOCH.into(),
            seq,
        }
    }

    /// A page of a timeline of two-entry turns (odd seqs are prompts, even
    /// ones replies) whose window is `min..=max`.
    fn page(rows: RangeInclusive<u64>, min: u64, max: u64, more: bool) -> Value {
        let entries: Vec<Value> = rows
            .map(|seq| {
                let item = if seq % 2 == 1 {
                    json!({ "type": "user_message", "text": format!("prompt {seq}") })
                } else {
                    json!({ "type": "assistant_message", "text": format!("reply {seq}") })
                };
                json!({ "seqStart": seq, "seqEnd": seq, "turnId": format!("t{}", seq.div_ceil(2)),
                        "timestamp": "2026-11-01T10:00:00Z", "item": item })
            })
            .collect();
        let end = entries
            .last()
            .map(|entry| json!({ "epoch": EPOCH, "seq": entry["seqEnd"] }));
        json!({ "epoch": EPOCH, "reset": false, "endCursor": end, "hasNewer": more,
                "window": { "minSeq": min, "maxSeq": max, "nextSeq": max + 1 },
                "entries": entries })
    }

    /// The read the recorder wants for the agent now.
    fn ask(recorder: &mut Recorder) -> Read {
        recorder.touch(key(), true);
        let mut due = recorder.due(8);
        assert_eq!(due.len(), 1, "one read due");
        due.remove(0).1
    }

    fn feed(recorder: &mut Recorder, page: Value) -> Option<Cursor> {
        recorder.on_page(&key(), &context(), false, Some("u1".into()), Ok(page))
    }

    #[tokio::test]
    async fn a_sink_that_starts_from_the_start_reads_the_timeline_from_its_beginning() {
        let mut rec = recorder(vec![sink("history", Begin::Start), sink("now", Begin::Now)]);
        // No cursors, so Reef looks at the end of the timeline, 6 rows long...
        assert_eq!(ask(&mut rec), Read::Tail);
        let more = feed(&mut rec, page(6..=6, 1, 6, false));
        // ...and reads on from before its first row, for the sink that wants it.
        assert_eq!(more, Some(at(0)));
        assert_eq!(cursor_of(&rec, "history"), Some((at(0), None)));
        assert_eq!(
            cursor_of(&rec, "now"),
            None,
            "its cursor is queued, at the end"
        );
        assert_eq!(rec.agents[&key()].through, Some(at(0)));
        assert!(take(&mut rec, "now").is_empty());
        assert_eq!(cursor_of(&rec, "now").map(|c| c.0), Some(at(6)));

        // The first page of the history, with more to come.
        rec.read_on(&key(), &at(0));
        let more = feed(&mut rec, page(1..=4, 1, 6, true));
        assert_eq!(more, Some(at(4)));
        let got = take(&mut rec, "history");
        assert_eq!(
            kinds(&got),
            ["prompt", "reply", "turn_finished", "prompt", "reply"]
        );
        assert_eq!(got[0]["text"], "prompt 1");
        assert_eq!(got[2]["whole"], true);
        // Not caught up yet: a Reef that stopped here would read on.
        assert_eq!(cursor_of(&rec, "history"), Some((at(4), None)));
        assert!(rec.stale(&key(), Some("u1")));
        // The sink that starts from now has none of it, and stays at the end.
        assert!(take(&mut rec, "now").is_empty());
        assert_eq!(cursor_of(&rec, "now").map(|c| c.0), Some(at(6)));

        // The last page: the rest, the last turn's summary, and caught up.
        rec.read_on(&key(), &at(4));
        let more = feed(&mut rec, page(5..=6, 1, 6, false));
        assert_eq!(more, None);
        let got = take(&mut rec, "history");
        assert_eq!(
            kinds(&got),
            ["turn_finished", "prompt", "reply", "turn_finished"]
        );
        assert_eq!(got[0]["reply"], "reply 4");
        assert_eq!(got[3]["reply"], "reply 6");
        assert_eq!(cursor_of(&rec, "history"), Some((at(6), Some("u1".into()))));
        assert!(!rec.stale(&key(), Some("u1")));
        // The other sink was owed nothing of the history, summaries included.
        assert!(take(&mut rec, "now").is_empty());
        assert_eq!(cursor_of(&rec, "now").map(|c| c.0), Some(at(6)));
        assert_eq!(rec.agents[&key()].through, Some(at(6)));
        assert!(rec.agents[&key()].backfill.is_empty());
    }

    #[tokio::test]
    async fn a_sink_added_beside_one_that_is_ahead_reads_all_and_does_not_set_it_back() {
        let mut rec = recorder(vec![sink("log", Begin::Now)]);
        assert_eq!(ask(&mut rec), Read::Tail);
        assert_eq!(feed(&mut rec, page(4..=4, 1, 4, false)), None);
        take(&mut rec, "log");
        assert_eq!(cursor_of(&rec, "log").map(|c| c.0), Some(at(4)));

        // A sink from the start joins; the read goes from the log's cursor,
        // finds the new sink has none, and begins again.
        let mut config = rec.config.clone();
        config.sinks.push(sink("search", Begin::Start));
        assert!(rec.set(config));
        assert_eq!(ask(&mut rec), Read::After(at(4)));
        let more = feed(&mut rec, page(5..=5, 1, 5, false));
        assert_eq!(more, Some(at(0)), "starts over, for the sink that is new");
        assert!(
            take(&mut rec, "search").is_empty(),
            "nothing from the page cut short"
        );
        assert!(take(&mut rec, "log").is_empty());

        // The whole history in two pages (the log has had rows 1 to 4).
        rec.read_on(&key(), &at(0));
        assert_eq!(feed(&mut rec, page(1..=3, 1, 5, true)), Some(at(3)));
        rec.read_on(&key(), &at(3));
        assert_eq!(feed(&mut rec, page(4..=5, 1, 5, false)), None);
        let got = take(&mut rec, "search");
        assert_eq!(
            kinds(&got),
            [
                "prompt",
                "reply",
                "turn_finished",
                "prompt",
                "reply",
                "turn_finished",
                "prompt",
                "turn_finished"
            ]
        );
        // The log gets only the row it had not had; its cursor never went back,
        // and no old summary came with it.
        let got = take(&mut rec, "log");
        assert_eq!(kinds(&got), ["prompt", "turn_finished"]);
        assert_eq!(got[0]["text"], "prompt 5");
        assert_eq!(cursor_of(&rec, "log").map(|c| c.0), Some(at(5)));
    }

    #[tokio::test]
    async fn a_sink_far_behind_is_not_given_more_agents_to_begin_on() {
        let mut rec = recorder(vec![sink("history", Begin::Start), sink("log", Begin::Now)]);
        let (begun, new) = (key(), ("box".to_string(), "new".to_string()));
        rec.sinks[0].cursors.insert(begun.clone(), (at(3), None));
        rec.sinks[0].records = HIGH_WATER;
        rec.touch(new.clone(), false);
        assert!(rec.due(8).is_empty(), "held: the sink would begin on it");
        // An agent it already follows is read as ever, the new one when it can.
        rec.touch(begun.clone(), false);
        assert_eq!(rec.due(8)[0].0, begun);
        rec.sinks[0].records = HIGH_WATER - 1;
        assert_eq!(rec.due(8)[0].0, new);
        // A sink that starts from now, however far behind, holds nothing.
        rec.sinks[0].records = 0;
        rec.sinks[1].records = HIGH_WATER;
        let other = ("box".to_string(), "other".to_string());
        rec.touch(other.clone(), false);
        assert_eq!(rec.due(8)[0].0, other);
    }

    #[tokio::test]
    async fn an_agent_whose_turn_just_ended_is_read_ahead_of_a_backlog() {
        let mut rec = recorder(vec![sink("history", Begin::Start)]);
        for n in 0..5 {
            rec.touch(("box".into(), format!("old{n}")), false);
        }
        rec.touch(("box".into(), "live".into()), true);
        let due = rec.due(1);
        assert_eq!(due[0].0, ("box".to_string(), "live".to_string()));
    }

    #[test]
    fn a_cursor_only_goes_forward_within_an_epoch() {
        let mut cursors = HashMap::new();
        advance(&mut cursors, key(), at(6), Some("u1".into()));
        advance(&mut cursors, key(), at(4), None);
        assert_eq!(cursors[&key()], (at(6), Some("u1".into())));
        advance(&mut cursors, key(), at(6), Some("u2".into()));
        assert_eq!(cursors[&key()].1.as_deref(), Some("u2"));
        // A new epoch starts the numbers again.
        let later = Cursor {
            epoch: "e2".into(),
            seq: 1,
        };
        advance(&mut cursors, key(), later.clone(), None);
        assert_eq!(cursors[&key()].0, later);
    }

    #[tokio::test]
    async fn a_long_backlog_waits_while_a_sink_is_far_behind() {
        let mut rec = recorder(vec![sink("history", Begin::Start)]);
        assert_eq!(ask(&mut rec), Read::Tail);
        assert_eq!(feed(&mut rec, page(60..=60, 1, 60, false)), Some(at(0)));
        // The sink has not taken what it was sent.
        rec.sinks[0].records = HIGH_WATER;
        rec.read_on(&key(), &at(0));
        assert_eq!(feed(&mut rec, page(1..=4, 1, 60, true)), None, "waits");
        let follow = &rec.agents[&key()];
        assert!(follow.dirty && follow.not_before.is_some());
        assert_eq!(follow.through, Some(at(4)), "and goes on from here");
        // Once the sink has taken them, the read is made again.
        take(&mut rec, "history");
        rec.agents.get_mut(&key()).unwrap().not_before = None;
        rec.agents.get_mut(&key()).unwrap().last_read = None;
        assert_eq!(rec.due(8).len(), 1);
    }

    #[tokio::test]
    async fn a_deleted_agent_is_forgotten_by_each_sink_that_has_a_cursor_for_it() {
        let mut busy = sink("kinds", Begin::Now);
        busy.kinds = vec!["prompt".into()];
        let mut other_host = sink("elsewhere", Begin::Now);
        other_host.hosts = vec!["other".into()];
        let mut rec = recorder(vec![sink("all", Begin::Now), busy, other_host]);
        assert_eq!(ask(&mut rec), Read::Tail);
        assert_eq!(feed(&mut rec, page(2..=2, 1, 2, false)), None);
        for name in ["all", "kinds", "elsewhere"] {
            assert!(take(&mut rec, name).is_empty());
            assert!(cursor_of(&rec, name).is_some(), "{name}");
        }
        rec.suspect(key(), true);
        let asked = rec.checks_due();
        assert_eq!(asked, [key()]);
        rec.checked(&key(), Ok(true));

        // Whatever its kinds, the record says who and nothing else.
        for name in ["all", "kinds"] {
            let got = take(&mut rec, name);
            assert_eq!(kinds(&got), ["forget"], "{name}");
            assert_eq!(got[0]["id"], "box/a1/forget");
            assert_eq!(got[0]["agent"], json!({ "id": "a1" }));
            assert_eq!(got[0]["host"], "box");
            assert_eq!(got[0]["v"], "fleet-event/1");
            assert_eq!(got[0]["body"], "");
            assert!(got[0]["time"].is_string() && got[0]["source"].is_string());
            assert!(got[0].get("workspace").is_none());
            assert_eq!(cursor_of(&rec, name), None, "taken, so no cursor");
        }
        // A sink of another host was never going to get it; the cursor goes.
        assert!(take(&mut rec, "elsewhere").is_empty());
        assert_eq!(cursor_of(&rec, "elsewhere"), None);
        assert!(!rec.agents.contains_key(&key()));
        // Nothing is asked about it again.
        rec.suspect(key(), true);
        assert!(rec.checks_due().is_empty());
    }

    #[tokio::test]
    async fn an_archived_agent_is_not_forgotten() {
        let mut rec = recorder(vec![sink("log", Begin::Now)]);
        assert_eq!(ask(&mut rec), Read::Tail);
        assert_eq!(feed(&mut rec, page(2..=2, 1, 2, false)), None);
        take(&mut rec, "log");

        // The daemon's `remove`; asked for by id the agent is still there.
        rec.suspect(key(), true);
        assert_eq!(rec.checks_due(), [key()]);
        rec.checked(&key(), Ok(false));
        assert!(take(&mut rec, "log").is_empty());
        assert!(
            cursor_of(&rec, "log").is_some(),
            "kept, for when it is back"
        );
        // The same agent, missing from a host's list when Reef reconnects, is
        // not asked about again, but a daemon's `remove` always is.
        rec.suspect(key(), false);
        assert!(rec.checks_due().is_empty());
        rec.suspect(key(), true);
        assert_eq!(rec.checks_due(), [key()]);
        rec.checked(&key(), Ok(true));
        assert_eq!(kinds(&take(&mut rec, "log")), ["forget"]);
    }

    #[tokio::test]
    async fn checks_are_paced_retried_and_only_for_agents_a_sink_follows() {
        let mut rec = recorder(vec![sink("log", Begin::Now)]);
        let keys: Vec<Key> = (0..9)
            .map(|n| ("box".to_string(), format!("a{n}")))
            .collect();
        for key in &keys {
            let sink = &mut rec.sinks[0];
            sink.cursors.insert(key.clone(), (at(1), None));
        }
        // An agent no sink follows needs no question.
        rec.suspect(("box".into(), "stranger".into()), true);
        for key in &keys {
            rec.suspect(key.clone(), false);
            rec.suspect(key.clone(), false);
        }
        let first = rec.checks_due();
        assert_eq!(first.len(), CHECKS, "a few at a time");
        assert!(rec.checks_due().is_empty(), "until some are answered");
        // One the daemon could not answer is asked again later, not at once.
        rec.checked(&first[0], Err("timed out".into()));
        rec.checked(&first[1], Ok(false));
        let next = rec.checks_due();
        assert_eq!(
            next.len(),
            2,
            "room for the two answered; the failed one waits"
        );
        assert!(!next.contains(&first[0]));
        assert!(!next.contains(&first[1]));
        // A daemon that keeps failing is asked less and less: 30 s, then 60 s.
        let pause = |rec: &Recorder, key: &Key| {
            rec.checks.retry_at[key]
                .saturating_duration_since(Instant::now())
                .as_secs()
        };
        assert!((29..=30).contains(&pause(&rec, &first[0])));
        rec.checks.asking.insert(first[0].clone());
        rec.checked(&first[0], Err("timed out".into()));
        assert_eq!(rec.checks.failures[&first[0]], 2);
        assert!((59..=60).contains(&pause(&rec, &first[0])));
        // An agent removed again while it was being asked about is asked again.
        rec.suspect(next[0].clone(), true);
        rec.checked(&next[0], Ok(false));
        assert!(rec.checks.waiting.contains(&next[0]));
        // A host that goes takes its questions with it.
        rec.forget_host("box");
        assert!(rec.checks.waiting.is_empty() && rec.checks.queued.is_empty());
    }

    #[test]
    fn only_an_answer_that_says_there_is_no_such_agent_says_deleted() {
        let found = json!({ "agent": { "id": "a1", "archivedAt": "2026-11-01T00:00:00Z" },
                            "project": null, "error": null });
        assert_eq!(is_gone(&found, "a1"), Ok(false));
        let none = json!({ "agent": null, "project": null, "error": null });
        assert_eq!(is_gone(&none, "a1"), Ok(true));
        // A daemon also matches the start of an id, or a title: not this agent.
        let other = json!({ "agent": { "id": "a10" }, "error": null });
        assert_eq!(is_gone(&other, "a1"), Ok(true));
        // Anything else says nothing: it is asked again, and nothing is forgotten.
        for unsure in [
            json!({ "agent": null, "error": "internal error" }),
            json!({ "agent": null, "error": { "code": 5 } }),
            json!({ "error": null }),
            json!({ "agent": "a1", "error": null }),
            json!({}),
        ] {
            assert!(is_gone(&unsure, "a1").is_err(), "{unsure}");
        }
    }
}
