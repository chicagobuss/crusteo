//! `directory_suggestions_request`: the folder picker's search, ported from
//! Paseo 0.12's `utils/directory-suggestions.ts` with its two 0.12 fixes.
//!
//! - **Without a `cwd`** (Add Project) it searches the user's home and
//!   answers absolute paths. A query names a place when it starts with `~`,
//!   `./` or is absolute inside the home; otherwise it matches anywhere.
//!   An empty query answers nothing. On macOS `~/Library` is never walked
//!   into, since reading its protected folders blocks forever for a process
//!   that cannot ask for consent; typing a path inside it still lists it.
//! - **With a `cwd`** it searches that workspace and answers relative paths:
//!   gitignored entries are skipped, a few dot folders (`.github`, ...) are
//!   walked, and an empty query lists the top level.
//!
//! The walk visits siblings round-robin, a level at a time, and creates a
//! folder's listing only when its turn comes, so memory stays bounded by the
//! 20,000-entry scan budget however large the tree (0.12's fix for running
//! out of memory when searches overlapped on a large home). Every listing and
//! `realpath` goes through two shared slots, and searches that need the same
//! folder share one pending read, so a stuck mount holds one thread, not one
//! per keystroke.
//!
//! Blocking: run it off the async threads.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

use serde::Serialize;

use crate::git;

const DEFAULT_LIMIT: usize = 30;
const MAX_LIMIT: usize = 100;
const MAX_DEPTH: usize = 12;
const MAX_SCANNED: usize = 20_000;
/// A home search that has seen this many entries and has a good match stops.
pub const HOME_CONFIDENT_THRESHOLD: usize = 5_000;
const CONFIDENT_FUZZY_SKIPS_PER_CHAR: usize = 2;
const NO_INDEX: usize = usize::MAX;
const NO_TIER: usize = 5;

/// Dot folders a workspace search walks into.
pub const WORKSPACE_HIDDEN_DIRECTORIES: &[&str] = &[
    ".agents",
    ".claude",
    ".codex",
    ".cursor",
    ".github",
    ".opencode",
    ".paseo",
    ".vscode",
];
const IGNORED_DIRECTORIES: &[&str] = &[
    "node_modules",
    "venv",
    "env",
    "virtualenv",
    "dist",
    "build",
    "target",
    "out",
    "coverage",
    "vendor",
    "__pycache__",
    ".git",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
}

/// One search, in Paseo's terms (`SearchDirectoryEntriesOptions`).
#[derive(Debug, Clone)]
pub struct Options {
    pub root: PathBuf,
    pub query: String,
    /// Answer absolute paths (else relative to the root).
    pub absolute: bool,
    pub include_files: bool,
    pub include_directories: bool,
    /// `matchMode: "suffix"`: the query's segments end the path.
    pub suffix: bool,
    /// `pathQueryPolicy: "rooted"`: only a rooted query names a place.
    pub rooted_policy: bool,
    pub aliases: Vec<String>,
    /// `blankQueryBehavior: "children"`.
    pub blank_children: bool,
    pub hidden: Vec<String>,
    pub limit: Option<usize>,
    pub confident_threshold: Option<usize>,
    pub respect_gitignore: bool,
    pub excluded: Vec<PathBuf>,
}

impl Options {
    /// The Add Project search of the user's home.
    pub fn home(home: PathBuf, query: &str) -> Self {
        let excluded = if cfg!(target_os = "macos") {
            vec![home.join("Library")]
        } else {
            Vec::new()
        };
        Self {
            root: home,
            query: query.into(),
            absolute: true,
            include_files: false,
            include_directories: true,
            suffix: false,
            rooted_policy: true,
            aliases: vec!["~".into()],
            blank_children: false,
            hidden: Vec::new(),
            limit: None,
            confident_threshold: Some(HOME_CONFIDENT_THRESHOLD),
            respect_gitignore: false,
            excluded,
        }
    }

    /// A search inside one workspace.
    pub fn workspace(root: PathBuf, query: &str) -> Self {
        Self {
            root,
            query: query.into(),
            absolute: false,
            include_files: false,
            include_directories: true,
            suffix: false,
            rooted_policy: false,
            aliases: Vec::new(),
            blank_children: true,
            hidden: WORKSPACE_HIDDEN_DIRECTORIES
                .iter()
                .map(|s| s.to_string())
                .collect(),
            limit: None,
            confident_threshold: None,
            respect_gitignore: true,
            excluded: Vec::new(),
        }
    }
}

// --- shared, bounded filesystem calls -----------------------------------------

#[derive(Clone)]
enum Answer {
    Listing(Arc<Vec<(String, RawKind)>>),
    Path(Option<PathBuf>),
    Kind(Option<Kind>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RawKind {
    File,
    Directory,
    Symlink,
}

struct Slot {
    answer: Mutex<Option<Answer>>,
    ready: Condvar,
}

struct Calls {
    pending: Mutex<HashMap<String, Arc<Slot>>>,
    free: Mutex<usize>,
    freed: Condvar,
}

/// Two filesystem calls at a time across every search, as Paseo's
/// `pLimit(2)`.
fn calls() -> &'static Calls {
    static CALLS: OnceLock<Calls> = OnceLock::new();
    CALLS.get_or_init(|| Calls {
        pending: Mutex::new(HashMap::new()),
        free: Mutex::new(2),
        freed: Condvar::new(),
    })
}

fn shared(key: String, call: impl FnOnce() -> Answer) -> Answer {
    let calls = calls();
    let (slot, owner) = {
        let mut pending = calls.pending.lock().unwrap_or_else(|p| p.into_inner());
        match pending.get(&key) {
            Some(slot) => (slot.clone(), false),
            None => {
                let slot = Arc::new(Slot {
                    answer: Mutex::new(None),
                    ready: Condvar::new(),
                });
                pending.insert(key.clone(), slot.clone());
                (slot, true)
            }
        }
    };
    if owner {
        {
            let mut free = calls.free.lock().unwrap_or_else(|p| p.into_inner());
            while *free == 0 {
                free = calls.freed.wait(free).unwrap_or_else(|p| p.into_inner());
            }
            *free -= 1;
        }
        let answer = call();
        *calls.free.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        calls.freed.notify_one();
        calls
            .pending
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&key);
        *slot.answer.lock().unwrap_or_else(|p| p.into_inner()) = Some(answer.clone());
        slot.ready.notify_all();
        return answer;
    }
    let mut answer = slot.answer.lock().unwrap_or_else(|p| p.into_inner());
    while answer.is_none() {
        answer = slot.ready.wait(answer).unwrap_or_else(|p| p.into_inner());
    }
    answer.clone().expect("set before notifying")
}

fn realpath(path: &Path) -> Option<PathBuf> {
    match shared(format!("realpath:{}", path.display()), || {
        Answer::Path(std::fs::canonicalize(path).ok().map(strip_verbatim))
    }) {
        Answer::Path(path) => path,
        _ => None,
    }
}

fn kind_of(path: &Path) -> Option<Kind> {
    match shared(format!("stat:{}", path.display()), || {
        Answer::Kind(std::fs::metadata(path).ok().and_then(|m| {
            if m.is_dir() {
                Some(Kind::Directory)
            } else if m.is_file() {
                Some(Kind::File)
            } else {
                None
            }
        }))
    }) {
        Answer::Kind(kind) => kind,
        _ => None,
    }
}

fn listing(directory: &Path) -> Arc<Vec<(String, RawKind)>> {
    let answer = shared(format!("readdir:{}", directory.display()), || {
        let mut entries: Vec<(String, RawKind)> = std::fs::read_dir(directory)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter_map(|entry| {
                        let kind = entry.file_type().ok()?;
                        let kind = if kind.is_symlink() {
                            RawKind::Symlink
                        } else if kind.is_dir() {
                            RawKind::Directory
                        } else if kind.is_file() {
                            RawKind::File
                        } else {
                            return None;
                        };
                        Some((entry.file_name().to_string_lossy().into_owned(), kind))
                    })
                    .collect()
            })
            .unwrap_or_default();
        // `localeCompare`, near enough: case folded first.
        entries.sort_by(|a, b| {
            a.0.to_lowercase()
                .cmp(&b.0.to_lowercase())
                .then(a.0.cmp(&b.0))
        });
        Answer::Listing(Arc::new(entries))
    });
    match answer {
        Answer::Listing(list) => list,
        _ => Arc::new(Vec::new()),
    }
}

fn strip_verbatim(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy();
    match text.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => PathBuf::from(rest),
        _ => path,
    }
}

// --- the query -------------------------------------------------------------------

struct Plan {
    is_path_query: bool,
    parent_part: String,
    search_term: String,
    normalized: String,
    browse_exact: bool,
}

struct Input {
    root: PathBuf,
    plan: Plan,
    options: Options,
    limit: usize,
    hidden: HashSet<String>,
    ignored: HashSet<PathBuf>,
    excluded: Vec<PathBuf>,
}

fn inside(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// `path.relative` with `/`, `.` for the root itself.
fn relative(root: &Path, path: &Path) -> String {
    match path.strip_prefix(root) {
        Ok(rest) if rest.as_os_str().is_empty() => ".".into(),
        Ok(rest) => rest.to_string_lossy().replace('\\', "/"),
        Err(_) => path.to_string_lossy().replace('\\', "/"),
    }
}

fn parse_query(options: &Options, root: &Path) -> Option<Plan> {
    let configured = git::resolve(&options.root);
    let typed = options.query.trim().replace('\\', "/");
    let mut normalized = typed.clone();
    let mut rooted = false;
    let mut absolute = false;
    for alias in &options.aliases {
        if normalized == *alias || normalized.starts_with(&format!("{alias}/")) {
            rooted = true;
            normalized = normalized[alias.len()..]
                .trim_start_matches('/')
                .to_string();
            break;
        }
    }
    if Path::new(&normalized).is_absolute() {
        absolute = true;
        let browse = normalized.ends_with('/');
        let path = git::resolve(Path::new(&normalized));
        let query_root = if inside(root, &path) {
            root.to_path_buf()
        } else if inside(&configured, &path) {
            configured
        } else {
            return None;
        };
        rooted = true;
        normalized = relative(&query_root, &path);
        if browse && normalized != "." {
            normalized.push('/');
        }
    }
    if normalized.starts_with("./") {
        rooted = true;
    }
    let mut normalized = normalized.trim_start_matches("./").to_string();
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    if normalized == "." && (rooted || typed == ".") {
        normalized.clear();
    }
    if normalized.is_empty() {
        if !(rooted || typed == ".") && !options.blank_children {
            return None;
        }
        return Some(Plan {
            is_path_query: true,
            parent_part: String::new(),
            search_term: String::new(),
            normalized,
            browse_exact: false,
        });
    }
    let filesystem_root = root.parent().is_none();
    if absolute && filesystem_root && !normalized.contains('/') {
        return Some(Plan {
            is_path_query: true,
            parent_part: normalized.clone(),
            search_term: String::new(),
            normalized,
            browse_exact: true,
        });
    }
    let is_path_query = rooted || (!options.rooted_policy && normalized.contains('/'));
    let slash = normalized.rfind('/');
    let (parent_part, search_term) = match slash {
        Some(at) if is_path_query => (
            normalized[..at].to_string(),
            normalized[at + 1..].to_string(),
        ),
        _ => (String::new(), normalized.clone()),
    };
    Some(Plan {
        is_path_query,
        parent_part,
        search_term,
        normalized,
        browse_exact: false,
    })
}

fn git_ignored(root: &Path) -> HashSet<PathBuf> {
    let args = [
        "ls-files",
        "-o",
        "-i",
        "--directory",
        "--exclude-standard",
        "-z",
    ];
    match git::run(
        root,
        &args,
        &[("GIT_OPTIONAL_LOCKS", "0")],
        std::time::Duration::from_secs(10),
    ) {
        Ok(output) if output.ok() => output
            .stdout
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(|p| root.join(git::native(p.trim_end_matches('/'))))
            .collect(),
        _ => HashSet::new(),
    }
}

/// Runs one search.
pub fn search(options: Options) -> Vec<Entry> {
    let Some(root) = realpath(&git::resolve(&options.root)) else {
        return Vec::new();
    };
    if kind_of(&root) != Some(Kind::Directory) {
        return Vec::new();
    }
    if !options.include_directories && !options.include_files {
        return Vec::new();
    }
    let ignored = if options.respect_gitignore {
        git_ignored(&root)
    } else {
        HashSet::new()
    };
    let excluded = options
        .excluded
        .iter()
        .filter_map(|p| realpath(&git::resolve(p)))
        .collect();
    let Some(plan) = parse_query(&options, &root) else {
        return Vec::new();
    };
    let limit = options.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let input = Input {
        hidden: options.hidden.iter().cloned().collect(),
        root,
        plan,
        limit,
        ignored,
        excluded,
        options,
    };
    let typed_path = input.options.rooted_policy && input.plan.is_path_query;
    let exact = if input.plan.browse_exact
        || (input.plan.is_path_query && (input.options.suffix || typed_path))
    {
        exact_entry(&input)
    } else {
        None
    };
    if exact.is_some() && input.limit == 1 {
        return exact.into_iter().collect();
    }
    let ranked = candidates(&input);
    let results = sort_and_format(ranked, &input);
    match exact {
        Some(exact) => std::iter::once(exact.clone())
            .chain(results.into_iter().filter(|e| *e != exact))
            .take(input.limit)
            .collect(),
        None => results.into_iter().take(input.limit).collect(),
    }
}

fn format_entry(path: &Path, kind: Kind, input: &Input) -> Entry {
    Entry {
        path: if input.options.absolute {
            path.to_string_lossy().into_owned()
        } else {
            relative(&input.root, path)
        },
        kind,
    }
}

fn exact_entry(input: &Input) -> Option<Entry> {
    if input.plan.normalized.is_empty() {
        return None;
    }
    let visible = git::resolve(&input.root.join(&input.plan.normalized));
    let resolved = realpath(&visible)?;
    if !inside(&input.root, &resolved) {
        return None;
    }
    let kind = kind_of(&resolved)?;
    let wanted = match kind {
        Kind::Directory => input.options.include_directories,
        Kind::File => input.options.include_files,
    };
    wanted.then(|| format_entry(&visible, kind, input))
}

// --- walking ----------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Child {
    name: String,
    resolved: PathBuf,
    kind: Kind,
    via_symlink: bool,
}

#[derive(Debug, Clone)]
struct Traversed {
    child: Child,
    visible: PathBuf,
    depth: usize,
}

fn resolve_child(directory: &Path, name: &str, kind: RawKind) -> Option<Child> {
    let path = directory.join(name);
    match kind {
        RawKind::File | RawKind::Directory => Some(Child {
            name: name.into(),
            resolved: path,
            kind: if kind == RawKind::File {
                Kind::File
            } else {
                Kind::Directory
            },
            via_symlink: false,
        }),
        RawKind::Symlink => {
            let resolved = realpath(&path)?;
            let kind = kind_of(&resolved)?;
            Some(Child {
                name: name.into(),
                resolved,
                kind,
                via_symlink: true,
            })
        }
    }
}

fn excluded_path(path: &Path, excluded: &[PathBuf]) -> bool {
    excluded.iter().any(|e| inside(e, path))
}

fn ignored_path(path: &Path, input: &Input) -> bool {
    let mut candidate = path.to_path_buf();
    while candidate != input.root && inside(&input.root, &candidate) {
        if input.ignored.contains(&candidate) {
            return true;
        }
        if !candidate.pop() {
            break;
        }
    }
    false
}

fn discoverable(child: &Child, input: &Input, excluded: &[PathBuf]) -> bool {
    let ignored = if child.via_symlink {
        ignored_path(&child.resolved, input)
    } else {
        input.ignored.contains(&child.resolved)
    };
    let excluded = !excluded.is_empty()
        && if child.via_symlink {
            excluded_path(&child.resolved, excluded)
        } else {
            excluded.contains(&child.resolved)
        };
    if ignored || excluded {
        return false;
    }
    if child.kind == Kind::File {
        return input.options.include_files && !child.name.starts_with('.');
    }
    if IGNORED_DIRECTORIES.contains(&child.name.as_str()) {
        return false;
    }
    !child.name.starts_with('.') || input.hidden.contains(&child.name)
}

struct Walk<'a> {
    input: &'a Input,
    visited: HashSet<PathBuf>,
}

/// One folder's children, created as their turn comes (`walkChildren`).
struct Source {
    directory: PathBuf,
    visible: PathBuf,
    depth: usize,
    listing: Option<Arc<Vec<(String, RawKind)>>>,
    at: usize,
}

impl Source {
    fn new(directory: PathBuf, visible: PathBuf, depth: usize) -> Self {
        Self {
            directory,
            visible,
            depth,
            listing: None,
            at: 0,
        }
    }

    fn next(&mut self, walk: &Walk) -> Option<Branch> {
        if self.listing.is_none() {
            let listing = if kind_of(&self.directory) == Some(Kind::Directory) {
                listing(&self.directory)
            } else {
                Arc::new(Vec::new())
            };
            self.listing = Some(listing);
        }
        let listing = self.listing.clone()?;
        while let Some((name, kind)) = listing.get(self.at) {
            self.at += 1;
            let Some(child) = resolve_child(&self.directory, name, *kind) else {
                continue;
            };
            let stays = !child.via_symlink || inside(&walk.input.root, &child.resolved);
            if !stays || !discoverable(&child, walk.input, &walk.input.excluded) {
                continue;
            }
            return Some(Branch::Fresh(Traversed {
                visible: self.visible.join(&child.name),
                child,
                depth: self.depth,
            }));
        }
        None
    }
}

/// `walkBranch`: an entry, then its subtree round-robin.
enum Branch {
    Fresh(Traversed),
    Pending(Traversed),
    Children(Box<RoundRobin>),
    Done,
}

impl Branch {
    fn next(&mut self, walk: &mut Walk) -> Option<Traversed> {
        match std::mem::replace(self, Branch::Done) {
            Branch::Fresh(entry) => {
                *self = Branch::Pending(entry.clone());
                Some(entry)
            }
            Branch::Pending(entry) => {
                if entry.child.kind != Kind::Directory
                    || walk.visited.contains(&entry.child.resolved)
                    || entry.depth >= MAX_DEPTH
                {
                    return None;
                }
                walk.visited.insert(entry.child.resolved.clone());
                let mut children = Box::new(RoundRobin::new(Source::new(
                    entry.child.resolved,
                    entry.visible,
                    entry.depth + 1,
                )));
                let next = children.next(walk);
                if next.is_some() {
                    *self = Branch::Children(children);
                }
                next
            }
            Branch::Children(mut children) => {
                let next = children.next(walk);
                if next.is_some() {
                    *self = Branch::Children(children);
                }
                next
            }
            Branch::Done => None,
        }
    }
}

/// `roundRobin`: one item from each branch in turn, until all are done.
struct RoundRobin {
    source: Option<Source>,
    active: VecDeque<Branch>,
    next_round: Vec<Branch>,
}

impl RoundRobin {
    fn new(source: Source) -> Self {
        Self {
            source: Some(source),
            active: VecDeque::new(),
            next_round: Vec::new(),
        }
    }

    fn next(&mut self, walk: &mut Walk) -> Option<Traversed> {
        loop {
            if let Some(source) = self.source.as_mut() {
                match source.next(walk) {
                    Some(mut branch) => {
                        if let Some(item) = branch.next(walk) {
                            self.next_round.push(branch);
                            return Some(item);
                        }
                    }
                    None => self.source = None,
                }
                continue;
            }
            if let Some(mut branch) = self.active.pop_front() {
                if let Some(item) = branch.next(walk) {
                    self.next_round.push(branch);
                    return Some(item);
                }
                continue;
            }
            if self.next_round.is_empty() {
                return None;
            }
            self.active = std::mem::take(&mut self.next_round).into();
        }
    }
}

fn candidates(input: &Input) -> Vec<Ranked> {
    let plan = &input.plan;
    if (plan.is_path_query && plan.normalized.is_empty()) || plan.browse_exact {
        return children(input);
    }
    let typed_path = input.options.rooted_policy && plan.is_path_query;
    if !typed_path || plan.parent_part.is_empty() {
        return tree(input, input.root.clone(), input.root.clone());
    }
    // A typed parent anchors the walk; one that is not there matches the
    // typed path against the whole tree.
    let visible = git::resolve(&input.root.join(&plan.parent_part));
    let parent = realpath(&visible)
        .filter(|p| inside(&input.root, p) && kind_of(p) == Some(Kind::Directory));
    match parent {
        None => tree(input, input.root.clone(), input.root.clone()),
        Some(parent) if excluded_path(&parent, &input.excluded) => children(input),
        Some(parent) => tree(input, visible, parent),
    }
}

fn children(input: &Input) -> Vec<Ranked> {
    let part = if input.plan.parent_part.is_empty() {
        "."
    } else {
        &input.plan.parent_part
    };
    let visible = git::resolve(&input.root.join(part));
    let Some(parent) = realpath(&visible) else {
        return Vec::new();
    };
    if !inside(&input.root, &parent) || ignored_path(&parent, input) {
        return Vec::new();
    }
    // Browsing inside an excluded folder was asked for: list what is there.
    let excluded: &[PathBuf] = if excluded_path(&parent, &input.excluded) {
        &[]
    } else {
        &input.excluded
    };
    let mut ranked = Vec::new();
    for (name, kind) in listing(&parent).iter() {
        let Some(child) = resolve_child(&parent, name, *kind) else {
            continue;
        };
        if !inside(&input.root, &child.resolved) || !discoverable(&child, input, excluded) {
            continue;
        }
        let entry = Traversed {
            visible: visible.join(&child.name),
            child,
            depth: 1,
        };
        if suggestable(&entry, input) {
            ranked.push(rank(&entry, input));
        }
    }
    ranked
}

fn tree(input: &Input, visible: PathBuf, resolved: PathBuf) -> Vec<Ranked> {
    let mut walk = Walk {
        input,
        visited: HashSet::from([resolved.clone()]),
    };
    let mut branches = RoundRobin::new(Source::new(resolved, visible, 1));
    let query = if input.plan.normalized.is_empty() {
        &input.plan.search_term
    } else {
        &input.plan.normalized
    };
    let mut ranked = Vec::new();
    let mut scanned = 0;
    while let Some(entry) = branches.next(&mut walk) {
        scanned += 1;
        if suggestable(&entry, input) {
            ranked.push(rank(&entry, input));
        }
        let confident = input
            .options
            .confident_threshold
            .is_some_and(|t| scanned >= t && confident_result(&ranked, query));
        if scanned >= MAX_SCANNED || confident {
            break;
        }
    }
    ranked
}

// --- ranking ----------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Ranked {
    path: PathBuf,
    kind: Kind,
    tier: usize,
    segment: usize,
    offset: usize,
    fuzzy: usize,
    depth: usize,
}

fn suggestable(entry: &Traversed, input: &Input) -> bool {
    if entry.child.name.starts_with('.') {
        return false;
    }
    let wanted = match entry.child.kind {
        Kind::Directory => input.options.include_directories,
        Kind::File => input.options.include_files,
    };
    if !wanted {
        return false;
    }
    if input.plan.normalized.is_empty() {
        return true;
    }
    if input.options.suffix {
        return suffix_matches(
            &relative(&input.root, &entry.visible),
            &input.plan.normalized,
        );
    }
    rank(entry, input).tier != NO_TIER
}

fn rank(entry: &Traversed, input: &Input) -> Ranked {
    let relative_path = relative(&input.root, &entry.visible);
    let lower = relative_path.to_lowercase();
    let query = if input.plan.is_path_query {
        &input.plan.normalized
    } else {
        &input.plan.search_term
    }
    .to_lowercase();
    let segments: Vec<&str> = if lower == "." {
        Vec::new()
    } else {
        lower.split('/').collect()
    };
    let score = (!query.is_empty())
        .then(|| score_path(&query, &relative_path))
        .flatten();
    let by_score = |score: Option<Score>| match score {
        Some(score) => (
            score.tier.min(4),
            NO_INDEX,
            score.offset,
            score.spread.unwrap_or(NO_INDEX),
        ),
        None => (NO_TIER, NO_INDEX, NO_INDEX, NO_INDEX),
    };
    let (tier, segment, offset, fuzzy) = if input.plan.is_path_query {
        by_score(score)
    } else if query.is_empty() {
        (3, NO_INDEX, NO_INDEX, NO_INDEX)
    } else {
        let found = lower.find(&query);
        let at = found.unwrap_or(NO_INDEX);
        let fuzzy = fuzzy_subsequence(&query, segments.last().copied().unwrap_or_default());
        let fuzzy_or = fuzzy.unwrap_or(NO_INDEX);
        let segment_match = |test: &dyn Fn(&str) -> bool| segments.iter().position(|s| test(s));
        if let Some(index) = segment_match(&|s| s == query) {
            (0, index, at, fuzzy_or)
        } else if let Some(index) = segment_match(&|s| s.starts_with(&query)) {
            (1, index, at, fuzzy_or)
        } else if let Some(index) = segment_match(&|s| s.contains(&query)) {
            (2, index, at, fuzzy_or)
        } else if (!input.options.absolute && lower.starts_with(&query))
            || (input.options.absolute && found.is_some())
        {
            (3, NO_INDEX, at, fuzzy_or)
        } else if let Some(fuzzy) = fuzzy {
            (4, NO_INDEX, at, fuzzy)
        } else {
            by_score(score)
        }
    };
    Ranked {
        path: entry.visible.clone(),
        kind: entry.child.kind,
        tier,
        segment,
        offset,
        fuzzy,
        depth: segments.len(),
    }
}

fn compare(a: &Ranked, b: &Ranked) -> std::cmp::Ordering {
    let kind = |k: Kind| u8::from(k != Kind::Directory);
    (a.tier, a.segment, a.offset, a.fuzzy, a.depth, kind(a.kind))
        .cmp(&(b.tier, b.segment, b.offset, b.fuzzy, b.depth, kind(b.kind)))
        .then_with(|| {
            let (a, b) = (a.path.to_string_lossy(), b.path.to_string_lossy());
            a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(&b))
        })
}

fn sort_and_format(ranked: Vec<Ranked>, input: &Input) -> Vec<Entry> {
    let mut best: HashMap<(Kind, PathBuf), Ranked> = HashMap::new();
    for entry in ranked {
        let key = (entry.kind, entry.path.clone());
        match best.get(&key) {
            Some(existing) if compare(&entry, existing) != std::cmp::Ordering::Less => {}
            _ => {
                best.insert(key, entry);
            }
        }
    }
    let mut unique: Vec<Ranked> = best.into_values().collect();
    unique.sort_by(compare);
    unique
        .into_iter()
        .map(|entry| format_entry(&entry.path, entry.kind, input))
        .collect()
}

fn confident_result(ranked: &[Ranked], query: &str) -> bool {
    let max_fuzzy = query.chars().count() * CONFIDENT_FUZZY_SKIPS_PER_CHAR;
    ranked
        .iter()
        .any(|e| e.tier < 4 || (e.tier == 4 && e.fuzzy <= max_fuzzy))
}

fn suffix_matches(relative_path: &str, query: &str) -> bool {
    let query: Vec<String> = query
        .to_lowercase()
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    if query.is_empty() {
        return false;
    }
    let lower = relative_path.to_lowercase();
    let path: Vec<&str> = lower.split('/').filter(|s| !s.is_empty()).collect();
    path.len() >= query.len()
        && path[path.len() - query.len()..]
            .iter()
            .zip(&query)
            .all(|(a, b)| a == b)
}

fn fuzzy_subsequence(query: &str, candidate: &str) -> Option<usize> {
    let query: Vec<char> = query.chars().collect();
    let mut at = 0;
    let (mut first, mut previous, mut gaps) = (None, None, 0);
    for (index, c) in candidate.chars().enumerate() {
        if at == query.len() {
            break;
        }
        if c != query[at] {
            continue;
        }
        first.get_or_insert(index);
        if let Some(previous) = previous {
            gaps += index - previous - 1;
        }
        previous = Some(index);
        at += 1;
    }
    (at == query.len()).then_some(first? + gaps)
}

/// `MatchScore` from Paseo's `search/text-match.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Score {
    tier: usize,
    offset: usize,
    spread: Option<usize>,
}

fn boundary(c: Option<char>) -> bool {
    c.is_none_or(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
}

/// `scoreMatch` without typo tolerance.
fn score_match(query: &str, text: &str) -> Option<Score> {
    if query.is_empty() {
        return Some(Score {
            tier: 0,
            offset: 0,
            spread: None,
        });
    }
    let q: Vec<char> = query.to_lowercase().chars().collect();
    let t: Vec<char> = text.to_lowercase().chars().collect();
    if q == t {
        return Some(Score {
            tier: 0,
            offset: 0,
            spread: None,
        });
    }
    let mut best: Option<Score> = None;
    if q.len() <= t.len() {
        for found in 0..=t.len() - q.len() {
            if t[found..found + q.len()] != q[..] {
                continue;
            }
            let before = found.checked_sub(1).map(|i| t[i]);
            let after = t.get(found + q.len()).copied();
            let starts = found == 0 || boundary(before);
            let tier = if starts && boundary(after) {
                1
            } else if found == 0 {
                2
            } else if starts {
                3
            } else {
                4
            };
            if best.is_none_or(|b| tier < b.tier || (tier == b.tier && found < b.offset)) {
                best = Some(Score {
                    tier,
                    offset: found,
                    spread: None,
                });
            }
        }
    }
    if best.is_some() {
        return best;
    }
    // In order, within one whitespace-delimited word.
    let (mut at, mut first, mut last) = (0, None, 0);
    for (index, c) in t.iter().enumerate() {
        if at == q.len() {
            break;
        }
        if c.is_whitespace() {
            (at, first) = (0, None);
            continue;
        }
        if *c != q[at] {
            continue;
        }
        first.get_or_insert(index);
        last = index;
        at += 1;
    }
    let first = first?;
    (at == q.len()).then_some(Score {
        tier: 5,
        offset: first,
        spread: Some(last - first + 1),
    })
}

/// `scorePathMatch`: the path as shown, then letters and digits only.
fn score_path(query: &str, path: &str) -> Option<Score> {
    if let Some(score) = score_match(query, path) {
        return Some(score);
    }
    let compact = |text: &str| -> (String, Vec<usize>) {
        let mut out = String::new();
        let mut offsets = Vec::new();
        for (index, c) in text.chars().enumerate() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_lowercase());
                offsets.push(index);
            }
        }
        (out, offsets)
    };
    let (query, _) = compact(query);
    let (text, offsets) = compact(path);
    if query.is_empty() || text.is_empty() {
        return None;
    }
    let score = score_match(&query, &text)?;
    Some(Score {
        offset: offsets.get(score.offset).copied().unwrap_or(score.offset),
        ..score
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree_at(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("burrow-suggest-{name}-{}", crate::ids::agent_id()));
        for dir in [
            "Developer/paseo",
            "Developer/crusteo/src",
            "Developer/crusteo/node_modules/junk",
            "notes/.hidden",
            "Library/Caches",
        ] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        std::fs::write(root.join("Developer/readme.md"), "x").unwrap();
        git::canonical(&root)
    }

    fn paths(entries: &[Entry]) -> Vec<String> {
        entries.iter().map(|e| e.path.clone()).collect()
    }

    fn under(root: &Path, parts: &[&str]) -> String {
        parts
            .iter()
            .fold(root.to_path_buf(), |path, part| path.join(part))
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn home_searches_answer_absolute_folders() {
        let home = tree_at("home");
        let found = search(Options::home(home.clone(), "crus"));
        assert_eq!(paths(&found)[0], under(&home, &["Developer", "crusteo"]));
        assert!(found.iter().all(|e| e.kind == Kind::Directory));
        // A typed path anchors the walk; an empty query answers nothing.
        let typed = search(Options::home(home.clone(), "~/Developer/pa"));
        assert_eq!(paths(&typed)[0], under(&home, &["Developer", "paseo"]));
        assert!(search(Options::home(home.clone(), "")).is_empty());
        let browse = search(Options::home(home.clone(), "~/"));
        assert!(paths(&browse).contains(&under(&home, &["Developer"])));
        assert!(!paths(&browse).iter().any(|p| p.contains(".hidden")));
        // Ignored folders are never offered.
        let junk = search(Options::home(home.clone(), "junk"));
        assert!(junk.is_empty(), "{junk:?}");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn workspace_searches_answer_relative_paths() {
        let root = tree_at("workspace");
        let top = search(Options::workspace(root.clone(), ""));
        assert!(paths(&top).contains(&"Developer".to_string()), "{top:?}");
        let mut files = Options::workspace(root.clone(), "readme");
        files.include_files = true;
        assert_eq!(paths(&search(files)), ["Developer/readme.md"]);
        let deep = search(Options::workspace(root.clone(), "crusteo/src"));
        assert_eq!(paths(&deep)[0], "Developer/crusteo/src");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn excluded_folders_are_not_walked_but_can_be_browsed() {
        let home = tree_at("excluded");
        let mut options = Options::home(home.clone(), "Caches");
        options.excluded = vec![home.join("Library")];
        assert!(search(options.clone()).is_empty());
        options.query = "~/Library/".into();
        let listed = search(options);
        assert!(
            paths(&listed).iter().any(|p| p.ends_with("Caches")),
            "{listed:?}"
        );
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn scores_follow_paseo() {
        assert_eq!(score_match("dev", "developer").unwrap().tier, 2);
        assert_eq!(score_match("src", "crusteo/src").unwrap().tier, 1);
        assert_eq!(score_match("cst", "crusteo").unwrap().tier, 5);
        assert_eq!(score_path("crusteo-src", "crusteo/src").unwrap().tier, 0);
        assert_eq!(fuzzy_subsequence("cro", "crusteo"), Some(4));
    }
}
