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

use config::{Config, SinkConfig};
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
        if !std::mem::replace(&mut follow.queued, true) {
            self.waiting.push_back(key);
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
            if follow.fetching.is_some() || !rested || !waited {
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
        follow.through = Some(end.clone());

        // Starting from now: no records, the sinks start at the end.
        let Read::After(from) = read else {
            for sink in &mut self.sinks {
                if !sink.cursors.contains_key(key) {
                    sink.queue
                        .push_back(Item::Mark(key.clone(), end.clone(), updated.clone()));
                }
            }
            return None;
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

        for sink in &mut self.sinks {
            let Some((cursor, _)) = sink.cursors.get(key).cloned() else {
                // A sink new since the last read starts at the end too.
                sink.queue
                    .push_back(Item::Mark(key.clone(), end.clone(), updated.clone()));
                continue;
            };
            let fresh = |record: &Record| match record.kind.as_str() {
                // Whole-turn and restart records go to every sink that
                // follows the agent; their ids make a repeat harmless.
                "turn_finished" | "gap" => true,
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
                .push_back(Item::Mark(key.clone(), end.clone(), updated.clone()));
        }
        self.trim();
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
            let keys: HashSet<Key> = (sink.queue.drain(..))
                .filter_map(|item| match item {
                    Item::Mark(key, ..) => Some(key),
                    Item::Record(_) => None,
                })
                .collect();
            sink.records = 0;
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
                    sink.cursors.insert(key, (cursor, updated));
                    sink.save_due = true;
                }
            }
            if sink.queue.is_empty() {
                continue;
            }
            let mut items = Vec::new();
            let mut records = Vec::new();
            while let Some(item) = sink.queue.front() {
                if matches!(item, Item::Record(_)) && records.len() >= sink.config.batch.max(1) {
                    break;
                }
                let item = sink.queue.pop_front().expect("an item");
                if let Item::Record(value) = &item {
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
                    sink.records += (items.iter())
                        .filter(|i| matches!(i, Item::Record(_)))
                        .count();
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
                    if let Item::Mark(key, cursor, updated) = item {
                        sink.cursors.insert(key, (cursor, updated));
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
                sink.records += (items.iter())
                    .filter(|i| matches!(i, Item::Record(_)))
                    .count();
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
