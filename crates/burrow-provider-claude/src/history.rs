//! Claude Code's own transcripts: `<config>/projects/<cwd>/<session>.jsonl`.
//!
//! Burrow keeps durable timelines of its own, so this is only needed for
//! a session Burrow did not record (one resumed from elsewhere). The
//! mapping follows Paseo's history replay: sidechain, meta and compact-summary
//! lines are skipped; user text becomes `user_message`, assistant blocks
//! become text, reasoning and tool calls, and a `tool_result` completes the
//! call it answers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use burrow_provider::{HistoryItem, RecentSession};
use serde_json::{Value, json};

use crate::tools;

/// `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn config_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        return PathBuf::from(dir);
    }
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".claude")
}

/// Claude Code's project directory name for a working directory.
pub fn encode(cwd: &str) -> String {
    let encoded: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if encoded.chars().count() <= 200 {
        return encoded;
    }
    let mut hash: i32 = 0;
    for unit in cwd.encode_utf16() {
        hash = hash
            .wrapping_shl(5)
            .wrapping_sub(hash)
            .wrapping_add(i32::from(unit as i16) & 0xffff);
    }
    let head: String = encoded.chars().take(200).collect();
    format!("{head}-{}", base36(u64::from(hash.unsigned_abs())))
}

fn base36(mut n: u64) -> String {
    if n == 0 {
        return "0".into();
    }
    let digits = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while n > 0 {
        out.push(digits[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_default()
}

/// Where the transcript is: the encoded working directory first, then any
/// project directory that has the session.
pub fn locate(config: &Path, cwd: &str, session_id: &str) -> Option<PathBuf> {
    let file = format!("{session_id}.jsonl");
    for dir in project_names(cwd) {
        let path = config.join("projects").join(dir).join(&file);
        if path.is_file() {
            return Some(path);
        }
    }
    std::fs::read_dir(config.join("projects"))
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&file))
        .find(|path| path.is_file())
}

/// The names Claude Code may have given a working directory's project
/// directory: for its real path (symlinks resolved), then as given.
fn project_names(cwd: &str) -> [String; 2] {
    let real = std::fs::canonicalize(cwd)
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| cwd.to_string());
    [encode(&real), encode(cwd)]
}

/// How many lines of a transcript are read to find its directory and first
/// prompt.
const HEAD_LINES: usize = 400;
/// How much of the end of a transcript is read for its name and latest
/// prompt. The CLI writes its names again each time it saves, so they are
/// near the end; a very long turn can push the latest prompt out of it.
const TAIL_BYTES: u64 = 512 * 1024;
/// Paseo's cap on a prompt preview, in characters.
const PREVIEW_CHARS: usize = 160;
/// The most transcripts a listing looks into, as Paseo's.
const SCAN_LIMIT: usize = 500;
/// How many transcripts' descriptions are kept before they are let go.
const KEPT: usize = 2000;

/// A transcript's size and time when it was read, and the session it said.
type Reading = (u64, std::time::SystemTime, Option<RecentSession>);

/// What each transcript last said of itself, kept while its size and time
/// stand, so a listing reads only the transcripts written since the one
/// before (the import sheet lists again for each letter searched).
#[derive(Debug, Default)]
pub struct Described(std::sync::Mutex<HashMap<PathBuf, Reading>>);

impl Described {
    fn of(&self, path: &Path, modified: std::time::SystemTime, len: u64) -> Option<RecentSession> {
        let kept = || self.0.lock().unwrap_or_else(|p| p.into_inner());
        let hit = kept()
            .get(path)
            .filter(|(was_len, was_modified, _)| *was_len == len && *was_modified == modified)
            .map(|(_, _, session)| session.clone());
        if let Some(session) = hit {
            return session;
        }
        let session = describe(path, modified);
        let mut kept = kept();
        if kept.len() >= KEPT {
            kept.clear();
        }
        kept.insert(path.to_path_buf(), (len, modified, session.clone()));
        session
    }
}

/// Up to `limit` of the most recently written sessions under
/// `<config>/projects`, newest first (Paseo's `collectRecentClaudeSessions`):
/// only those of `cwd`'s project directory when there is one, which is why
/// the limit applies to that directory alone. The `limit * 3` newest
/// transcripts (at most 500) are looked into, since some say no directory.
/// Sub-agent transcripts (`agent-*.jsonl`) are left out.
pub fn recent(
    config: &Path,
    cwd: Option<&str>,
    limit: usize,
    described: &Described,
) -> Result<Vec<RecentSession>, String> {
    let root = config.join("projects");
    let dirs: Vec<PathBuf> = match cwd {
        Some(cwd) => {
            let mut dirs: Vec<PathBuf> = project_names(cwd).map(|name| root.join(name)).into();
            dirs.dedup();
            dirs
        }
        None => match std::fs::read_dir(&root) {
            Ok(dirs) => (dirs.flatten().map(|d| d.path()))
                .filter(|path| path.is_dir())
                .collect(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(format!("{}: {error}", root.display())),
        },
    };
    let mut files = Vec::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.ends_with(".jsonl") || name.starts_with("agent-") {
                continue;
            }
            if let Ok(meta) = entry.metadata()
                && let Ok(modified) = meta.modified()
            {
                files.push((entry.path(), modified, meta.len()));
            }
        }
    }
    files.sort_by_key(|file| std::cmp::Reverse(file.1));
    let scan = limit.saturating_mul(3).min(SCAN_LIMIT);
    Ok((files.into_iter().take(scan))
        .filter_map(|(path, modified, len)| described.of(&path, modified, len))
        .take(limit)
        .collect())
}

/// What the lines of a transcript say of its session.
#[derive(Default)]
struct Seen {
    cwd: Option<String>,
    custom_title: Option<String>,
    ai_title: Option<String>,
    first_prompt: Option<String>,
    last_prompt: Option<String>,
}

impl Seen {
    /// Takes in one line, by Paseo's rules for a session's description:
    /// sidechains, and user lines that are synthetic (meta lines and tool
    /// results), say nothing. The last name wins; so does the last prompt.
    fn take(&mut self, entry: &Value) {
        let user = entry["type"] == "user";
        if truthy(&entry["isSidechain"])
            || (user
                && (entry["isSynthetic"] == true
                    || entry["isMeta"] == true
                    || truthy(&entry["toolUseResult"])))
        {
            return;
        }
        if self.cwd.is_none() {
            self.cwd = (entry["cwd"].as_str())
                .filter(|cwd| !cwd.is_empty())
                .map(str::to_string);
        }
        match entry["type"].as_str() {
            Some("custom-title") if entry["customTitle"].is_string() => {
                self.custom_title = trimmed(&entry["customTitle"]);
            }
            Some("ai-title") if entry["aiTitle"].is_string() => {
                self.ai_title = trimmed(&entry["aiTitle"]);
            }
            Some("user") => {
                if let Some(prompt) = prompt_text(&entry["message"]) {
                    self.first_prompt.get_or_insert_with(|| prompt.clone());
                    self.last_prompt = Some(prompt);
                }
            }
            _ => {}
        }
    }
}

/// A transcript as a session for the import sheet: its first lines say where
/// it ran and what was asked first, its last ones its name and what was
/// asked last. The session is the file's name, as `claude --resume` takes
/// it. None when the file cannot be read or never says its directory.
fn describe(path: &Path, modified: std::time::SystemTime) -> Option<RecentSession> {
    use std::io::BufRead;
    let id = path.file_stem()?.to_string_lossy().to_string();
    let mut seen = Seen::default();
    let file = std::fs::File::open(path).ok()?;
    for line in std::io::BufReader::new(file).lines().take(HEAD_LINES) {
        let Ok(line) = line else { break };
        if let Ok(entry) = serde_json::from_str::<Value>(&line) {
            seen.take(&entry);
        }
        if seen.cwd.is_some() && seen.first_prompt.is_some() {
            break;
        }
    }
    for line in tail(path).lines() {
        // Only these lines can change what is known, and most are neither.
        if !(line.contains("\"user\"") || line.contains("-title\"")) {
            continue;
        }
        if let Ok(entry) = serde_json::from_str::<Value>(line) {
            seen.take(&entry);
        }
    }
    let first_prompt = seen.first_prompt;
    let title = (seen.custom_title.or(seen.ai_title))
        .or_else(|| first_prompt.clone())
        .unwrap_or_else(|| format!("Claude session {}", id.chars().take(8).collect::<String>()));
    let last_activity_ms = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    Some(RecentSession {
        id,
        cwd: seen.cwd?,
        title: Some(title),
        first_prompt: first_prompt.as_deref().and_then(preview),
        last_prompt: seen.last_prompt.as_deref().and_then(preview),
        last_activity_ms,
    })
}

/// The last [`TAIL_BYTES`] of a file, from its first whole line.
fn tail(path: &Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut file) = std::fs::File::open(path) else {
        return String::new();
    };
    let start = (file.metadata()).map_or(0, |meta| meta.len().saturating_sub(TAIL_BYTES));
    let mut bytes = Vec::new();
    if file.seek(SeekFrom::Start(start)).is_err() || file.read_to_end(&mut bytes).is_err() {
        return String::new();
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if start == 0 {
        return text;
    }
    // The window began somewhere in a line: leave that one out.
    (text.split_once('\n'))
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default()
}

/// A prompt as the import sheet shows it: whitespace collapsed, and cut at
/// [`PREVIEW_CHARS`].
fn preview(text: &str) -> Option<String> {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!one_line.is_empty()).then(|| one_line.chars().take(PREVIEW_CHARS).collect())
}

/// JavaScript's truthiness of a JSON value, which Claude Code's own flags
/// (`isSidechain`, `toolUseResult`) are read by.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(on) => *on,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.),
        Value::String(text) => !text.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// A string value, trimmed, when it is not empty.
fn trimmed(value: &Value) -> Option<String> {
    let text = value.as_str()?.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// What a user line asked, when it is a prompt: text of its `content`, `text`
/// or first block with some, other than the CLI's own placeholders.
fn prompt_text(message: &Value) -> Option<String> {
    let content = &message["content"];
    let text = match content.as_str().or_else(|| message["text"].as_str()) {
        Some(text) => text.trim(),
        None => (content.as_array()?.iter())
            .filter_map(|block| block["text"].as_str())
            .map(str::trim)
            .find(|text| !text.is_empty() && !transcript_noise(text))?,
    };
    if transcript_noise(text) {
        return None;
    }
    command_prompt(text)
}

/// A slash command's lines as the prompt that ran it, `/name args`; any
/// other text as it is. None when the command's name cannot be read.
fn command_prompt(text: &str) -> Option<String> {
    if between(text, "command-message").is_none() {
        return Some(text.to_string());
    }
    let filled = |tag: &str| between(text, tag).filter(|inside| !inside.is_empty());
    let name = filled("command-name").or_else(|| filled("command-message"))?;
    let name = if name.starts_with('/') {
        name.to_string()
    } else {
        format!("/{name}")
    };
    Some(match filled("command-args") {
        Some(args) => format!("{name} {args}"),
        None => name,
    })
}

/// The trimmed text between `<tag>` and `</tag>`.
fn between<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let start = text.find(&open)? + open.len();
    let end = start + text[start..].find(&format!("</{tag}>"))?;
    Some(text[start..end].trim())
}

/// The name the user gave a session with `/rename`: the last usable
/// `custom-title` in its transcript (Paseo's `readClaudeRenamedSessionTitle`).
/// An `ai-title`, which the CLI writes itself, is not one.
pub fn renamed(config: &Path, cwd: &str, session_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(locate(config, cwd, session_id)?).ok()?;
    // From the end, where the CLI writes it again each time it saves.
    (text.lines().rev())
        .filter(|line| line.contains("\"custom-title\""))
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry["type"] == "custom-title" && !truthy(&entry["isSidechain"]))
        .find_map(|entry| trimmed(&entry["customTitle"]))
}

pub fn read(config: &Path, cwd: &str, session_id: &str) -> Vec<HistoryItem> {
    match locate(config, cwd, session_id).and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(text) => parse(&text),
        None => Vec::new(),
    }
}

/// Text the CLI writes into a transcript that no one typed.
fn transcript_noise(text: &str) -> bool {
    let text = text.trim();
    text.is_empty()
        || text.starts_with("[Request interrupted")
        || text == "No response requested."
        || text.starts_with("<local-command-stdout>")
}

/// [`transcript_noise`], and a slash command's name line, which the history
/// shows as the command's own text instead.
fn noise(text: &str) -> bool {
    transcript_noise(text) || text.trim().starts_with("<command-name>")
}

/// Turns transcript lines into timeline items, in order.
pub fn parse(text: &str) -> Vec<HistoryItem> {
    let mut items: Vec<HistoryItem> = Vec::new();
    // Tool calls waiting for their result: id → (index in items, name, input).
    let mut open: HashMap<String, (usize, String, Value)> = HashMap::new();
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry["isSidechain"] == true || entry["isCompactSummary"] == true {
            continue;
        }
        let timestamp = entry["timestamp"].as_str().map(str::to_string);
        let push = |items: &mut Vec<HistoryItem>, item: Value| {
            items.push(HistoryItem {
                timestamp: timestamp.clone(),
                item,
            });
        };
        let uuid = entry["uuid"].as_str().unwrap_or_default().to_string();
        match entry["type"].as_str() {
            Some("user") => {
                let content = &entry["message"]["content"];
                let mut texts = Vec::new();
                match content {
                    Value::String(text) => texts.push(text.clone()),
                    Value::Array(blocks) => {
                        for block in blocks {
                            match block["type"].as_str() {
                                Some("text") => {
                                    texts.push(block["text"].as_str().unwrap_or_default().into())
                                }
                                Some("tool_result") => {
                                    let id = block["tool_use_id"].as_str().unwrap_or_default();
                                    if let Some((at, name, input)) = open.remove(id) {
                                        let output = tools::result_text(&block["content"]);
                                        let failed = block["is_error"].as_bool().unwrap_or(false);
                                        let (status, detail, error) = if failed {
                                            (
                                                "failed",
                                                tools::detail(&name, &input, None),
                                                json!({"message": output}),
                                            )
                                        } else {
                                            (
                                                "completed",
                                                tools::detail(&name, &input, Some(&output)),
                                                Value::Null,
                                            )
                                        };
                                        items[at].item = json!({"type": "tool_call",
                                            "callId": id, "name": name, "detail": detail,
                                            "status": status, "error": error});
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if entry["isMeta"] == true {
                    continue;
                }
                let text = texts.join("\n");
                if !noise(&text) {
                    push(
                        &mut items,
                        json!({"type": "user_message", "text": text, "messageId": uuid}),
                    );
                }
            }
            Some("assistant") => {
                for block in entry["message"]["content"].as_array().into_iter().flatten() {
                    match block["type"].as_str() {
                        Some("text") => {
                            let text = block["text"].as_str().unwrap_or_default();
                            if !text.is_empty() {
                                push(
                                    &mut items,
                                    json!({"type": "assistant_message", "text": text,
                                           "messageId": uuid}),
                                );
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = block["thinking"].as_str().filter(|t| !t.is_empty())
                            {
                                push(&mut items, json!({"type": "reasoning", "text": text}));
                            }
                        }
                        Some("tool_use") => {
                            let id = block["id"].as_str().unwrap_or_default().to_string();
                            let name = block["name"].as_str().unwrap_or_default().to_string();
                            let input = block["input"].clone();
                            push(
                                &mut items,
                                json!({"type": "tool_call", "callId": id, "name": name,
                                       "detail": tools::detail(&name, &input, None),
                                       "status": "running", "error": null}),
                            );
                            if name == "TodoWrite"
                                && let Some(todo) = tools::todo(&input)
                            {
                                push(&mut items, todo);
                            }
                            let at = items
                                .iter()
                                .rposition(|i| i.item["callId"] == id.as_str())
                                .unwrap_or(items.len() - 1);
                            open.insert(id, (at, name, input));
                        }
                        _ => {}
                    }
                }
            }
            Some("system") if entry["subtype"] == "compact_boundary" => {
                push(
                    &mut items,
                    json!({"type": "compaction", "status": "completed"}),
                );
            }
            _ => {}
        }
    }
    // Calls the transcript never finished were cut off.
    for (_, (at, _, _)) in open {
        items[at].item["status"] = "canceled".into();
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recent_sessions_come_from_transcripts_newest_first() {
        let config = std::env::temp_dir().join(format!("claude-recent-{}", std::process::id()));
        let project = config.join("projects").join("-work-app");
        std::fs::create_dir_all(&project).unwrap();
        let older = [
            r#"{"type":"summary","summary":"x"}"#,
            r#"{"type":"user","isMeta":true,"cwd":"/work/app","message":{"content":"<command-name>/clear"}}"#,
            r#"{"type":"user","cwd":"/work/app","message":{"content":[{"type":"text","text":"Fix the parser"}]}}"#,
        ]
        .join("\n");
        std::fs::write(project.join("old-1.jsonl"), older).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let newer = r#"{"type":"user","cwd":"/work/app/src","message":{"content":"Add a test"}}"#;
        std::fs::write(project.join("new-2.jsonl"), newer).unwrap();
        // A sub-agent's transcript, and one that never says where it ran.
        std::fs::write(project.join("agent-3.jsonl"), newer).unwrap();
        std::fs::write(project.join("bare-4.jsonl"), r#"{"type":"summary"}"#).unwrap();

        let cache = Described::default();
        let found = recent(&config, None, 10, &cache).unwrap();
        let ids: Vec<&str> = found.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["new-2", "old-1"]);
        assert_eq!(found[0].cwd, "/work/app/src");
        assert_eq!(found[1].first_prompt.as_deref(), Some("Fix the parser"));
        assert!(found[0].last_activity_ms >= found[1].last_activity_ms);
        assert_eq!(recent(&config, None, 1, &cache).unwrap().len(), 1);
        assert!(
            recent(&config.join("missing"), None, 10, &cache)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&config);
    }

    /// A scratch Claude config directory with one project directory.
    fn config(name: &str) -> PathBuf {
        let config = std::env::temp_dir().join(format!("claude-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&config);
        std::fs::create_dir_all(config.join("projects")).unwrap();
        config
    }

    fn write(config: &Path, project: &str, session: &str, lines: &[String]) -> PathBuf {
        let dir = config.join("projects").join(project);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{session}.jsonl"));
        std::fs::write(&path, lines.join("\n")).unwrap();
        path
    }

    fn user(cwd: &str, content: Value) -> String {
        json!({"type": "user", "cwd": cwd, "message": {"content": content}}).to_string()
    }

    fn described(config: &Path, cwd: Option<&str>) -> Vec<RecentSession> {
        recent(config, cwd, 10, &Described::default()).unwrap()
    }

    #[test]
    fn a_session_is_named_and_quoted_as_the_import_sheet_shows_it() {
        let config = config("describe");
        let long = format!("{} {}", "word".repeat(60), "tail");
        let lines = [
            // Lines that say nothing: a tool result, a meta line, a sidechain.
            json!({"type": "user", "cwd": "/elsewhere", "toolUseResult": {"ok": true},
                   "message": {"content": [{"type": "tool_result", "content": "x"}]}})
            .to_string(),
            json!({"type": "user", "isMeta": true, "message": {"content": "meta"}}).to_string(),
            json!({"type": "user", "isSidechain": true, "message": {"content": "side"}})
                .to_string(),
            user("/work/app", json!("  Fix   the\n parser  ")),
            // A slash command is what the user typed.
            user(
                "/work/app",
                json!(
                    "<command-message>diagnose</command-message>\n<command-name>/diagnose</command-name>\n<command-args>why it fails</command-args>"
                ),
            ),
            user(
                "/work/app",
                json!("<local-command-stdout>ok</local-command-stdout>"),
            ),
            user("/work/app", json!("[Request interrupted by user]")),
            user("/work/app", json!(long)),
            json!({"type": "ai-title", "aiTitle": "Parser fixes"}).to_string(),
        ];
        write(&config, "-work-app", "0123456789abcdef", &lines);

        let found = described(&config, None);
        assert_eq!(found.len(), 1);
        let session = &found[0];
        assert_eq!(session.id, "0123456789abcdef");
        assert_eq!(session.cwd, "/work/app");
        assert_eq!(session.first_prompt.as_deref(), Some("Fix the parser"));
        let last = session.last_prompt.as_deref().unwrap();
        assert_eq!(last.chars().count(), 160, "{last}");
        assert!(last.starts_with("wordword"));
        // The title: a rename, else the summary the CLI wrote, else the first
        // prompt, else the session's id.
        assert_eq!(session.title.as_deref(), Some("Parser fixes"));
        let mut more = lines.to_vec();
        more.push(json!({"type": "custom-title", "customTitle": " Old name "}).to_string());
        more.push(json!({"type": "custom-title", "customTitle": "Parser, again"}).to_string());
        write(&config, "-work-app", "0123456789abcdef", &more);
        assert_eq!(
            described(&config, None)[0].title.as_deref(),
            Some("Parser, again")
        );
        write(&config, "-work-app", "0123456789abcdef", &lines[3..5]);
        assert_eq!(
            described(&config, None)[0].title.as_deref(),
            Some("Fix   the\n parser"),
            "the first prompt, as it was written"
        );
        let command = write(&config, "-work-app", "0123456789abcdef", &lines[4..5]);
        assert_eq!(
            described(&config, None)[0].first_prompt.as_deref(),
            Some("/diagnose why it fails")
        );
        std::fs::write(&command, user("/work/app", json!("   ")) + "\n").unwrap();
        assert_eq!(
            described(&config, None)[0].title.as_deref(),
            Some("Claude session 01234567")
        );
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn the_latest_prompt_and_name_come_from_the_end_of_a_long_transcript() {
        let config = config("tail");
        let filler = json!({"type": "assistant", "message": {"content": [
            {"type": "text", "text": "x".repeat(1000)}]}})
        .to_string();
        let fill = |lines: usize| vec![filler.clone(); lines];
        // Past the window: the first prompt, then a long stretch of work.
        let mut lines = vec![user("/work/app", json!("first prompt"))];
        lines.extend(fill(700));
        lines.push(user("/work/app", json!("a later prompt")));
        lines.extend(fill(100));
        lines.push(user("/work/app", json!("the latest prompt")));
        lines.push(json!({"type": "custom-title", "customTitle": "Renamed"}).to_string());
        write(&config, "-work-app", "long", &lines);
        // No prompt within the window: the first stands in for the latest.
        let mut quiet = vec![user("/work/app", json!("only prompt"))];
        quiet.extend(fill(700));
        write(&config, "-work-app", "quiet", &quiet);

        let found = described(&config, None);
        let long = found.iter().find(|s| s.id == "long").unwrap();
        assert_eq!(long.first_prompt.as_deref(), Some("first prompt"));
        assert_eq!(long.last_prompt.as_deref(), Some("the latest prompt"));
        assert_eq!(long.title.as_deref(), Some("Renamed"));
        let quiet = found.iter().find(|s| s.id == "quiet").unwrap();
        assert_eq!(quiet.last_prompt.as_deref(), Some("only prompt"));
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn a_directory_is_listed_before_the_limit_applies() {
        let config = config("scoped");
        for n in 0..4 {
            let session = format!("other-{n}");
            write(
                &config,
                "-work-other",
                &session,
                &[user("/work/other", json!("x"))],
            );
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
        write(
            &config,
            "-work-app",
            "mine",
            &[user("/work/app", json!("y"))],
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
        for n in 0..4 {
            let session = format!("later-{n}");
            write(
                &config,
                "-work-other",
                &session,
                &[user("/work/other", json!("x"))],
            );
        }
        let cache = Described::default();
        // None of the three newest transcripts overall is in /work/app.
        let all = recent(&config, None, 1, &cache).unwrap();
        assert!(all[0].id.starts_with("later-"), "{all:?}");
        let ids: Vec<String> = (recent(&config, Some("/work/app"), 1, &cache).unwrap())
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["mine"]);
        assert!(
            recent(&config, Some("/work/nowhere"), 10, &cache)
                .unwrap()
                .is_empty()
        );
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn a_transcript_is_read_again_only_when_its_size_or_time_changed() {
        let config = config("cache");
        let path = write(
            &config,
            "-work-app",
            "s",
            &[user("/work/app", json!("one"))],
        );
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        let cache = Described::default();
        let first = recent(&config, None, 10, &cache).unwrap();
        assert_eq!(first[0].first_prompt.as_deref(), Some("one"));
        // Same size, same time: what was read stands.
        std::fs::write(&path, user("/work/app", json!("two"))).unwrap();
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(before).unwrap();
        let same = recent(&config, None, 10, &cache).unwrap();
        assert_eq!(same[0].first_prompt.as_deref(), Some("one"));
        // Longer: read again.
        std::fs::write(&path, user("/work/app", json!("three"))).unwrap();
        file.set_modified(before).unwrap();
        let longer = recent(&config, None, 10, &cache).unwrap();
        assert_eq!(longer[0].first_prompt.as_deref(), Some("three"));
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn a_rename_is_the_last_usable_custom_title() {
        let config = config("renamed");
        let lines = [
            json!({"type": "custom-title", "customTitle": "First name"}).to_string(),
            user("/work/app", json!("hello")),
            json!({"type": "custom-title", "customTitle": "Second name"}).to_string(),
            json!({"type": "ai-title", "aiTitle": "Machine summary"}).to_string(),
            json!({"type": "custom-title", "customTitle": "Side", "isSidechain": true}).to_string(),
            json!({"type": "custom-title", "customTitle": "  "}).to_string(),
        ];
        write(&config, "-work-app", "named", &lines);
        write(&config, "-work-app", "plain", &lines[1..2]);
        // Found under any project directory, as the history is.
        assert_eq!(
            renamed(&config, "/elsewhere", "named").as_deref(),
            Some("Second name")
        );
        assert_eq!(renamed(&config, "/work/app", "plain"), None);
        assert_eq!(renamed(&config, "/work/app", "missing"), None);
        let _ = std::fs::remove_dir_all(&config);
    }

    #[test]
    fn encodes_working_directories_like_claude_code() {
        assert_eq!(encode("/home/me/my.project"), "-home-me-my-project");
        let long = format!("/{}", "a".repeat(300));
        let encoded = encode(&long);
        assert!(encoded.len() > 200 && encoded.starts_with("-aaa"));
    }

    #[test]
    fn parses_a_transcript() {
        let text = [
            r#"{"type":"user","uuid":"u1","timestamp":"t1","message":{"role":"user","content":"list files"}}"#,
            r#"{"type":"assistant","uuid":"a1","timestamp":"t2","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"user","uuid":"u2","timestamp":"t3","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"a\nb","is_error":false}]}}"#,
            r#"{"type":"assistant","uuid":"a2","timestamp":"t4","message":{"content":[{"type":"text","text":"Two files."}]}}"#,
            r#"{"type":"user","isSidechain":true,"message":{"content":"ignored"}}"#,
            r#"{"type":"user","uuid":"u3","message":{"content":"[Request interrupted by user]"}}"#,
        ]
        .join("\n");
        let items = parse(&text);
        let kinds: Vec<&str> = items
            .iter()
            .map(|i| i.item["type"].as_str().unwrap())
            .collect();
        assert_eq!(kinds, ["user_message", "tool_call", "assistant_message"]);
        assert_eq!(items[1].item["status"], "completed");
        assert_eq!(items[1].item["detail"]["output"], "a\nb");
        assert_eq!(items[0].timestamp.as_deref(), Some("t1"));
    }
}
