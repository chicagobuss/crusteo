//! Adding projects and making workspaces: what the user picked, as the
//! daemon's requests, and the one line that says what will be made.
//!
//! Paseo 0.10 to 0.12 take the same requests here. Each step is gated on the
//! `server_info` feature Paseo gates it on, so a daemon without one (such as
//! Burrow, for now) gets the plainer path rather than an error.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Host;

/// `server_info.features` names the flow depends on.
pub mod feature {
    /// `project.add.request`. Paseo's Add Project also wants
    /// [`STABLE_PROJECT_IDENTITY`].
    pub const PROJECT_ADD: &str = "projectAdd";
    pub const STABLE_PROJECT_IDENTITY: &str = "stableProjectIdentity";
    /// More than one workspace per project: worktrees beside the checkout.
    pub const WORKSPACE_MULTIPLICITY: &str = "workspaceMultiplicity";
    /// `workspace.create.request` also takes the first agent.
    pub const CREATION_LIFECYCLE: &str = "creationLifecycle";
    /// `forge.search.request`: pull and merge requests of GitHub, GitLab
    /// and Gitea or Forgejo.
    pub const FORGE_SEARCH: &str = "forgeSearch";
}

/// What a host can do of the flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Support {
    /// Registers a folder as a project. Without it, adding a project makes
    /// a workspace in the folder, which registers it too.
    pub add_project: bool,
    /// Makes worktree workspaces. Without it, only the project's folder.
    pub worktrees: bool,
    /// Makes the workspace and its first agent in one request. Without it,
    /// the agent follows in a request of its own.
    pub agent_in_create: bool,
    /// Lists pull and merge requests to start from.
    pub forge_search: bool,
}

impl Support {
    pub fn for_host(host: &Host) -> Self {
        Self {
            add_project: host.has(feature::PROJECT_ADD)
                && host.has(feature::STABLE_PROJECT_IDENTITY),
            worktrees: host.has(feature::WORKSPACE_MULTIPLICITY),
            agent_in_create: host.has(feature::CREATION_LIFECYCLE),
            forge_search: host.has(feature::FORGE_SEARCH),
        }
    }
}

/// Where a new workspace works.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Isolation {
    /// In the project's own folder.
    Directory,
    /// In a git worktree of its own, on a branch of its own.
    #[default]
    Worktree,
}

/// A pull or merge request a worktree can check out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRequest {
    /// `github`, `gitlab`, `gitea`, …, as the daemon names the forge.
    pub forge: Option<String>,
    pub number: u64,
    pub title: String,
    /// The branch it would merge.
    pub head: String,
    pub base: Option<String>,
    /// The repository it belongs to, on forges that say.
    pub project_path: Option<String>,
}

/// What a worktree starts from.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum StartRef {
    /// The repository's default branch, as the daemon resolves it.
    #[default]
    Default,
    /// A branch to branch off: `origin`'s copy, or the local one.
    Branch { name: String, remote: bool },
    /// A pull or merge request, checked out on its own branch.
    ChangeRequest(ChangeRequest),
}

impl StartRef {
    /// The full ref the daemon branches off, if not the default.
    pub fn ref_name(&self) -> Option<String> {
        match self {
            StartRef::Default => None,
            StartRef::Branch { name, remote: true } => Some(format!("refs/remotes/origin/{name}")),
            StartRef::Branch {
                name,
                remote: false,
            } => Some(format!("refs/heads/{name}")),
            StartRef::ChangeRequest(change) => Some(change.head.clone()),
        }
    }

    /// The ref back from [`Self::ref_name`], for remembered choices.
    pub fn from_ref_name(name: &str) -> Self {
        if let Some(branch) = name.strip_prefix("refs/remotes/origin/") {
            StartRef::Branch {
                name: branch.into(),
                remote: true,
            }
        } else if let Some(branch) = name.strip_prefix("refs/heads/") {
            StartRef::Branch {
                name: branch.into(),
                remote: false,
            }
        } else {
            StartRef::Default
        }
    }

    /// As the form shows it.
    pub fn label(&self) -> String {
        match self {
            StartRef::Default => "default branch".into(),
            StartRef::Branch { name, remote: true } => format!("origin/{name}"),
            StartRef::Branch {
                name,
                remote: false,
            } => name.clone(),
            StartRef::ChangeRequest(change) => format!("#{} {}", change.number, change.head),
        }
    }
}

/// A branch as `branch_suggestions_response` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchRef {
    pub name: String,
    pub local: bool,
    pub remote: bool,
    /// The local branch has commits `origin` does not, or lacks some.
    pub diverged: bool,
}

impl BranchRef {
    /// The starting points it offers, as Paseo's picker lists them:
    /// `origin`'s copy first, and the local one when there is no remote
    /// copy or the two differ.
    pub fn starts(&self) -> Vec<StartRef> {
        let mut starts = Vec::new();
        if self.remote {
            starts.push(StartRef::Branch {
                name: self.name.clone(),
                remote: true,
            });
        }
        if self.local && (!self.remote || self.diverged) {
            starts.push(StartRef::Branch {
                name: self.name.clone(),
                remote: false,
            });
        }
        starts
    }
}

/// The branch a new worktree goes on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Branch {
    /// Named by crusteo, from the prompt. Paseo may rename it once its
    /// first agent says what the work is.
    Auto(String),
    /// Made up, like `brave-otter`, for a worktree without a prompt.
    Generated(String),
    /// A new branch the user named. Kept as typed.
    Named(String),
    /// A branch the repository already has, checked out as it is.
    Existing(String),
}

impl Branch {
    pub fn name(&self) -> &str {
        match self {
            Branch::Auto(name)
            | Branch::Generated(name)
            | Branch::Named(name)
            | Branch::Existing(name) => name,
        }
    }
}

/// Picks the branch: what the user typed, else a name from the prompt, else
/// a made-up one; never one of `taken`, unless the user typed it, which
/// means checking that branch out.
pub fn resolve_branch(typed: &str, prompt: &str, taken: &[String], seed: u64) -> Branch {
    let typed = typed.trim();
    if !typed.is_empty() {
        return if taken.iter().any(|name| name == typed) {
            Branch::Existing(typed.to_string())
        } else {
            Branch::Named(typed.to_string())
        };
    }
    match branch_from_prompt(prompt) {
        Some(name) => Branch::Auto(unique(&name, taken)),
        None => Branch::Generated(unique(&mnemonic(seed), taken)),
    }
}

/// Words a branch name can do without.
const FILLER: &[&str] = &[
    "a", "an", "the", "of", "in", "on", "to", "for", "and", "or", "with", "at", "by", "from",
    "into", "is", "are", "be", "it", "its", "this", "that", "please", "can", "could", "you",
    "would", "should", "we", "i", "me", "my", "our", "so", "then", "just",
];

/// A branch name from a prompt's first words, as Paseo's own slugs are
/// made: lower case, words joined by `-`, at most 40 characters. None when
/// the prompt has no word to name it by.
pub fn branch_from_prompt(prompt: &str) -> Option<String> {
    let line = prompt.lines().find(|line| !line.trim().is_empty())?;
    let words: Vec<String> = line
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_ascii_lowercase)
        .filter(|word| !FILLER.contains(&word.as_str()))
        .take(5)
        .collect();
    let slug = slugify(&words.join("-"), 40);
    (!slug.is_empty()).then_some(slug)
}

/// Paseo's slug rules: lower case ASCII letters and digits, runs of
/// anything else as one `-`, none at either end, cut at a `-` to fit `max`.
pub fn slugify(text: &str, max: usize) -> String {
    let mut slug = String::new();
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let mut slug = slug.trim_end_matches('-').to_string();
    if slug.len() > max {
        let cut = slug[..max].rfind('-').filter(|&at| at > 0).unwrap_or(max);
        slug.truncate(cut);
    }
    slug.trim_end_matches('-').to_string()
}

/// `name`, or `name-2`, `name-3`, … whichever `taken` lacks.
pub fn unique(name: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == name) {
        return name.to_string();
    }
    (2..)
        .map(|n| format!("{name}-{n}"))
        .find(|candidate| !taken.iter().any(|t| t == candidate))
        .unwrap_or_else(|| name.to_string())
}

const ADJECTIVES: &[&str] = &[
    "brave", "calm", "eager", "fond", "keen", "lucky", "mellow", "nimble", "quiet", "rapid",
    "steady", "sunny", "tidy", "witty", "bold", "gentle",
];
const ANIMALS: &[&str] = &[
    "otter", "heron", "lynx", "marten", "badger", "falcon", "gecko", "ibis", "koala", "lemur",
    "newt", "puffin", "quail", "raven", "stoat", "wren",
];

/// A name like Paseo's for a worktree no prompt names: `brave-otter`.
pub fn mnemonic(seed: u64) -> String {
    let a = ADJECTIVES[(seed % ADJECTIVES.len() as u64) as usize];
    let b = ANIMALS[((seed / ADJECTIVES.len() as u64) % ANIMALS.len() as u64) as usize];
    format!("{a}-{b}")
}

/// Why git would refuse `name` as a branch, if it would; a subset of
/// `git check-ref-format` that catches what people type by mistake.
pub fn branch_problem(name: &str) -> Option<&'static str> {
    let name = name.trim();
    if name.is_empty() {
        return None;
    }
    if name.chars().any(|c| c.is_whitespace()) {
        return Some("A branch name has no spaces");
    }
    if name
        .chars()
        .any(|c| c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        return Some("A branch name has none of ~ ^ : ? * [ \\");
    }
    if name.contains("..") || name.contains("@{") || name.contains("//") {
        return Some("A branch name has no .., @{ or //");
    }
    if name.starts_with(['-', '/', '.']) || name.ends_with(['/', '.']) || name.ends_with(".lock") {
        return Some("A branch name cannot start with - / . or end with / . .lock");
    }
    None
}

/// A workspace title from a prompt: its first line, cut at a word to fit.
pub fn prompt_title(prompt: &str) -> Option<String> {
    let line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    const MAX: usize = 80;
    if line.chars().count() <= MAX {
        return Some(line.to_string());
    }
    let cut: String = line.chars().take(MAX).collect();
    let cut = match cut.rfind(' ') {
        Some(at) if at > MAX / 2 => &cut[..at],
        _ => &cut,
    };
    Some(format!("{}…", cut.trim_end()))
}

/// Everything the form decided, ready to become requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub host: crate::HostId,
    /// The host's name, for the summary line.
    pub host_name: String,
    pub project_id: String,
    pub project_name: String,
    /// The project's folder on the host.
    pub root: String,
    pub isolation: Isolation,
    /// For a worktree; ignored when starting from a pull request.
    pub branch: Branch,
    pub start: StartRef,
}

impl Plan {
    /// The `source` of `workspace.create.request`.
    pub fn source(&self) -> Value {
        if self.isolation == Isolation::Directory {
            return json!({ "kind": "directory", "path": self.root, "projectId": self.project_id });
        }
        let mut source =
            json!({ "kind": "worktree", "cwd": self.root, "projectId": self.project_id });
        if let StartRef::ChangeRequest(change) = &self.start {
            source["action"] = json!("checkout");
            source["refName"] = json!(change.head);
            source["worktreeSlug"] = json!(format!("pr-{}", change.number));
            let mut from = json!({ "kind": "change_request", "number": change.number });
            if let Some(forge) = &change.forge {
                from["forge"] = json!(forge);
            }
            if let Some(path) = &change.project_path {
                from["projectPath"] = json!(path);
            }
            source["checkoutSource"] = from;
            // Older daemons only knew GitHub pull requests by number.
            if change.forge.as_deref() == Some("github") {
                source["githubPrNumber"] = json!(change.number);
            }
            return source;
        }
        match &self.branch {
            Branch::Existing(name) => {
                source["action"] = json!("checkout");
                source["refName"] = json!(name);
                source["worktreeSlug"] = json!(slugify(name, 50));
            }
            Branch::Auto(name) | Branch::Generated(name) => {
                // The slug is the placeholder branch Paseo names it by.
                source["action"] = json!("branch-off");
                source["worktreeSlug"] = json!(name);
            }
            Branch::Named(name) => {
                source["action"] = json!("branch-off");
                source["branchName"] = json!(name);
                source["worktreeSlug"] = json!(slugify(name, 50));
            }
        }
        // A starting point is only for a new branch: checking out an
        // existing one names the branch itself, not a ref to start from.
        if !matches!(self.branch, Branch::Existing(_))
            && let Some(base) = self.start.ref_name()
        {
            source["refName"] = json!(base);
        }
        source
    }

    /// A new branch the user named. Paseo renames every new branch after
    /// its first agent's work, given the prompt (even one the user typed,
    /// in 0.12), so such a branch is made without the prompt.
    fn keeps_typed_branch(&self) -> bool {
        self.isolation == Isolation::Worktree
            && !matches!(self.start, StartRef::ChangeRequest(_))
            && matches!(self.branch, Branch::Named(_))
    }

    /// Whether the first agent can go in the workspace request; otherwise
    /// it follows in a request of its own.
    pub fn agent_in_create(&self, support: Support) -> bool {
        support.agent_in_create && !self.keeps_typed_branch()
    }

    /// `workspace.create.request`, with the first agent (see
    /// [`initial_agent`]) when [`Self::agent_in_create`] allows.
    pub fn create_request(
        &self,
        prompt: &str,
        agent: Option<Value>,
        key: &str,
        support: Support,
    ) -> Value {
        let mut message = json!({
            "type": "workspace.create.request",
            "idempotencyKey": key,
            "source": self.source(),
        });
        let prompt = prompt.trim();
        if !prompt.is_empty() {
            let named = self.keeps_typed_branch();
            if !named {
                message["firstAgentContext"] = json!({ "prompt": prompt });
            }
            // A title of its own where Paseo will not make one from the
            // context: without it, or on a host that does not name.
            if named || !support.agent_in_create {
                message["title"] = json!(prompt_title(prompt));
            }
        }
        if let Some(agent) = agent {
            message["agent"] = agent;
        }
        message
    }

    /// What will be made, as runs of text, the names among them marked.
    pub fn summary(&self) -> Vec<(String, bool)> {
        let plain = |text: &str| (text.to_string(), false);
        let name = |text: &str| (text.to_string(), true);
        let mut out = Vec::new();
        if self.isolation == Isolation::Directory {
            out.push(plain("New workspace in "));
            out.push(name(&self.root));
        } else if let StartRef::ChangeRequest(change) = &self.start {
            out.push(plain("New worktree on "));
            out.push(name(&format!("#{}", change.number)));
            out.push(plain(" "));
            out.push(name(&change.head));
        } else {
            match &self.branch {
                Branch::Auto(branch) => {
                    out.push(plain("New worktree "));
                    out.push(name(branch));
                    out.push(plain(" (named for the prompt) from "));
                    out.push(name(&self.start.label()));
                }
                Branch::Generated(branch) => {
                    out.push(plain("New worktree "));
                    out.push(name(branch));
                    out.push(plain(" from "));
                    out.push(name(&self.start.label()));
                }
                Branch::Named(branch) => {
                    out.push(plain("New worktree on new branch "));
                    out.push(name(branch));
                    out.push(plain(" from "));
                    out.push(name(&self.start.label()));
                }
                Branch::Existing(branch) => {
                    out.push(plain("New worktree on existing branch "));
                    out.push(name(branch));
                }
            }
        }
        out.push(plain(" on "));
        out.push(name(&self.host_name));
        out
    }

    /// [`Self::summary`] as one string, for tests and tooltips.
    pub fn summary_text(&self) -> String {
        self.summary().into_iter().map(|(text, _)| text).collect()
    }
}

/// The first agent, as `workspace.create.request.agent` takes it.
pub fn initial_agent(config: Value, prompt: &crate::prompt::Prompt, message_id: &str) -> Value {
    let mut agent = json!({ "config": config, "labels": {} });
    if !prompt.is_empty() {
        agent["initialPrompt"] = json!(prompt.text);
        agent["clientMessageId"] = json!(message_id);
        if !prompt.images.is_empty() {
            agent["images"] = json!(prompt.images);
        }
    }
    agent
}

/// An agent's `config`: `mode` when the provider offers it, else the
/// provider's own default when it offers that one, else its first, so a
/// stale choice cannot fail with "Invalid mode" (Paseo 0.12's fix for its
/// own form).
pub fn agent_config(
    provider: &crate::providers::Provider,
    model: Option<&str>,
    thinking: Option<&str>,
    mode: Option<&str>,
    cwd: &str,
) -> Value {
    let mut config = json!({ "provider": provider.id, "cwd": cwd });
    if let Some(model) = model {
        config["model"] = json!(model);
    }
    if let Some(thinking) = thinking {
        config["thinkingOptionId"] = json!(thinking);
    }
    let offered = |mode: &&String| provider.modes.iter().any(|m| &m.id == *mode);
    let chosen = mode.map(str::to_string).filter(|mode| offered(&mode));
    let mode = chosen
        .as_ref()
        .or_else(|| {
            (provider.default_mode.as_ref())
                .filter(|mode| provider.modes.is_empty() || offered(mode))
        })
        .or_else(|| provider.modes.first().map(|m| &m.id));
    if let Some(mode) = mode {
        config["modeId"] = json!(mode);
    }
    config
}

/// What the user last chose for a project, so the next workspace there
/// starts the same way.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Remembered {
    pub isolation: Isolation,
    /// A branch ref; pull requests are not remembered.
    pub start: Option<String>,
}

impl Remembered {
    pub fn start(&self) -> StartRef {
        self.start
            .as_deref()
            .map(StartRef::from_ref_name)
            .unwrap_or_default()
    }
}

/// What the user last picked for a new agent on a host, so the next one
/// there starts the same way. A choice the host no longer offers falls back
/// to its default when it is read.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AgentChoice {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub mode: Option<String>,
    pub thinking: Option<String>,
}

// ---- replies ---------------------------------------------------------------

/// The reply's error, if it has one.
fn reply_error(payload: &Value) -> Option<String> {
    payload["error"]
        .as_str()
        .filter(|error| !error.is_empty())
        .map(str::to_string)
}

/// Folders from `directory_suggestions_response`.
pub fn parse_directories(payload: &Value) -> Result<Vec<String>, String> {
    if let Some(error) = reply_error(payload) {
        return Err(error);
    }
    let entries: Vec<String> = (payload["entries"].as_array().into_iter().flatten())
        .filter(|entry| entry["kind"] != "file")
        .filter_map(|entry| entry["path"].as_str().map(str::to_string))
        .collect();
    if !entries.is_empty() {
        return Ok(entries);
    }
    Ok((payload["directories"].as_array().into_iter().flatten())
        .filter_map(|path| path.as_str().map(str::to_string))
        .collect())
}

/// Branches from `branch_suggestions_response`, details where the daemon
/// gives them.
pub fn parse_branches(payload: &Value) -> Result<Vec<BranchRef>, String> {
    if let Some(error) = reply_error(payload) {
        return Err(error);
    }
    let details: Vec<BranchRef> = (payload["branchDetails"].as_array().into_iter().flatten())
        .filter_map(|detail| {
            let ahead = detail["localAhead"].as_u64().unwrap_or(0);
            let behind = detail["localBehind"].as_u64().unwrap_or(0);
            Some(BranchRef {
                name: detail["name"].as_str()?.to_string(),
                local: detail["hasLocal"].as_bool().unwrap_or(true),
                remote: detail["hasRemote"].as_bool().unwrap_or(false),
                diverged: ahead > 0 || behind > 0,
            })
        })
        .collect();
    if !details.is_empty() {
        return Ok(details);
    }
    Ok((payload["branches"].as_array().into_iter().flatten())
        .filter_map(|name| {
            Some(BranchRef {
                name: name.as_str()?.to_string(),
                local: true,
                remote: false,
                diverged: false,
            })
        })
        .collect())
}

/// Pull and merge requests from `forge.search.response`, or why there are
/// none to show: the forge's CLI is missing or signed out.
pub fn parse_change_requests(payload: &Value) -> Result<Vec<ChangeRequest>, String> {
    if let Some(error) = reply_error(payload) {
        return Err(error);
    }
    match payload["authState"].as_str() {
        None | Some("authenticated") => {}
        Some("unauthenticated") => {
            return Err("Sign in to the forge's CLI on the host to list pull requests".into());
        }
        Some("cli_missing") => {
            return Err("The host has no forge CLI (gh, glab or tea) to list pull requests".into());
        }
        // No remote: nothing to list, and nothing to say.
        Some(_) => return Ok(Vec::new()),
    }
    Ok((payload["items"].as_array().into_iter().flatten())
        .filter(|item| {
            matches!(
                item["kind"].as_str(),
                Some("change_request" | "github-pr" | "pr")
            )
        })
        .filter_map(|item| {
            let head = item["headRefName"]
                .as_str()
                .filter(|head| !head.is_empty())?;
            Some(ChangeRequest {
                forge: item["forge"].as_str().map(str::to_string),
                number: item["number"].as_u64()?,
                title: item["title"].as_str().unwrap_or_default().to_string(),
                head: head.to_string(),
                base: item["baseRefName"].as_str().map(str::to_string),
                project_path: item["projectPath"].as_str().map(str::to_string),
            })
        })
        .collect())
}

/// Whether a failed request means the daemon has no such request at all.
pub fn unsupported(error: &str) -> bool {
    error.contains("unknown_schema") || error.contains("Unknown message type")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(isolation: Isolation, branch: Branch, start: StartRef) -> Plan {
        Plan {
            host: crate::HostId::new("hst_1"),
            host_name: "devbox".into(),
            project_id: "prj_1".into(),
            project_name: "app".into(),
            root: "/home/dev/code/app".into(),
            isolation,
            branch,
            start,
        }
    }

    fn change() -> ChangeRequest {
        ChangeRequest {
            forge: Some("github".into()),
            number: 42,
            title: "Fix login".into(),
            head: "fix/login".into(),
            base: Some("main".into()),
            project_path: None,
        }
    }

    #[test]
    fn branch_names_come_from_the_prompts_first_words() {
        assert_eq!(
            branch_from_prompt("Fix the flaky login retry in the auth service, then add tests")
                .as_deref(),
            Some("fix-flaky-login-retry-auth")
        );
        assert_eq!(
            branch_from_prompt("\n\n  Bump deps\nmore").as_deref(),
            Some("bump-deps")
        );
        assert_eq!(branch_from_prompt("日本語 の レビュー"), None);
        assert_eq!(branch_from_prompt(""), None);
        let long = branch_from_prompt("Investigate latencyspikeinpaymentsapiendpoints thoroughly")
            .unwrap();
        assert!(long.len() <= 40, "{long}");
        assert!(!long.ends_with('-'));
    }

    #[test]
    fn slugs_follow_paseos_rules() {
        assert_eq!(slugify("  Feat/New Thing!! ", 50), "feat-new-thing");
        assert_eq!(slugify("a-b-c-d-e-f", 4), "a-b");
        assert_eq!(slugify("abcdefgh", 4), "abcd");
        assert_eq!(slugify("---", 10), "");
    }

    #[test]
    fn a_typed_branch_wins_and_an_existing_one_is_checked_out() {
        let taken = vec!["main".to_string(), "fix-login".to_string()];
        assert_eq!(
            resolve_branch(" feat/x ", "Fix login", &taken, 0),
            Branch::Named("feat/x".into())
        );
        assert_eq!(
            resolve_branch("main", "", &taken, 0),
            Branch::Existing("main".into())
        );
        assert_eq!(
            resolve_branch("", "Fix login", &taken, 0),
            Branch::Auto("fix-login-2".into())
        );
        assert_eq!(
            resolve_branch("", "", &[], 0),
            Branch::Generated(mnemonic(0))
        );
        assert_ne!(mnemonic(1), mnemonic(2));
    }

    #[test]
    fn branch_problems_are_caught_before_git_sees_them() {
        assert_eq!(branch_problem("feat/x"), None);
        assert_eq!(branch_problem(""), None);
        assert!(branch_problem("my branch").is_some());
        assert!(branch_problem("a..b").is_some());
        assert!(branch_problem("-x").is_some());
        assert!(branch_problem("x.lock").is_some());
        assert!(branch_problem("what?").is_some());
    }

    #[test]
    fn worktree_sources_say_how_to_branch() {
        let auto = plan(
            Isolation::Worktree,
            Branch::Auto("fix-login".into()),
            StartRef::Branch {
                name: "main".into(),
                remote: true,
            },
        );
        let source = auto.source();
        assert_eq!(source["kind"], "worktree");
        assert_eq!(source["action"], "branch-off");
        assert_eq!(source["worktreeSlug"], "fix-login");
        assert_eq!(source["refName"], "refs/remotes/origin/main");
        assert!(source.get("branchName").is_none());

        let named = plan(
            Isolation::Worktree,
            Branch::Named("feat/New-UI".into()),
            StartRef::Default,
        );
        let source = named.source();
        assert_eq!(source["branchName"], "feat/New-UI");
        assert_eq!(source["worktreeSlug"], "feat-new-ui");
        assert!(source.get("refName").is_none());

        let existing = plan(
            Isolation::Worktree,
            Branch::Existing("release".into()),
            StartRef::Default,
        );
        let source = existing.source();
        assert_eq!(source["action"], "checkout");
        assert_eq!(source["refName"], "release");
        // A remembered starting point does not replace the branch checked out.
        let existing = plan(
            Isolation::Worktree,
            Branch::Existing("develop".into()),
            StartRef::Branch {
                name: "develop".into(),
                remote: false,
            },
        );
        assert_eq!(existing.source()["refName"], "develop");

        let pr = plan(
            Isolation::Worktree,
            Branch::Auto("ignored".into()),
            StartRef::ChangeRequest(change()),
        );
        let source = pr.source();
        assert_eq!(source["action"], "checkout");
        assert_eq!(source["refName"], "fix/login");
        assert_eq!(source["checkoutSource"]["number"], 42);
        assert_eq!(source["checkoutSource"]["forge"], "github");
        assert_eq!(source["githubPrNumber"], 42);

        let dir = plan(
            Isolation::Directory,
            Branch::Auto("x".into()),
            StartRef::Default,
        );
        assert_eq!(
            dir.source(),
            json!({"kind": "directory", "path": "/home/dev/code/app", "projectId": "prj_1"})
        );
    }

    #[test]
    fn a_named_branch_is_made_without_the_agent_so_paseo_keeps_it() {
        let support = Support {
            agent_in_create: true,
            ..Default::default()
        };
        let named = plan(
            Isolation::Worktree,
            Branch::Named("feat/x".into()),
            StartRef::Default,
        );
        assert!(!named.agent_in_create(support));
        let request = named.create_request("Fix login\nand more", None, "k1", support);
        assert!(request.get("firstAgentContext").is_none());
        assert_eq!(request["title"], "Fix login");
        assert_eq!(request["idempotencyKey"], "k1");

        let auto = plan(
            Isolation::Worktree,
            Branch::Auto("fix-login".into()),
            StartRef::Default,
        );
        assert!(auto.agent_in_create(support));
        assert!(!auto.agent_in_create(Support::default()));
        let request = auto.create_request("Fix login", Some(json!({"config": {}})), "k2", support);
        assert_eq!(request["firstAgentContext"]["prompt"], "Fix login");
        assert!(request["agent"].is_object());
        // Paseo titles it from the context.
        assert!(request.get("title").is_none());
        // A host that does not gets the title too.
        let request = auto.create_request("Fix login", None, "k4", Support::default());
        assert_eq!(request["title"], "Fix login");

        // Nothing to name it by: no context at all.
        let request = auto.create_request("  ", None, "k3", support);
        assert!(request.get("firstAgentContext").is_none());
        assert!(request.get("title").is_none());
    }

    #[test]
    fn the_summary_says_what_will_be_made_and_where() {
        let auto = plan(
            Isolation::Worktree,
            Branch::Auto("fix-login".into()),
            StartRef::Branch {
                name: "main".into(),
                remote: true,
            },
        );
        assert_eq!(
            auto.summary_text(),
            "New worktree fix-login (named for the prompt) from origin/main on devbox"
        );
        let named = plan(
            Isolation::Worktree,
            Branch::Named("feat/x".into()),
            StartRef::Default,
        );
        assert_eq!(
            named.summary_text(),
            "New worktree on new branch feat/x from default branch on devbox"
        );
        let pr = plan(
            Isolation::Worktree,
            Branch::Auto("x".into()),
            StartRef::ChangeRequest(change()),
        );
        assert_eq!(pr.summary_text(), "New worktree on #42 fix/login on devbox");
        let dir = plan(
            Isolation::Directory,
            Branch::Auto("x".into()),
            StartRef::Default,
        );
        assert_eq!(
            dir.summary_text(),
            "New workspace in /home/dev/code/app on devbox"
        );
        assert!(
            dir.summary()
                .iter()
                .any(|(text, name)| *name && text == "devbox")
        );
    }

    #[test]
    fn support_follows_server_features() {
        let mut host = Host::default();
        assert_eq!(Support::for_host(&host), Support::default());
        host.server = paseo_client::ServerInfo::from_status(&json!({"type": "status",
            "payload": {"status": "server_info", "version": "0.11.2", "features": {
                "projectAdd": true, "stableProjectIdentity": true,
                "workspaceMultiplicity": true, "creationLifecycle": true, "forgeSearch": true}}}));
        assert_eq!(
            Support::for_host(&host),
            Support {
                add_project: true,
                worktrees: true,
                agent_in_create: true,
                forge_search: true
            }
        );
    }

    #[test]
    fn replies_parse_in_every_shape_paseo_sends() {
        let dirs = json!({"directories": ["/a"], "entries": [
            {"path": "/home/dev/code", "kind": "directory"}, {"path": "/x.txt", "kind": "file"}],
            "error": null});
        assert_eq!(parse_directories(&dirs).unwrap(), ["/home/dev/code"]);
        assert_eq!(
            parse_directories(&json!({"directories": ["/a"]})).unwrap(),
            ["/a"]
        );
        assert_eq!(
            parse_directories(&json!({"error": "boom"})).unwrap_err(),
            "boom"
        );

        let branches = parse_branches(&json!({"branches": ["main", "dev"], "branchDetails": [
            {"name": "main", "hasLocal": true, "hasRemote": true, "localAhead": 0, "localBehind": 2},
            {"name": "dev", "hasLocal": true, "hasRemote": false}]}))
        .unwrap();
        assert_eq!(branches[0].starts().len(), 2);
        assert_eq!(branches[0].starts()[0].label(), "origin/main");
        assert_eq!(
            branches[1].starts(),
            [StartRef::Branch {
                name: "dev".into(),
                remote: false
            }]
        );
        let plain = parse_branches(&json!({"branches": ["main"]})).unwrap();
        assert!(plain[0].local && !plain[0].remote);

        let prs = parse_change_requests(&json!({"authState": "authenticated", "items": [
            {"kind": "change_request", "forge": "gitlab", "number": 7, "title": "MR",
             "headRefName": "feature", "projectPath": "group/app"},
            {"kind": "issue", "number": 8, "title": "Bug"},
            {"kind": "change_request", "number": 9, "title": "no head"}]}))
        .unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].project_path.as_deref(), Some("group/app"));
        assert!(parse_change_requests(&json!({"authState": "cli_missing", "items": []})).is_err());
        assert!(
            parse_change_requests(&json!({"authState": "no_remote", "items": []}))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn remembered_choices_round_trip() {
        let start = StartRef::Branch {
            name: "release/1.2".into(),
            remote: true,
        };
        let remembered = Remembered {
            isolation: Isolation::Directory,
            start: start.ref_name(),
        };
        let back: Remembered =
            serde_json::from_value(serde_json::to_value(&remembered).unwrap()).unwrap();
        assert_eq!(back.start(), start);
        assert_eq!(back.isolation, Isolation::Directory);
        let old: Remembered = serde_json::from_str("{}").unwrap();
        assert_eq!(old.isolation, Isolation::Worktree);
        assert_eq!(old.start(), StartRef::Default);
    }

    #[test]
    fn agent_config_never_sends_a_mode_the_provider_lacks() {
        use crate::providers::{Choice, Provider};
        let mut provider = Provider {
            id: "claude".into(),
            label: "Claude".into(),
            status: "ready".into(),
            enabled: true,
            default_mode: Some("gone".into()),
            modes: vec![Choice {
                id: "auto".into(),
                label: "Auto".into(),
            }],
            models: vec![],
        };
        let config = agent_config(&provider, Some("opus"), None, None, "/w");
        assert_eq!(config["modeId"], "auto");
        assert_eq!(config["model"], "opus");
        // A mode chosen is kept when offered, and dropped when not.
        provider.modes.push(Choice {
            id: "plan".into(),
            label: "Plan".into(),
        });
        let chosen = agent_config(&provider, None, None, Some("plan"), "/w");
        assert_eq!(chosen["modeId"], "plan");
        let stale = agent_config(&provider, None, None, Some("gone"), "/w");
        assert_eq!(stale["modeId"], "auto");
        provider.modes.clear();
        provider.default_mode = None;
        assert!(
            agent_config(&provider, None, None, None, "/w")
                .get("modeId")
                .is_none()
        );
    }
}
