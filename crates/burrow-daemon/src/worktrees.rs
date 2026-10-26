//! Burrow-owned git worktrees, laid out and cut as Paseo 0.12 does
//! (`utils/worktree.ts`, `worktree-core.ts`, `resolve-worktree-creation-intent.ts`).
//!
//! ```text
//! <worktrees root>/<project hash>/<slug>[-N]     one worktree, git's own
//! <its git dir>/paseo/worktree.json              its base ref, as Paseo writes it
//! ```
//!
//! The worktrees root is `<home>/worktrees` (`--worktrees-root` overrides it).
//! The project hash is Paseo's: the first eight base-36 digits of a SHA-256 of
//! the main checkout's real path, so one repository's worktrees share a folder
//! whichever checkout they were cut from. A path that is taken gets `-1`,
//! `-2`, ... A branch name that is taken, or that would nest under or over an
//! existing branch, gets the same suffix on the clashing segment.
//!
//! Everything here blocks on `git`: run it off the async threads.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::git::{self, DEFAULT_TIMEOUT};

/// Paseo's `MAX_SLUG_LENGTH`.
const MAX_SLUG_LENGTH: usize = 50;
/// How long refreshing a remote-tracking base may take before the cached
/// ref is used (Paseo: clients give up on a create after 60 s).
const REMOTE_BASE_REFRESH: Duration = Duration::from_secs(15);

/// Why a worktree could not be made, with Paseo's wire code
/// (`toWorktreeWireError`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeError {
    pub code: &'static str,
    pub message: String,
}

impl WorktreeError {
    pub fn unknown(message: impl Into<String>) -> Self {
        Self {
            code: "unknown",
            message: message.into(),
        }
    }
}

impl From<String> for WorktreeError {
    fn from(message: String) -> Self {
        Self::unknown(message)
    }
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// What to cut a worktree from (`WorktreeCreationIntent`, without the
/// change-request kinds, which need a forge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// A new branch `branch_name` off `base_branch`.
    BranchOff {
        base_branch: String,
        branch_name: String,
    },
    /// An existing branch, checked out in the new worktree.
    CheckoutBranch { branch_name: String },
}

impl Intent {
    pub fn branch_name(&self) -> &str {
        match self {
            Intent::BranchOff { branch_name, .. } | Intent::CheckoutBranch { branch_name } => {
                branch_name
            }
        }
    }
}

/// A worktree just made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub branch_name: String,
    pub worktree_path: String,
    /// The exact ref it is compared against (`baseBranch` on its workspace).
    pub comparison_base_ref: String,
}

// --- names ------------------------------------------------------------------

/// Paseo's `slugify`: lowercase, runs of anything else become one `-`, at
/// most 50 characters, cut at a hyphen in the second half when there is one.
pub fn slugify(input: &str) -> String {
    let mut slug = String::new();
    let mut gap = false;
    for c in input.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            if gap && !slug.is_empty() {
                slug.push('-');
            }
            gap = false;
            slug.push(c);
        } else {
            gap = true;
        }
    }
    if slug.len() <= MAX_SLUG_LENGTH {
        return slug;
    }
    let truncated = &slug[..MAX_SLUG_LENGTH];
    match truncated.rfind('-') {
        Some(at) if at > MAX_SLUG_LENGTH / 2 => truncated[..at].to_string(),
        _ => truncated.trim_end_matches('-').to_string(),
    }
}

/// Paseo's `validateBranchSlug`, with its messages.
pub fn validate_slug(slug: &str) -> Result<(), String> {
    if slug.is_empty() {
        return Err("Branch name cannot be empty".into());
    }
    if slug.len() > 100 {
        return Err("Branch name too long (max 100 characters)".into());
    }
    if !slug
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'/')
    {
        return Err(
            "Branch name must contain only lowercase letters, numbers, hyphens, and forward slashes"
                .into(),
        );
    }
    if slug.starts_with('-') || slug.ends_with('-') {
        return Err("Branch name cannot start or end with a hyphen".into());
    }
    if slug.contains("--") {
        return Err("Branch name cannot have consecutive hyphens".into());
    }
    Ok(())
}

/// A worktree slug: slugified, then validated (`normalizeWorktreeSlug`).
pub fn worktree_slug(value: &str) -> Result<String, String> {
    let slug = slugify(value);
    validate_slug(&slug).map_err(|error| format!("Invalid worktree name: {error}"))?;
    Ok(slug)
}

/// Paseo's `assertSafeGitRef`.
pub fn assert_safe_ref(reference: &str, label: &str) -> Result<(), String> {
    let safe = !reference.is_empty()
        && reference
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
        && !reference.contains("..")
        && !reference.contains("@{");
    if safe {
        Ok(())
    } else {
        Err(format!("Invalid {label}: {reference}"))
    }
}

const ADJECTIVES: &[&str] = &[
    "amber", "brave", "brisk", "calm", "clever", "cosy", "crisp", "dapper", "eager", "fair",
    "gentle", "glad", "golden", "happy", "humble", "jolly", "keen", "kind", "lively", "lucky",
    "mellow", "merry", "nimble", "noble", "plucky", "polite", "proud", "quick", "quiet", "rapid",
    "rosy", "shiny", "snug", "steady", "sunny", "swift", "tidy", "vivid", "warm", "witty",
];
const NOUNS: &[&str] = &[
    "badger", "beaver", "bison", "crane", "falcon", "ferret", "finch", "fox", "gecko", "heron",
    "ibis", "koala", "lark", "lemur", "lynx", "marten", "mole", "moose", "newt", "otter", "owl",
    "panda", "puffin", "quail", "raven", "robin", "salmon", "seal", "shrew", "sparrow", "stoat",
    "swan", "tapir", "tern", "toad", "trout", "vole", "walrus", "wombat", "wren",
];

/// A random two-word name for a worktree nobody named, as Paseo's
/// `createNameId()` gives one (`brisk-otter`).
pub fn random_name() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let adjective = ADJECTIVES[usize::from(bytes[0]) % ADJECTIVES.len()];
    let noun = NOUNS[usize::from(bytes[1]) % NOUNS.len()];
    format!("{adjective}-{noun}")
}

/// `branchNameFromRef`: the branch a ref names, for display.
pub fn branch_name_from_ref(reference: &str) -> String {
    let trimmed = reference.trim();
    if let Some(rest) = trimmed.strip_prefix("refs/heads/") {
        return rest.to_string();
    }
    if let Some(rest) = trimmed.strip_prefix("refs/remotes/") {
        return rest.split_once('/').map_or(rest, |(_, b)| b).to_string();
    }
    trimmed
        .strip_prefix("origin/")
        .unwrap_or(trimmed)
        .to_string()
}

fn required_base_name(base: &str) -> Result<String, String> {
    if base.trim().is_empty() {
        return Err("Base branch is required when creating a Paseo worktree".into());
    }
    let name = branch_name_from_ref(base);
    if name.is_empty() {
        return Err("Base branch is required when creating a Paseo worktree".into());
    }
    if name == "HEAD" {
        return Err("Base branch cannot be HEAD when creating a Paseo worktree".into());
    }
    Ok(name)
}

// --- where worktrees go --------------------------------------------------------

/// Paseo's `deriveWorktreeProjectHash`: eight base-36 digits from a SHA-256
/// of the main checkout's real path (of `cwd` itself outside a repository).
pub fn project_hash(cwd: &Path) -> String {
    let root = git::main_repo_root(cwd)
        .unwrap_or_else(|| git::canonical(cwd).to_string_lossy().into_owned());
    short_hash(&root)
}

fn short_hash(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    let mut number = 0u64;
    for byte in &digest[..8] {
        number = (number << 8) | u64::from(*byte);
    }
    let mut digits = Vec::new();
    while number > 0 {
        let digit = (number % 36) as u8;
        digits.push(if digit < 10 {
            b'0' + digit
        } else {
            b'a' + digit - 10
        });
        number /= 36;
    }
    while digits.len() < 13 {
        digits.push(b'0');
    }
    digits.reverse();
    String::from_utf8_lossy(&digits[..8]).into_owned()
}

/// The folder a repository's worktrees go in.
pub fn project_root(cwd: &Path, base_root: &Path) -> PathBuf {
    base_root.join(project_hash(cwd))
}

/// `isPaseoOwnedWorktreeCwd`: a path under `<root>/<hash>/<slug>` is the
/// Burrow's, whatever git still remembers. Returns that worktree's folder.
pub fn owned_worktree(path: &Path, base_root: &Path) -> Option<PathBuf> {
    let relative = git::relative_inside(base_root, path)?;
    let mut parts = relative.components();
    let (hash, slug) = (parts.next()?, parts.next()?);
    Some(git::canonical(base_root).join(hash).join(slug))
}

// --- branches -------------------------------------------------------------------

pub fn local_branch_exists(cwd: &Path, branch: &str) -> bool {
    git::succeeds(
        cwd,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
}

/// `resolveRepositoryDefaultBranch`: `origin/HEAD` (as a local name when
/// that branch exists locally), else `main`, else `master`.
pub fn default_branch(repo: &Path) -> Option<String> {
    if let Some(reference) = git::read(
        repo,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    ) {
        let short = reference.trim_start_matches("refs/remotes/").to_string();
        let local = short.strip_prefix("origin/").unwrap_or(&short).to_string();
        if local_branch_exists(repo, &local) {
            return Some(local);
        }
        if git::succeeds(repo, &["show-ref", "--verify", "--quiet", &reference]) {
            return Some(short);
        }
    }
    let branches = git::read(repo, &["branch", "--format=%(refname:short)"]).unwrap_or_default();
    let has = |name: &str| branches.lines().any(|line| line.trim() == name);
    ["main", "master"]
        .into_iter()
        .find(|name| has(name))
        .map(str::to_string)
}

/// The repository's default branch, or Paseo's error.
pub fn require_default_branch(repo: &Path) -> Result<String, String> {
    default_branch(repo).ok_or_else(|| "Unable to resolve repository default branch".into())
}

/// `resolveBaseBranchForWorktree`: the exact ref a base name means.
fn resolve_base_ref(cwd: &Path, requested: &str) -> Result<String, String> {
    let requested = requested.trim();
    let normalized = required_base_name(requested)?;
    let exact = if requested.starts_with("refs/") {
        Some(requested.to_string())
    } else {
        requested
            .strip_prefix("origin/")
            .map(|_| format!("refs/remotes/{requested}"))
    };
    if let Some(exact) = exact {
        return if git::succeeds(cwd, &["rev-parse", "--verify", &exact]) {
            Ok(exact)
        } else {
            Err(format!("Base branch not found: {exact}"))
        };
    }
    [
        format!("refs/heads/{requested}"),
        format!("refs/remotes/origin/{requested}"),
        requested.to_string(),
    ]
    .into_iter()
    .find(|candidate| git::succeeds(cwd, &["rev-parse", "--verify", candidate]))
    .ok_or_else(|| format!("Base branch not found: {normalized}"))
}

/// A remote-tracking base is as fresh as the last fetch: fetch that one
/// branch, briefly, and branch from the cached ref when the remote is away.
fn refresh_remote_base(cwd: &Path, base: &str) {
    let Some(rest) = base.strip_prefix("refs/remotes/") else {
        return;
    };
    let remotes = git::read(cwd, &["remote"]).unwrap_or_default();
    let Some(remote) = remotes
        .lines()
        .map(str::trim)
        .find(|name| !name.is_empty() && rest.starts_with(&format!("{name}/")))
    else {
        return;
    };
    let branch = &rest[remote.len() + 1..];
    let refspec = format!("+refs/heads/{branch}:{base}");
    let _ = git::run(cwd, &["fetch", remote, &refspec], &[], REMOTE_BASE_REFRESH);
}

fn validate_git_branch_name(cwd: &Path, branch: &str) -> Result<(), String> {
    if git::succeeds(cwd, &["check-ref-format", "--branch", branch]) {
        Ok(())
    } else {
        Err(format!(
            "Invalid branch name: Git rejected ref name '{branch}'"
        ))
    }
}

/// `resolveUniqueLocalBranchName`: the candidate, or with `-N` on the segment
/// that clashes with an existing branch (as itself, a parent or a child).
fn unique_branch_name(cwd: &Path, candidate: &str) -> String {
    let listed = git::read(
        cwd,
        &["for-each-ref", "--format=%(refname:strip=2)", "refs/heads/"],
    )
    .unwrap_or_default();
    let existing: Vec<&str> = listed.lines().filter(|l| !l.is_empty()).collect();
    let original: Vec<&str> = candidate.split('/').collect();
    let mut segments: Vec<String> = original.iter().map(|s| s.to_string()).collect();
    let mut suffix = 1;
    loop {
        let name = segments.join("/");
        let parent =
            (0..segments.len()).find(|&i| existing.contains(&segments[..=i].join("/").as_str()));
        let child = existing
            .iter()
            .any(|branch| branch.starts_with(&format!("{name}/")));
        if parent.is_none() && !child {
            return name;
        }
        let at = parent.unwrap_or(segments.len() - 1);
        segments[at] = format!("{}-{suffix}", original[at]);
        suffix += 1;
    }
}

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Listed {
    pub path: String,
    pub branch_name: Option<String>,
    pub head: Option<String>,
}

fn worktree_list(cwd: &Path) -> Vec<Listed> {
    let output = git::read(cwd, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let mut entries = Vec::new();
    let mut current: Option<Listed> = None;
    for line in output.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.extend(current.take());
            current = Some(Listed {
                path: git::native(path.trim()),
                ..Listed::default()
            });
        } else if let Some(entry) = current.as_mut() {
            if let Some(reference) = line.strip_prefix("branch ") {
                let reference = reference.trim();
                entry.branch_name = Some(
                    reference
                        .strip_prefix("refs/heads/")
                        .unwrap_or(reference)
                        .to_string(),
                );
            } else if let Some(head) = line.strip_prefix("HEAD ") {
                entry.head = Some(head.trim().to_string());
            } else if line.trim().is_empty() {
                entries.extend(current.take());
            }
        }
    }
    entries.extend(current);
    entries
}

fn branch_checked_out(cwd: &Path, branch: &str) -> bool {
    worktree_list(cwd)
        .iter()
        .any(|entry| entry.branch_name.as_deref() == Some(branch))
}

/// `ensureLocalBranch`: a branch only `origin` has is fetched first.
fn ensure_local_branch(cwd: &Path, branch: &str) -> Result<(), WorktreeError> {
    if local_branch_exists(cwd, branch) {
        return Ok(());
    }
    let refspec = format!("{branch}:{branch}");
    match git::run(cwd, &["fetch", "origin", &refspec], &[], DEFAULT_TIMEOUT) {
        Ok(output) if output.ok() => Ok(()),
        _ => Err(WorktreeError {
            code: "unknown_branch",
            message: format!("Unknown branch: {branch}"),
        }),
    }
}

// --- making and removing worktrees ------------------------------------------------

struct Plan {
    branch_name: String,
    base_ref_name: String,
    base_ref: Option<String>,
    add: Vec<String>,
}

fn plan(cwd: &Path, intent: &Intent, slug: &str) -> Result<Plan, WorktreeError> {
    match intent {
        Intent::BranchOff {
            base_branch,
            branch_name,
        } => {
            validate_git_branch_name(cwd, branch_name)?;
            let base_ref_name = required_base_name(base_branch)?;
            let resolved = resolve_base_ref(cwd, base_branch)?;
            let exists = local_branch_exists(cwd, branch_name);
            let base = if exists {
                branch_name.clone()
            } else {
                resolved.clone()
            };
            refresh_remote_base(cwd, &base);
            let candidate = if exists { slug } else { branch_name.as_str() };
            let branch_name = unique_branch_name(cwd, candidate);
            Ok(Plan {
                add: vec!["-b".into(), branch_name.clone(), "--no-track".into(), base],
                branch_name,
                base_ref_name,
                base_ref: Some(resolved),
            })
        }
        Intent::CheckoutBranch { branch_name } => {
            validate_git_branch_name(cwd, branch_name)?;
            ensure_local_branch(cwd, branch_name)?;
            // Not the branch itself: what it should be compared with.
            let base_ref_name = default_branch(cwd)
                .or_else(|| {
                    git::read(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])
                        .filter(|current| current != branch_name && current != "HEAD")
                })
                .unwrap_or_else(|| branch_name.clone());
            if branch_checked_out(cwd, branch_name) {
                let unique = unique_branch_name(cwd, branch_name);
                return Ok(Plan {
                    add: vec![
                        "-b".into(),
                        unique.clone(),
                        "--no-track".into(),
                        branch_name.clone(),
                    ],
                    branch_name: unique,
                    base_ref_name,
                    base_ref: None,
                });
            }
            Ok(Plan {
                add: vec![branch_name.clone()],
                branch_name: branch_name.clone(),
                base_ref_name,
                base_ref: None,
            })
        }
    }
}

/// `createWorktree`: `git worktree add` at `<root>/<hash>/<slug>[-N]`, then
/// Paseo's metadata file, then `paseo.json` copied in when git did not
/// bring one. `repo` is the checkout to cut from.
pub fn create(
    repo: &Path,
    slug: &str,
    intent: &Intent,
    base_root: &Path,
) -> Result<Created, WorktreeError> {
    let plan = plan(repo, intent, slug)?;
    let folder = project_root(repo, base_root);
    std::fs::create_dir_all(&folder)
        .map_err(|err| format!("Failed to create {}: {err}", folder.display()))?;
    let wanted = folder.join(slug);
    let mut path = wanted.clone();
    let mut suffix = 1;
    while path.exists() {
        path = PathBuf::from(format!("{}-{suffix}", wanted.to_string_lossy()));
        suffix += 1;
    }
    let path_text = path.to_string_lossy().into_owned();
    let mut args = vec!["worktree", "add", path_text.as_str()];
    args.extend(plan.add.iter().map(String::as_str));
    git::must(repo, &args, DEFAULT_TIMEOUT)?;
    let path = git::canonical(&path);
    write_metadata(&path, &plan)?;
    seed_paseo_config(repo, &path)?;
    Ok(Created {
        branch_name: plan.branch_name,
        worktree_path: path.to_string_lossy().into_owned(),
        comparison_base_ref: plan.base_ref.unwrap_or(plan.base_ref_name),
    })
}

/// A worktree's own git directory: `.git` is a `gitdir:` file in a linked
/// worktree, the directory itself in a main checkout.
fn git_dir(worktree: &Path) -> Result<PathBuf, String> {
    let dot_git = worktree.join(".git");
    if !dot_git.exists() {
        return Err(format!("Not a git repository: {}", worktree.display()));
    }
    if let Ok(text) = std::fs::read_to_string(&dot_git)
        && let Some(target) = text.lines().find_map(|l| l.strip_prefix("gitdir:"))
    {
        return Ok(git::resolve_against(worktree, &git::native(target.trim())));
    }
    Ok(dot_git)
}

/// `<git dir>/paseo/worktree.json`, version 1, as Paseo writes it.
pub fn metadata_path(worktree: &Path) -> Result<PathBuf, String> {
    Ok(git_dir(worktree)?.join("paseo").join("worktree.json"))
}

pub fn read_metadata(worktree: &Path) -> Option<Value> {
    let text = std::fs::read_to_string(metadata_path(worktree).ok()?).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_metadata_value(worktree: &Path, metadata: &Value) -> Result<(), String> {
    let path = metadata_path(worktree)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let temp = path.with_extension(format!("{}.tmp", std::process::id()));
    let text = format!(
        "{}\n",
        serde_json::to_string_pretty(metadata).unwrap_or_default()
    );
    std::fs::write(&temp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&temp, &path).map_err(|e| e.to_string())
}

fn write_metadata(worktree: &Path, plan: &Plan) -> Result<(), String> {
    let mut metadata = json!({
        "version": 1,
        "baseRefName": plan.base_ref_name,
        "changeRequestLookupTarget": {
            "headRef": plan.branch_name, "localBranchName": plan.branch_name,
        },
    });
    if let Some(base_ref) = &plan.base_ref {
        metadata["baseRef"] = base_ref.clone().into();
    }
    write_metadata_value(worktree, &metadata)
}

/// The port a worktree's setup scripts get as `PASEO_WORKTREE_PORT`, kept in
/// its metadata (version 2, `runtime.worktreePort`) once chosen.
pub fn runtime_port(worktree: &Path) -> Result<u16, String> {
    let metadata = read_metadata(worktree);
    if let Some(port) = metadata
        .as_ref()
        .and_then(|m| m["runtime"]["worktreePort"].as_u64())
        .and_then(|p| u16::try_from(p).ok())
    {
        return Ok(port);
    }
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|addr| addr.port())
        .map_err(|err| format!("Failed to acquire available port: {err}"))?;
    if let Some(mut metadata) = metadata {
        metadata["version"] = 2.into();
        metadata["runtime"] = json!({"worktreePort": port});
        write_metadata_value(worktree, &metadata)?;
    }
    Ok(port)
}

/// `seedPaseoConfigFile`: `paseo.json` is copied when the target has none.
pub fn seed_paseo_config(source: &Path, target: &Path) -> Result<(), String> {
    let from = source.join("paseo.json");
    let to = target.join("paseo.json");
    if !from.is_file() || to.exists() {
        return Ok(());
    }
    std::fs::copy(&from, &to)
        .map(|_| ())
        .map_err(|err| format!("Failed to copy paseo.json: {err}"))
}

/// `deletePaseoWorktree`: `git worktree remove --force`, then the folder,
/// retried, then `git worktree prune`. Only a folder under the worktrees
/// root is ever removed.
pub fn delete(repo: Option<&Path>, worktree: &Path, base_root: &Path) -> Result<(), String> {
    let owned = owned_worktree(worktree, base_root)
        .ok_or_else(|| "Refusing to delete non-Paseo worktree".to_string())?;
    let owned_text = owned.to_string_lossy().into_owned();
    if let Some(repo) = repo {
        let _ = git::run(
            repo,
            &["worktree", "remove", &owned_text, "--force"],
            &[],
            DEFAULT_TIMEOUT,
        );
    }
    let mut last = None;
    for delay in [0, 100, 300, 700, 1500] {
        std::thread::sleep(Duration::from_millis(delay));
        if !owned.exists() {
            last = None;
            break;
        }
        last = std::fs::remove_dir_all(&owned).err();
    }
    if let Some(repo) = repo {
        let _ = git::run(repo, &["worktree", "prune"], &[], Duration::from_secs(30));
    }
    match last {
        Some(err) if owned.exists() => Err(format!("Failed to remove worktree directory: {err}")),
        _ => Ok(()),
    }
}

/// `listPaseoWorktrees`: git's worktrees of `cwd`'s repository that live in
/// its folder under the worktrees root.
pub fn list(cwd: &Path, base_root: &Path) -> Result<Vec<(Listed, String)>, String> {
    if git::read(cwd, &["rev-parse", "--git-dir"]).is_none() {
        return Err(format!("Not a git repository: {}", cwd.display()));
    }
    let folder = project_root(cwd, base_root);
    Ok(worktree_list(cwd)
        .into_iter()
        .map(|mut entry| {
            entry.path = git::canonical(Path::new(&entry.path))
                .to_string_lossy()
                .into_owned();
            entry
        })
        .filter(|entry| git::relative_inside(&folder, Path::new(&entry.path)).is_some())
        .map(|entry| {
            let created = created_at(Path::new(&entry.path));
            (entry, created)
        })
        .collect())
}

fn created_at(path: &Path) -> String {
    let at = std::fs::metadata(path)
        .ok()
        .and_then(|m| m.created().or_else(|_| m.modified()).ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as i64);
    crate::clock::iso_from_ms(at)
}

// --- branch reads for clients -------------------------------------------------

/// `branch_suggestions_response.branchDetails` entry.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSuggestion {
    pub name: String,
    pub committer_date: i64,
    pub has_local: bool,
    pub has_remote: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_ahead: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_behind: Option<u64>,
}

/// `normalizeBranchSuggestionName`.
fn suggestion_name(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let name = trimmed
        .strip_prefix("refs/heads/")
        .or_else(|| trimmed.strip_prefix("refs/remotes/"))
        .unwrap_or(trimmed);
    let name = name.strip_prefix("origin/").unwrap_or(name);
    (!name.is_empty() && name != "HEAD" && name != "origin").then(|| name.to_string())
}

fn git_refs(cwd: &Path, prefix: &str) -> Vec<(String, i64, String)> {
    git::read(
        cwd,
        &[
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)%09%(committerdate:unix)%09%(objectname)",
            prefix,
        ],
    )
    .unwrap_or_default()
    .lines()
    .filter_map(|line| {
        let mut parts = line.trim().split('\t');
        let name = parts.next().filter(|n| !n.is_empty())?;
        let date = parts.next().and_then(|d| d.parse().ok()).unwrap_or(0);
        let oid = parts.next().filter(|o| !o.is_empty())?;
        Some((name.to_string(), date, oid.to_string()))
    })
    .collect()
}

/// `listBranchSuggestions`: local branches and `origin`'s, merged by name,
/// matched by substring, prefix matches first, then newest first, with how
/// far a local branch is ahead of and behind `origin`'s.
pub fn branch_suggestions(
    cwd: &Path,
    query: Option<&str>,
    limit: Option<u32>,
) -> Result<Vec<BranchSuggestion>, String> {
    if git::read(cwd, &["rev-parse", "--git-dir"]).is_none() {
        return Err(format!("Not a git repository: {}", cwd.display()));
    }
    let limit = limit.unwrap_or(50).clamp(1, 200) as usize;
    let raw = query.unwrap_or_default().trim().to_lowercase();
    let query = suggestion_name(&raw).unwrap_or(raw);
    struct Meta {
        date: i64,
        local: Option<String>,
        remote: Option<String>,
    }
    let mut order: Vec<String> = Vec::new();
    let mut meta: std::collections::HashMap<String, Meta> = std::collections::HashMap::new();
    for (refs, local) in [
        (git_refs(cwd, "refs/heads"), true),
        (git_refs(cwd, "refs/remotes/origin"), false),
    ] {
        for (name, date, oid) in refs {
            let Some(name) = suggestion_name(&name) else {
                continue;
            };
            let entry = meta.entry(name.clone()).or_insert_with(|| {
                order.push(name.clone());
                Meta {
                    date: 0,
                    local: None,
                    remote: None,
                }
            });
            entry.date = entry.date.max(date);
            if local {
                entry.local = Some(oid);
            } else {
                entry.remote = Some(oid);
            }
        }
    }
    let mut names: Vec<String> = order
        .into_iter()
        .filter(|name| query.is_empty() || name.to_lowercase().contains(&query))
        .collect();
    names.sort_by(|a, b| {
        let prefix = |n: &str| !query.is_empty() && n.to_lowercase().starts_with(&query);
        prefix(b)
            .cmp(&prefix(a))
            .then(meta[b].date.cmp(&meta[a].date))
            .then_with(|| a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b)))
    });
    Ok(names
        .into_iter()
        .take(limit)
        .map(|name| {
            let entry = &meta[&name];
            let mut suggestion = BranchSuggestion {
                name: name.clone(),
                committer_date: entry.date,
                has_local: entry.local.is_some(),
                has_remote: entry.remote.is_some(),
                local_ahead: None,
                local_behind: None,
            };
            if let (Some(local), Some(remote)) = (&entry.local, &entry.remote) {
                if local == remote {
                    (suggestion.local_ahead, suggestion.local_behind) = (Some(0), Some(0));
                } else if let Some(counts) = git::read(
                    cwd,
                    &[
                        "rev-list",
                        "--left-right",
                        "--count",
                        &format!("refs/heads/{name}...refs/remotes/origin/{name}"),
                    ],
                ) {
                    let mut numbers = counts.split_whitespace().map(|n| n.parse::<u64>().ok());
                    if let (Some(Some(ahead)), Some(Some(behind))) =
                        (numbers.next(), numbers.next())
                    {
                        (suggestion.local_ahead, suggestion.local_behind) =
                            (Some(ahead), Some(behind));
                    }
                }
            }
            suggestion
        })
        .collect())
}

/// `resolveBranchCheckout`, for `validate_branch_request`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchResolution {
    Local(String),
    RemoteOnly { name: String, remote_ref: String },
    NotFound,
}

pub fn resolve_branch(cwd: &Path, name: &str) -> Result<BranchResolution, String> {
    if git::read(cwd, &["rev-parse", "--git-dir"]).is_none() {
        return Err(format!("Not a git repository: {}", cwd.display()));
    }
    let Some(name) = suggestion_name(name) else {
        return Ok(BranchResolution::NotFound);
    };
    let verify =
        |reference: &str| git::succeeds(cwd, &["rev-parse", "--verify", "--quiet", reference]);
    if verify(&format!("refs/heads/{name}")) {
        return Ok(BranchResolution::Local(name));
    }
    if verify(&format!("refs/remotes/origin/{name}")) {
        return Ok(BranchResolution::RemoteOnly {
            remote_ref: format!("origin/{name}"),
            name,
        });
    }
    Ok(BranchResolution::NotFound)
}

// --- in-place branch changes (git options without a worktree) -------------------

fn ensure_clean(cwd: &Path) -> Result<(), String> {
    let status = git::read(cwd, &["status", "--porcelain"]).unwrap_or_default();
    if status.trim().is_empty() {
        Ok(())
    } else {
        Err(
            "Working directory has uncommitted changes. Commit or stash before switching branches."
                .into(),
        )
    }
}

/// `checkoutExistingBranch`: switches the checkout to `branch`, tracking
/// `origin`'s when only it has the branch.
pub fn checkout_existing(cwd: &Path, branch: &str) -> Result<(), String> {
    assert_safe_ref(branch, "branch")?;
    let resolution = resolve_branch(cwd, branch)?;
    if resolution == BranchResolution::NotFound {
        return Err(format!("Branch not found: {branch}"));
    }
    ensure_clean(cwd)?;
    match resolution {
        BranchResolution::Local(name) => {
            if git::read(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).as_deref() != Some(&name) {
                git::must(cwd, &["checkout", &name], DEFAULT_TIMEOUT)?;
            }
        }
        BranchResolution::RemoteOnly { name, remote_ref } => {
            git::must(
                cwd,
                &["checkout", "-b", &name, "--track", &remote_ref],
                DEFAULT_TIMEOUT,
            )?;
        }
        BranchResolution::NotFound => {}
    }
    Ok(())
}

/// `createBranchFromBase`: a new branch off `base`, without an upstream.
pub fn create_branch(cwd: &Path, base: &str, new_branch: &str) -> Result<(), String> {
    assert_safe_ref(base, "base branch")?;
    assert_safe_ref(new_branch, "new branch")?;
    if resolve_branch(cwd, base)? == BranchResolution::NotFound {
        return Err(format!("Base branch not found: {base}"));
    }
    if local_branch_exists(cwd, new_branch) {
        return Err(format!("Branch already exists: {new_branch}"));
    }
    ensure_clean(cwd)?;
    git::must(
        cwd,
        &["checkout", "-b", new_branch, "--no-track", base],
        DEFAULT_TIMEOUT,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_follow_paseo() {
        assert_eq!(slugify("Fix the Login bug!"), "fix-the-login-bug");
        assert_eq!(slugify("feature/foo"), "feature-foo");
        assert_eq!(slugify("--x--"), "x");
        let long = slugify(&"word ".repeat(20));
        assert!(
            long.len() <= MAX_SLUG_LENGTH && !long.ends_with('-'),
            "{long}"
        );
        assert!(validate_slug("ok/name-1").is_ok());
        assert!(validate_slug("Bad").is_err());
        assert!(validate_slug("a--b").is_err());
        assert!(assert_safe_ref("origin/main", "base branch").is_ok());
        assert_eq!(
            assert_safe_ref("a..b", "base branch").unwrap_err(),
            "Invalid base branch: a..b"
        );
    }

    #[test]
    fn hashes_are_eight_base36_digits() {
        let hash = short_hash("/home/someone/repo");
        assert_eq!(hash.len(), 8);
        assert!(
            hash.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        );
        assert_eq!(hash, short_hash("/home/someone/repo"));
        assert_ne!(hash, short_hash("/home/someone/other"));
    }

    #[test]
    fn refs_name_their_branch() {
        assert_eq!(branch_name_from_ref("refs/heads/main"), "main");
        assert_eq!(branch_name_from_ref("refs/remotes/upstream/dev"), "dev");
        assert_eq!(branch_name_from_ref("origin/x/y"), "x/y");
        let name = random_name();
        assert!(validate_slug(&name).is_ok(), "{name}");
    }
}
