//! Codex thread items as timeline items: Paseo's `codex/tool-call-mapper.ts`,
//! `tool-call-detail-parser.ts` and the item half of `threadItemToTimeline`.
//!
//! Paseo validates each item with zod before mapping it, so an item whose
//! fields have unexpected types maps to nothing. The `field` helpers keep
//! that: a present field of the wrong type fails the whole item.

use serde_json::{Map, Value, json};

/// Paseo's `truncateDiffText` limit, in UTF-16 units as JavaScript counts.
const DIFF_LIMIT: usize = 12_000;

/// The item's own type, with the PascalCase spellings of older Codex builds
/// normalized.
pub fn item_type(item: &Value) -> Option<String> {
    let raw = item["type"].as_str()?;
    let normalized = match raw {
        "UserMessage" => "userMessage",
        "AgentMessage" => "agentMessage",
        "Reasoning" => "reasoning",
        "Plan" => "plan",
        "CommandExecution" => "commandExecution",
        "FileChange" => "fileChange",
        "McpToolCall" => "mcpToolCall",
        "WebSearch" => "webSearch",
        "CollabAgentToolCall" => "collabAgentToolCall",
        "SubAgentActivity" => "subAgentActivity",
        "ImageView" => "imageView",
        "ImageGeneration" => "imageGeneration",
        other => other,
    };
    Some(normalized.to_string())
}

fn non_empty(value: &Value) -> Option<&str> {
    value.as_str().filter(|s| !s.is_empty())
}

/// zod's `z.string().optional()`: absent is fine, any other type fails.
fn opt_string<'a>(item: &'a Value, key: &str) -> Result<Option<&'a str>, ()> {
    match item.get(key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(()),
    }
}

/// `z.string().nullable().optional()`.
fn nullable_string<'a>(item: &'a Value, key: &str) -> Result<Option<&'a str>, ()> {
    match item.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(()),
    }
}

/// A present field that is not JSON `null` (JavaScript's `?? null`).
fn present(item: &Value, key: &str) -> Value {
    match item.get(key) {
        Some(Value::Null) | None => Value::Null,
        Some(value) => value.clone(),
    }
}

/// `normalizeToolCallStatus`.
pub fn status(raw: Option<&str>, error: &Value, output: &Value) -> &'static str {
    if !error.is_null() {
        return "failed";
    }
    if let Some(raw) = raw {
        let normalized = raw.trim().to_lowercase();
        if !normalized.is_empty() {
            return match normalized.as_str() {
                "failed" | "failure" | "error" | "errored" | "rejected" | "denied" | "declined" => {
                    "failed"
                }
                "canceled" | "cancelled" | "interrupted" | "aborted" => "canceled",
                "completed" | "complete" | "done" | "success" | "succeeded" => "completed",
                _ => "running",
            };
        }
    }
    if output.is_null() {
        "running"
    } else {
        "completed"
    }
}

/// `truncateDiffText`.
pub fn truncate_diff(text: &str) -> String {
    let units = text.encode_utf16().count();
    if units <= DIFF_LIMIT {
        return text.to_string();
    }
    let mut kept = String::new();
    let mut count = 0;
    for c in text.chars() {
        let width = c.len_utf16();
        if count + width > DIFF_LIMIT {
            break;
        }
        count += width;
        kept.push(c);
    }
    format!("{kept}\n...[truncated {} chars]", units - DIFF_LIMIT)
}

/// `stripCwdPrefix`: a path inside `cwd` relative to it.
pub fn strip_cwd(path: &str, cwd: Option<&str>) -> String {
    let Some(cwd) = cwd.filter(|c| !c.is_empty()) else {
        return path.to_string();
    };
    if path.is_empty() {
        return path.to_string();
    }
    let cwd = cwd.replace('\\', "/");
    let cwd = cwd.trim_end_matches('/');
    let normalized = path.replace('\\', "/");
    if let Some(rest) = normalized.strip_prefix(&format!("{cwd}/")) {
        return rest.to_string();
    }
    if normalized == cwd {
        return ".".into();
    }
    path.to_string()
}

/// `stripMatchingEdgeQuotes`.
fn strip_quotes(value: &str) -> &str {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// The script of `[/path/]bash -lc <script>` (also `zsh`, `sh`, `-c`).
/// `any_dir`: any directory prefix (thread items), else only `/bin/`
/// (Paseo's notification path).
fn unwrap_unix_shell(command: &str, any_dir: bool) -> Option<&str> {
    let mut rest = command;
    if let Some(slash) = rest.rfind('/') {
        let (dir, name) = rest.split_at(slash + 1);
        let dir_ok = if any_dir {
            dir.starts_with('/') && !dir.chars().any(char::is_whitespace) && !dir.contains("//")
        } else {
            dir == "/bin/"
        };
        if !dir_ok {
            return None;
        }
        rest = name;
    }
    let name_end = rest.find(char::is_whitespace)?;
    if !matches!(&rest[..name_end], "zsh" | "bash" | "sh") {
        return None;
    }
    let rest = rest[name_end..].trim_start();
    let flag_end = rest.find(char::is_whitespace)?;
    if !matches!(&rest[..flag_end], "-lc" | "-c") {
        return None;
    }
    let script = rest[flag_end..].trim_start();
    (!script.is_empty()).then_some(script)
}

/// The script of a `pwsh -Command <script>` / `cmd /c <script>` wrapper.
fn unwrap_windows_shell(command: &str) -> Option<&str> {
    let mut words = command.split_whitespace();
    let first = words.next()?;
    if !is_windows_shell(first) {
        return None;
    }
    let lower = command.to_lowercase();
    for flag in [" -command ", " -c ", " /c "] {
        if let Some(at) = lower.find(flag) {
            let script = command[at + flag.len()..].trim();
            return (!script.is_empty()).then_some(script);
        }
    }
    None
}

fn is_windows_shell(word: &str) -> bool {
    let word = word.trim_matches(['"', '\'']);
    let name = word
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(word)
        .to_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    matches!(name, "pwsh" | "powershell" | "cmd")
}

/// `normalizeCommandExecutionCommand`: the command a thread item shows,
/// without its shell wrapper.
pub fn display_command(value: &Value) -> Option<String> {
    match value {
        Value::String(command) => {
            let trimmed = command.trim();
            let unwrapped = unwrap_unix_shell(trimmed, true)
                .or_else(|| unwrap_windows_shell(trimmed))
                .map(|s| strip_quotes(s.trim()))
                .unwrap_or(trimmed);
            (!unwrapped.is_empty()).then(|| unwrapped.to_string())
        }
        Value::Array(parts) => {
            let parts: Vec<&str> = parts
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect();
            if parts.is_empty() {
                return None;
            }
            if parts.len() >= 3 && matches!(parts[1], "-lc" | "-c") {
                return Some(parts[2].to_string());
            }
            if parts.len() >= 3
                && is_windows_shell(parts[0])
                && matches!(parts[1].to_lowercase().as_str(), "-command" | "-c" | "/c")
            {
                let joined = parts[2..].join(" ");
                let joined = joined.trim();
                return (!joined.is_empty()).then(|| strip_quotes(joined).to_string());
            }
            Some(parts.join(" "))
        }
        _ => None,
    }
}

/// `normalizeCodexCommandValue`, the approval request's version: a string
/// loses only a `[/bin/]bash -lc` wrapper; an array stays an array unless
/// it is one.
pub fn approval_command(value: &Value) -> Option<Value> {
    match value {
        Value::String(command) => {
            let trimmed = command.trim();
            if trimmed.is_empty() {
                return None;
            }
            let Some(script) = unwrap_unix_shell(trimmed, false).map(str::trim) else {
                return Some(trimmed.into());
            };
            if script.is_empty() {
                return Some(trimmed.into());
            }
            Some(strip_quotes(script).into())
        }
        Value::Array(parts) => {
            let parts: Vec<&str> = parts
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .collect();
            if parts.is_empty() {
                return None;
            }
            if parts.len() >= 3 && matches!(parts[1], "-lc" | "-c") {
                return Some(parts[2].into());
            }
            Some(parts.into())
        }
        _ => None,
    }
}

/// `extractCodexShellOutput`: the output of a unified-exec envelope
/// (`Chunk ID: ... Wall time: ... Process exited with code ...`).
pub fn shell_output(text: Option<&str>) -> Option<String> {
    let text = text.filter(|t| !t.is_empty())?;
    let normalized = text.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let header = |line: &str| {
        let line = line.trim().to_lowercase();
        [
            "chunk id:",
            "wall time:",
            "process exited with code",
            "original token count:",
        ]
        .iter()
        .any(|prefix| line.starts_with(prefix))
    };
    let first = lines[0].trim().to_lowercase();
    let window: Vec<String> = lines
        .iter()
        .take(8)
        .map(|l| l.trim().to_lowercase())
        .collect();
    let envelope = first.starts_with("chunk id:")
        && window.iter().any(|l| l.starts_with("wall time:"))
        && window
            .iter()
            .any(|l| l.starts_with("process exited with code"));
    if !envelope {
        return Some(text.to_string());
    }
    let body = match lines.iter().position(|l| l.trim() == "Output:") {
        Some(at) => lines[at + 1..].join("\n"),
        None => {
            let first_body = (1..lines.len()).find(|&i| !header(lines[i]))?;
            lines[first_body..].join("\n")
        }
    };
    (!body.is_empty()).then_some(body)
}

/// `ToolShellInputSchema`: `{command, cwd}` from `command`/`cmd`, `cwd`/`directory`.
fn shell_input(input: &Value) -> Result<(Option<String>, Option<String>), ()> {
    if input.is_null() {
        return Ok((None, None));
    }
    let raw = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .ok_or(())?;
    let command = match raw {
        Value::String(s) => Some(s.clone()).filter(|s| !s.is_empty()),
        Value::Array(tokens) => {
            if tokens.iter().any(|t| !t.is_string()) {
                return Err(());
            }
            let joined = tokens
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            Some(joined).filter(|s| !s.is_empty())
        }
        _ => return Err(()),
    };
    let cwd = non_empty(&input["cwd"])
        .or_else(|| non_empty(&input["directory"]))
        .map(str::to_string);
    Ok((command, cwd))
}

/// `ToolShellOutputSchema`: `{command, output, exitCode}`.
fn shell_output_fields(output: &Value) -> Result<(Option<String>, Option<String>, Value), ()> {
    match output {
        Value::Null => Ok((None, None, Value::Null)),
        Value::String(text) => Ok((None, shell_output(Some(text)), Value::Null)),
        Value::Object(_) => {
            let first = [
                &output["output"],
                &output["text"],
                &output["content"],
                &output["aggregated_output"],
                &output["aggregatedOutput"],
                &output["structuredContent"]["output"],
                &output["structuredContent"]["text"],
                &output["structuredContent"]["content"],
                &output["structured_content"]["output"],
                &output["structured_content"]["text"],
                &output["structured_content"]["content"],
                &output["result"]["output"],
                &output["result"]["text"],
                &output["result"]["content"],
            ]
            .into_iter()
            .find_map(|v| non_empty(v));
            // JavaScript's `??` skips `null` as well as a missing field.
            let exit = [
                &output["exitCode"],
                &output["exit_code"],
                &output["metadata"]["exitCode"],
                &output["metadata"]["exit_code"],
            ]
            .into_iter()
            .find(|v| !v.is_null())
            .cloned()
            .unwrap_or(Value::Null);
            let command = non_empty(&output["command"])
                .or_else(|| non_empty(&output["result"]["command"]))
                .map(str::to_string);
            Ok((command, shell_output(first), exit))
        }
        _ => Err(()),
    }
}

fn shell_detail(input: &Value, output: &Value) -> Option<Value> {
    let (input_command, cwd) = shell_input(input).ok()?;
    let (output_command, text, exit) = shell_output_fields(output).ok()?;
    let command = input_command.or(output_command)?;
    let mut detail = json!({"type": "shell", "command": command});
    if let Some(cwd) = cwd {
        detail["cwd"] = cwd.into();
    }
    if let Some(text) = text.filter(|t| !t.is_empty()) {
        detail["output"] = text.into();
    }
    if !exit.is_null() {
        detail["exitCode"] = exit;
    }
    Some(detail)
}

/// The first of `path`, `file_path`, `filePath` that is a string.
fn path_field(value: &Value) -> Option<&str> {
    ["path", "file_path", "filePath"]
        .iter()
        .find_map(|key| value[*key].as_str())
}

/// `ToolEditTextSchema` accepts these keys only as strings.
const EDIT_TEXT_KEYS: &[&str] = &[
    "old_string",
    "old_str",
    "oldContent",
    "old_content",
    "new_string",
    "new_str",
    "newContent",
    "new_content",
    "content",
    "patch",
    "diff",
    "unified_diff",
    "unifiedDiff",
];

fn edit_text_ok(value: &Value) -> bool {
    value.is_object()
        && EDIT_TEXT_KEYS
            .iter()
            .all(|k| value.get(*k).is_none_or(Value::is_string))
}

fn first_text<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| non_empty(&value[*k]))
}

struct Edit {
    path: Option<String>,
    old: Option<String>,
    new: Option<String>,
    diff: Option<String>,
}

fn edit_input(input: &Value) -> Result<Option<Edit>, ()> {
    if input.is_null() {
        return Ok(None);
    }
    let path = path_field(input).ok_or(())?;
    if !edit_text_ok(input) {
        return Err(());
    }
    Ok(Some(Edit {
        path: Some(path.to_string()),
        old: first_text(
            input,
            &["old_string", "old_str", "oldContent", "old_content"],
        )
        .map(str::to_string),
        new: first_text(
            input,
            &[
                "new_string",
                "new_str",
                "newContent",
                "new_content",
                "content",
            ],
        )
        .map(str::to_string),
        diff: first_text(input, &["patch", "diff", "unified_diff", "unifiedDiff"])
            .map(truncate_diff),
    }))
}

/// `CodexLooseEditOutputSchema`: anything that does not parse is `None`.
fn edit_output(output: &Value) -> Option<Edit> {
    if !output.is_object() {
        return None;
    }
    let diff_of = |v: &Value| {
        first_text(v, &["patch", "diff", "unified_diff", "unifiedDiff"]).map(truncate_diff)
    };
    if let Some(path) = path_field(output)
        && edit_text_ok(output)
    {
        return Some(Edit {
            path: Some(path.to_string()),
            old: None,
            new: first_text(output, &["newContent", "new_content", "content"]).map(str::to_string),
            diff: diff_of(output),
        });
    }
    if let Some(files) = output["files"].as_array().filter(|f| !f.is_empty()) {
        let parsed: Option<Vec<(&str, Option<String>)>> = files
            .iter()
            .map(|file| {
                let path = path_field(file)?;
                let ok = ["patch", "diff", "unified_diff", "unifiedDiff"]
                    .iter()
                    .all(|k| file.get(*k).is_none_or(Value::is_string));
                ok.then(|| (path, diff_of(file)))
            })
            .collect();
        if let Some(parsed) = parsed {
            let (path, diff) = parsed[0].clone();
            return Some(Edit {
                path: Some(path.to_string()),
                old: None,
                new: None,
                diff,
            });
        }
    }
    edit_text_ok(output).then(|| Edit {
        path: None,
        old: None,
        new: first_text(output, &["newContent", "new_content", "content"]).map(str::to_string),
        diff: diff_of(output),
    })
}

fn edit_detail(input: &Value, output: &Value, cwd: Option<&str>) -> Option<Value> {
    let input = edit_input(input).ok()?;
    let output = edit_output(output);
    let path = input
        .as_ref()
        .and_then(|i| i.path.clone())
        .or_else(|| output.as_ref().and_then(|o| o.path.clone()))?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    let mut detail = json!({"type": "edit", "filePath": strip_cwd(path, cwd)});
    if let Some(old) = input.as_ref().and_then(|i| i.old.clone()) {
        detail["oldString"] = old.into();
    }
    let new = input
        .as_ref()
        .and_then(|i| i.new.clone())
        .or_else(|| output.as_ref().and_then(|o| o.new.clone()));
    if let Some(new) = new {
        detail["newString"] = new.into();
    }
    let diff = input
        .as_ref()
        .and_then(|i| i.diff.clone())
        .or_else(|| output.as_ref().and_then(|o| o.diff.clone()));
    if let Some(diff) = diff.filter(|d| !d.is_empty()) {
        detail["unifiedDiff"] = diff.into();
    }
    Some(detail)
}

fn search_detail(input: &Value, tool_name: &str) -> Option<Value> {
    if input.is_null() {
        return None;
    }
    let query = ["query", "q", "pattern"]
        .iter()
        .find_map(|k| input.get(*k))?
        .as_str()?;
    if query.is_empty() {
        return None;
    }
    Some(json!({"type": "search", "query": query, "toolName": tool_name}))
}

/// `deriveCodexToolDetail`.
pub fn detail(name: &str, input: &Value, output: &Value, cwd: Option<&str>) -> Value {
    let parsed = match name {
        "Bash" | "shell" | "bash" | "exec" | "exec_command" | "command" => {
            shell_detail(input, output)
        }
        "edit" | "apply_patch" | "apply_diff" => edit_detail(input, output, cwd),
        "search" | "web_search" => search_detail(input, name),
        _ => None,
    };
    parsed.unwrap_or_else(|| json!({"type": "unknown", "input": input, "output": output}))
}

fn is_edit_tool(name: &str) -> bool {
    matches!(name, "edit" | "apply_patch" | "apply_diff")
}

fn renderable_edit(detail: &Value) -> bool {
    if detail["type"] != "edit" {
        return true;
    }
    ["unifiedDiff", "newString", "oldString"]
        .iter()
        .any(|k| detail[*k].as_str().is_some_and(|s| !s.trim().is_empty()))
}

/// `toToolCallTimelineItem`: a `tool_call` item from a resolved call.
pub fn tool_call(
    call_id: &str,
    name: &str,
    input: Value,
    output: Value,
    status: &str,
    error: Value,
    cwd: Option<&str>,
) -> Value {
    let mut detail = detail(name, &input, &output, cwd);
    if is_edit_tool(name) && status != "running" && !renderable_edit(&detail) {
        detail = json!({"type": "unknown", "input": input, "output": output});
    }
    let error = if status == "failed" {
        if error.is_null() {
            json!({"message": "Tool call failed"})
        } else {
            error
        }
    } else {
        Value::Null
    };
    json!({"type": "tool_call", "callId": call_id, "name": name, "status": status,
           "error": error, "detail": detail})
}

fn object_or_null(map: Map<String, Value>) -> Value {
    if map.is_empty() {
        Value::Null
    } else {
        Value::Object(map)
    }
}

fn command_execution(item: &Value) -> Option<Value> {
    let id = non_empty(&item["id"])?;
    let raw_status = opt_string(item, "status").ok()?;
    let command_value = match item.get("command") {
        None => Value::Null,
        Some(v @ Value::String(_)) => v.clone(),
        Some(Value::Array(parts)) if parts.iter().all(Value::is_string) => {
            Value::Array(parts.clone())
        }
        Some(_) => return None,
    };
    let cwd = opt_string(item, "cwd").ok()?;
    let aggregated = nullable_string(item, "aggregatedOutput").ok()?;
    let exit = match item.get("exitCode") {
        None => None,
        Some(v @ (Value::Null | Value::Number(_))) => Some(v.clone()),
        Some(_) => return None,
    };
    let command = display_command(&command_value);
    let parsed_output = shell_output(aggregated);
    let mut input = Map::new();
    if let Some(command) = &command {
        input.insert("command".into(), command.clone().into());
    }
    if let Some(cwd) = cwd {
        input.insert("cwd".into(), cwd.into());
    }
    let output = if parsed_output.is_some() || exit.is_some() {
        let mut output = Map::new();
        if let Some(command) = &command {
            output.insert("command".into(), command.clone().into());
        }
        if let Some(text) = parsed_output {
            output.insert("output".into(), text.into());
        }
        if let Some(exit) = exit {
            output.insert("exitCode".into(), exit);
        }
        Value::Object(output)
    } else {
        Value::Null
    };
    let error = present(item, "error");
    let status = status(raw_status, &error, &output);
    Some(tool_call(
        id,
        "shell",
        object_or_null(input),
        output,
        status,
        error,
        cwd,
    ))
}

struct FileEntry {
    path: String,
    kind: Option<String>,
    diff: Option<String>,
}

fn file_entry(entry: &Value, cwd: Option<&str>, fallback: Option<&str>) -> Option<FileEntry> {
    let path = ["path", "file_path", "filePath"]
        .iter()
        .find_map(|k| entry[*k].as_str().map(str::trim).filter(|p| !p.is_empty()))
        .or_else(|| fallback.map(str::trim).filter(|p| !p.is_empty()))?;
    // Codex 0.16x sends `kind` as `{"type": "add"}`; Paseo reads only a
    // string here, so such a kind is left out, as there.
    let kind = entry["kind"]
        .as_str()
        .or_else(|| entry["type"].as_str())
        .filter(|k| !k.is_empty())
        .map(str::to_string);
    let diff = [
        "diff",
        "patch",
        "unified_diff",
        "unifiedDiff",
        "content",
        "newString",
    ]
    .iter()
    .find_map(|k| non_empty(&entry[*k]))
    .map(str::to_string);
    Some(FileEntry {
        path: strip_cwd(path, cwd),
        kind,
        diff,
    })
}

fn file_entries(changes: &Value, cwd: Option<&str>) -> Vec<FileEntry> {
    match changes {
        Value::Array(list) => list
            .iter()
            .filter(|e| e.is_object())
            .filter_map(|e| file_entry(e, cwd, None))
            .collect(),
        Value::Object(map) => {
            if let Some(files) = changes.get("files").filter(|f| f.is_array()) {
                return file_entries(files, cwd);
            }
            if let Some(single) = file_entry(changes, cwd, None) {
                return vec![single];
            }
            map.iter()
                .filter_map(|(path, value)| match value {
                    Value::Object(_) => file_entry(value, cwd, Some(path)),
                    Value::String(diff) => {
                        let path = strip_cwd(path.trim(), cwd);
                        (!path.is_empty()).then(|| FileEntry {
                            path,
                            kind: None,
                            diff: Some(diff.clone()),
                        })
                    }
                    _ => None,
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

fn looks_like_unified_diff(text: &str) -> bool {
    let t = text.trim_start();
    !t.is_empty()
        && (t.starts_with("diff --git")
            || t.starts_with("@@")
            || t.starts_with("--- ")
            || t.starts_with("+++ "))
}

fn patch_directive(line: &str) -> Option<(&'static str, &str)> {
    let t = line.trim();
    for (prefix, kind) in [
        ("*** Add File:", "add"),
        ("*** Update File:", "update"),
        ("*** Delete File:", "delete"),
    ] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return Some((kind, rest.trim()));
        }
    }
    None
}

fn looks_like_apply_patch(text: &str) -> bool {
    let t = text.trim_start();
    !t.is_empty()
        && (t.starts_with("*** Begin Patch") || text.lines().any(|l| patch_directive(l).is_some()))
}

/// `codexApplyPatchToUnifiedDiff`.
fn apply_patch_to_diff(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    let mut out: Vec<String> = Vec::new();
    let mut saw = false;
    for line in normalized.split('\n') {
        if let Some((kind, path)) = patch_directive(line) {
            let path = path.trim().trim_matches(['"', '\'']);
            if !path.is_empty() {
                if out.last().is_some_and(|l| !l.is_empty()) {
                    out.push(String::new());
                }
                let left = if kind == "add" {
                    "/dev/null".to_string()
                } else {
                    format!("a/{path}")
                };
                let right = if kind == "delete" {
                    "/dev/null".to_string()
                } else {
                    format!("b/{path}")
                };
                out.push(format!("diff --git a/{path} b/{path}"));
                out.push(format!("--- {left}"));
                out.push(format!("+++ {right}"));
                saw = true;
            }
            continue;
        }
        let t = line.trim();
        if t == "*** Begin Patch"
            || t == "*** End Patch"
            || t == "*** End of File"
            || t.starts_with("*** Move to:")
        {
            continue;
        }
        if line.starts_with("@@")
            || line.starts_with('+')
            || line.starts_with('-')
            || line.starts_with(' ')
            || line.starts_with("\\ No newline at end of file")
        {
            out.push(line.to_string());
            saw = true;
        }
    }
    if !saw {
        return text.to_string();
    }
    let joined = out.join("\n");
    let joined = joined.trim();
    if joined.is_empty() {
        text.to_string()
    } else {
        joined.to_string()
    }
}

/// `classifyDiffLikeText`: `Ok(diff)` for a diff or an apply_patch body.
fn classify(text: &str) -> Result<String, String> {
    if looks_like_unified_diff(text) {
        Ok(text.to_string())
    } else if looks_like_apply_patch(text) {
        Ok(apply_patch_to_diff(text))
    } else {
        Err(text.to_string())
    }
}

/// `contentToDeletionDiff`.
fn deletion_diff(path: &str, content: &str) -> String {
    let normalized = content.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let mut out = vec![
        format!("diff --git a/{path} b/{path}"),
        format!("--- a/{path}"),
        "+++ /dev/null".to_string(),
    ];
    // Paseo keeps every line but a trailing empty one (its `indexOf` test
    // looks at the first occurrence, so repeated empty lines count once).
    let kept: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            !l.is_empty() || lines.iter().position(|x| x == l).unwrap_or(0) < lines.len() - 1
        })
        .collect();
    if !kept.is_empty() {
        out.push(format!("@@ -1,{} +0,0 @@", kept.len()));
        out.extend(kept.iter().map(|l| format!("-{l}")));
    }
    out.join("\n")
}

/// `{unifiedDiff}` for a diff, `{newString}` for anything else.
fn edit_text_fields(text: Option<&str>) -> (Option<String>, Option<String>) {
    match text.filter(|t| !t.is_empty()) {
        None => (None, None),
        Some(text) => match classify(text) {
            Ok(diff) => (Some(truncate_diff(&diff)), None),
            Err(text) => (None, Some(text)),
        },
    }
}

fn file_text_fields(file: Option<&FileEntry>) -> (Option<String>, Option<String>) {
    let Some(file) = file else {
        return (None, None);
    };
    if file.kind.as_deref() == Some("delete") {
        return match &file.diff {
            Some(diff) => match classify(diff) {
                Ok(diff) => (Some(truncate_diff(&diff)), None),
                Err(_) => (Some(truncate_diff(&deletion_diff(&file.path, diff))), None),
            },
            None => (Some(deletion_diff(&file.path, "")), None),
        };
    }
    edit_text_fields(file.diff.as_deref())
}

fn file_change(item: &Value, cwd: Option<&str>) -> Option<Value> {
    let id = non_empty(&item["id"])?;
    let raw_status = opt_string(item, "status").ok()?;
    let files = file_entries(&item["changes"], cwd);
    let listed = |with_text: bool| -> Vec<Value> {
        files
            .iter()
            .map(|file| {
                let mut entry = json!({"path": file.path});
                if let Some(kind) = &file.kind {
                    entry["kind"] = kind.clone().into();
                }
                if with_text {
                    if file.kind.as_deref() == Some("delete") {
                        entry["patch"] = file_text_fields(Some(file)).0.into();
                    } else if let Some(text) = file.diff.as_deref().filter(|t| !t.is_empty()) {
                        match classify(text) {
                            Ok(diff) => entry["patch"] = truncate_diff(&diff).into(),
                            Err(text) => entry["content"] = text.into(),
                        }
                    }
                }
                entry
            })
            .collect()
    };
    let output = if files.is_empty() {
        Value::Null
    } else {
        json!({"files": listed(true)})
    };
    let error = present(item, "error");
    let status = status(raw_status, &error, &output);
    let (diff, new) = file_text_fields(files.first());
    let mut input = Map::new();
    if !files.is_empty() {
        input.insert("files".into(), listed(false).into());
    }
    if diff.is_some() || new.is_some() {
        input.insert("path".into(), files[0].path.clone().into());
    }
    if let Some(diff) = diff {
        input.insert("patch".into(), diff.into());
    }
    if let Some(new) = new {
        input.insert("content".into(), new.into());
    }
    Some(tool_call(
        id,
        "apply_patch",
        object_or_null(input),
        output,
        status,
        error,
        cwd,
    ))
}

/// `splitCodexMcpToolResultImages`: image blocks become `[image]` text.
pub fn mcp_result_without_images(result: &Value) -> Value {
    let Some(content) = result["content"].as_array().filter(|_| result.is_object()) else {
        return result.clone();
    };
    let mut images = false;
    let replaced: Vec<Value> = content
        .iter()
        .map(|block| {
            if block["type"] == "image"
                && block["data"].is_string()
                && block["mimeType"].is_string()
            {
                images = true;
                json!({"type": "text", "text": "[image]"})
            } else {
                block.clone()
            }
        })
        .collect();
    if !images {
        return result.clone();
    }
    let mut out = result.clone();
    out["content"] = replaced.into();
    out
}

fn mcp_tool_call(item: &Value, cwd: Option<&str>) -> Option<Value> {
    let id = non_empty(&item["id"])?;
    let raw_status = opt_string(item, "status").ok()?;
    let tool = item["tool"].as_str().filter(|t| !t.is_empty())?;
    let server = opt_string(item, "server").ok()?;
    let tool = tool.trim();
    if tool.is_empty() {
        return None;
    }
    let name = match server.map(str::trim).filter(|s| !s.is_empty()) {
        Some(server) => format!("{server}.{tool}"),
        None => tool.to_string(),
    };
    let output = mcp_result_without_images(&present(item, "result"));
    let error = present(item, "error");
    let status = status(raw_status, &error, &output);
    Some(tool_call(
        id,
        &name,
        present(item, "arguments"),
        output,
        status,
        error,
        cwd,
    ))
}

fn web_search(item: &Value) -> Option<Value> {
    let id = non_empty(&item["id"])?;
    let raw_status = opt_string(item, "status").ok()?;
    let query = opt_string(item, "query").ok()?;
    let input = query.map_or(Value::Null, |q| json!({"query": q}));
    let output = present(item, "action");
    let error = present(item, "error");
    let status = status(Some(raw_status.unwrap_or("completed")), &error, &output);
    Some(tool_call(
        id,
        "web_search",
        input,
        output,
        status,
        error,
        None,
    ))
}

fn collab_agent(item: &Value) -> Option<Value> {
    let id = non_empty(&item["id"])?;
    let raw_status = opt_string(item, "status").ok()?;
    let prompt = opt_string(item, "prompt").ok()?;
    opt_string(item, "tool").ok()?;
    let states = match item.get("agentsStates") {
        None => None,
        Some(Value::Object(map)) => Some(map),
        Some(_) => return None,
    };
    let error = present(item, "error");
    let status = if !error.is_null() {
        "failed"
    } else {
        let parent = status_of(raw_status);
        if parent == "failed" {
            "failed"
        } else {
            let children: Vec<&str> = states
                .into_iter()
                .flat_map(|m| m.values())
                .filter_map(|s| s["status"].as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| match s.trim().to_lowercase().as_str() {
                    "error" | "errored" => "running",
                    "shutdown" => "canceled",
                    "notfound" => "failed",
                    _ => status_of(Some(s)),
                })
                .collect();
            if children.contains(&"failed") {
                "failed"
            } else if children.contains(&"canceled") {
                "canceled"
            } else if !children.is_empty() {
                if children.iter().all(|s| *s == "completed") {
                    "completed"
                } else {
                    "running"
                }
            } else {
                parent
            }
        }
    };
    let mut detail = json!({"type": "sub_agent", "subAgentType": "Sub-agent"});
    if let Some(prompt) = prompt.filter(|p| !p.is_empty()) {
        detail["description"] = prompt.into();
    }
    detail["log"] = "".into();
    detail["actions"] = json!([]);
    let error = if status == "failed" {
        if error.is_null() {
            json!({"message": "Sub-agent failed"})
        } else {
            error
        }
    } else {
        Value::Null
    };
    Some(
        json!({"type": "tool_call", "callId": id, "name": "Sub-agent", "status": status,
                "error": error, "detail": detail}),
    )
}

fn status_of(raw: Option<&str>) -> &'static str {
    status(raw, &Value::Null, &Value::Null)
}

fn sub_agent_activity(item: &Value) -> Option<Value> {
    let id = non_empty(&item["id"])?;
    let kind = item["kind"].as_str()?;
    if !matches!(kind, "started" | "interacted" | "interrupted") {
        return None;
    }
    non_empty(&item["agentThreadId"])?;
    let path = item["agentPath"].as_str()?;
    let mut name = if path == "/root" {
        String::new()
    } else if let Some(rest) = path.strip_prefix("/root/") {
        rest.to_string()
    } else if path.contains(['/', '\\']) {
        let cut = path.rfind(['/', '\\']).map_or(0, |at| at + 1);
        path[cut..].to_string()
    } else {
        path.to_string()
    };
    let description = name.clone();
    name = name
        .split('/')
        .map(|segment| {
            segment
                .split(['_', '-'])
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut chars = s.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => s,
            }
        })
        .collect::<Vec<_>>()
        .join(" / ");
    if name.is_empty() {
        name = "Sub-agent".into();
    }
    let status = if kind == "interrupted" {
        "canceled"
    } else {
        "running"
    };
    Some(
        json!({"type": "tool_call", "callId": id, "name": "Sub-agent", "status": status,
                "error": null, "detail": {"type": "sub_agent", "subAgentType": name,
                "description": description, "log": "", "actions": []}}),
    )
}

/// `mapCodexToolCallFromThreadItem`, for an item already known to be a tool.
fn tool_item(kind: &str, item: &Value, cwd: Option<&str>) -> Option<Value> {
    match kind {
        "commandExecution" => command_execution(item),
        "fileChange" => file_change(item, cwd),
        "mcpToolCall" => mcp_tool_call(item, cwd),
        "webSearch" => web_search(item),
        "collabAgentToolCall" => collab_agent(item),
        "subAgentActivity" => sub_agent_activity(item),
        _ => None,
    }
}

/// `normalizePlanMarkdown`.
pub fn plan_markdown(text: &str) -> String {
    text.split('\n')
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn join_strings(value: &Value) -> String {
    value
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .map(|p| p.as_str().map_or_else(|| p.to_string(), str::to_string))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// `threadItemToTimeline`: one Codex thread item as a timeline item, or
/// `None` for the kinds Paseo does not show. Images are not mapped yet.
pub fn thread_item(item: &Value, include_user_message: bool, cwd: Option<&str>) -> Option<Value> {
    if !item.is_object() {
        return None;
    }
    let kind = item_type(item)?;
    match kind.as_str() {
        "commandExecution"
        | "fileChange"
        | "mcpToolCall"
        | "webSearch"
        | "collabAgentToolCall"
        | "subAgentActivity" => tool_item(&kind, item, cwd),
        "userMessage" => {
            if !include_user_message {
                return None;
            }
            let parts: Vec<&str> = item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "text")
                .filter_map(|b| b["text"].as_str())
                .collect();
            let mut out = json!({"type": "user_message", "text": parts.join("\n")});
            if let Some(id) = non_empty(&item["id"]) {
                out["messageId"] = id.into();
            }
            let client = [
                &item["clientId"],
                &item["client_id"],
                &item["clientUserMessageId"],
            ]
            .into_iter()
            .find(|v| !v.is_null());
            if let Some(client) = client.and_then(non_empty) {
                out["clientMessageId"] = client.into();
            }
            Some(out)
        }
        "agentMessage" => {
            let mut out = json!({"type": "assistant_message",
                                 "text": item["text"].as_str().unwrap_or_default()});
            if let Some(id) = non_empty(&item["id"]) {
                out["messageId"] = id.into();
            }
            Some(out)
        }
        "plan" => {
            let text = plan_markdown(item["text"].as_str().unwrap_or_default());
            if text.is_empty() {
                return None;
            }
            let call_id = non_empty(&item["id"])
                .or_else(|| non_empty(&item["itemId"]))
                .map_or_else(|| format!("plan:{text}"), str::to_string);
            Some(
                json!({"type": "tool_call", "callId": call_id, "name": "plan",
                        "status": "completed", "error": null,
                        "detail": {"type": "plan", "text": text}}),
            )
        }
        "reasoning" => {
            let summary = join_strings(&item["summary"]);
            let text = if summary.is_empty() {
                join_strings(&item["content"])
            } else {
                summary
            };
            (!text.is_empty()).then(|| json!({"type": "reasoning", "text": text}))
        }
        "contextCompaction" => Some(json!({"type": "compaction", "status": "completed"})),
        _ => None,
    }
}

/// `mapCodexPlanUpdateToTodo`: a `turn/plan/updated` as a `todo` item.
pub fn plan_todo(plan: &Value) -> Value {
    let items: Vec<Value> = plan
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .filter_map(|(index, entry)| {
            let text = entry["step"].as_str()?.trim();
            if text.is_empty() {
                return None;
            }
            let status = match entry["status"].as_str() {
                Some("completed") => "completed",
                Some("inProgress" | "in_progress") => "in_progress",
                _ => "pending",
            };
            Some(
                json!({"id": index.to_string(), "text": text, "status": status,
                        "completed": status == "completed"}),
            )
        })
        .collect();
    json!({"type": "todo", "items": items})
}

/// The running `shell` call an approval request previews
/// (`mapCodexExecNotificationToToolCall` with `running: true`).
pub fn exec_preview(call_id: &str, command: &Value, cwd: Option<&str>) -> Option<Value> {
    let command = approval_command(command)?;
    let call_id = call_id.trim();
    if call_id.is_empty() {
        return None;
    }
    let mut input = json!({"command": command});
    if let Some(cwd) = cwd.filter(|c| !c.is_empty()) {
        input["cwd"] = cwd.into();
    }
    let mut call = tool_call(
        call_id,
        "shell",
        input,
        Value::Null,
        "completed",
        Value::Null,
        cwd,
    );
    call["status"] = "running".into();
    Some(call)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CWD: &str = "/home/user/repo";

    #[test]
    fn a_command_loses_its_shell_wrapper() {
        assert_eq!(
            display_command(&json!("/bin/bash -lc 'echo hi > a.txt'")).as_deref(),
            Some("echo hi > a.txt")
        );
        assert_eq!(
            display_command(&json!("/usr/bin/zsh -c ls")).as_deref(),
            Some("ls")
        );
        assert_eq!(
            display_command(&json!(["bash", "-lc", "git status"])).as_deref(),
            Some("git status")
        );
        assert_eq!(
            display_command(&json!("pwsh -NoProfile -Command Get-ChildItem")).as_deref(),
            Some("Get-ChildItem")
        );
        assert_eq!(
            approval_command(&json!("/usr/bin/bash -lc ls")),
            Some(json!("/usr/bin/bash -lc ls")),
            "the approval path unwraps only /bin/"
        );
    }

    #[test]
    fn a_command_item_maps_like_paseo() {
        let running = json!({"type": "commandExecution", "id": "exec-1",
            "command": "/bin/bash -lc 'echo probe > probe.txt'", "cwd": CWD,
            "processId": null, "status": "inProgress", "aggregatedOutput": null, "exitCode": null});
        let call = thread_item(&running, false, Some(CWD)).unwrap();
        assert_eq!(
            call,
            json!({"type": "tool_call", "callId": "exec-1", "name": "shell", "status": "running",
                   "error": null, "detail": {"type": "shell", "command": "echo probe > probe.txt",
                   "cwd": CWD}})
        );
        let mut done = running.clone();
        done["status"] = "completed".into();
        done["exitCode"] = 0.into();
        done["aggregatedOutput"] = "probe\n".into();
        let call = thread_item(&done, false, Some(CWD)).unwrap();
        assert_eq!(call["status"], "completed");
        assert_eq!(call["detail"]["output"], "probe\n");
        assert_eq!(call["detail"]["exitCode"], 0);
        let mut declined = running;
        declined["status"] = "declined".into();
        let call = thread_item(&declined, false, Some(CWD)).unwrap();
        assert_eq!(call["status"], "failed");
        assert_eq!(call["error"], json!({"message": "Tool call failed"}));
    }

    #[test]
    fn a_file_change_becomes_an_edit() {
        let item = json!({"type": "fileChange", "id": "patch-1", "status": "completed",
            "changes": [{"path": format!("{CWD}/src/a.rs"), "kind": {"type": "update"},
                         "diff": "@@ -1 +1 @@\n-old\n+new\n"}]});
        let call = thread_item(&item, false, Some(CWD)).unwrap();
        assert_eq!(call["name"], "apply_patch");
        assert_eq!(
            call["detail"],
            json!({"type": "edit", "filePath": "src/a.rs",
                   "unifiedDiff": "@@ -1 +1 @@\n-old\n+new\n"})
        );
        let added = json!({"type": "fileChange", "id": "patch-2", "status": "completed",
            "changes": [{"path": format!("{CWD}/b.txt"), "kind": {"type": "add"},
                         "diff": "hello\n"}]});
        let call = thread_item(&added, false, Some(CWD)).unwrap();
        assert_eq!(
            call["detail"],
            json!({"type": "edit", "filePath": "b.txt", "newString": "hello\n"})
        );
        let empty = json!({"type": "fileChange", "id": "patch-3", "status": "completed",
            "changes": []});
        assert_eq!(
            thread_item(&empty, false, Some(CWD)).unwrap()["detail"]["type"],
            "unknown"
        );
    }

    #[test]
    fn an_apply_patch_body_becomes_a_unified_diff() {
        let patch = "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch";
        assert_eq!(
            apply_patch_to_diff(patch),
            "diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@\n-old\n+new"
        );
    }

    #[test]
    fn other_items_map_like_paseo() {
        let mcp = json!({"type": "mcpToolCall", "id": "m1", "server": "docs", "tool": "search",
            "status": "completed", "arguments": {"q": "x"}, "result": {"content": []},
            "error": null});
        let call = thread_item(&mcp, false, None).unwrap();
        assert_eq!(call["name"], "docs.search");
        assert_eq!(call["detail"]["type"], "unknown");
        let web = json!({"type": "webSearch", "id": "w1", "query": "rust", "action": null});
        let call = thread_item(&web, false, None).unwrap();
        assert_eq!(
            call["detail"],
            json!({"type": "search", "query": "rust", "toolName": "web_search"})
        );
        assert_eq!(call["status"], "completed");
        let plan = json!({"type": "plan", "id": "p1", "text": "- one  \n- two\n"});
        assert_eq!(
            thread_item(&plan, false, None).unwrap()["detail"],
            json!({"type": "plan", "text": "- one\n- two"})
        );
        let reasoning =
            json!({"type": "reasoning", "id": "r1", "summary": ["a", "b"], "content": []});
        assert_eq!(
            thread_item(&reasoning, false, None).unwrap(),
            json!({"type": "reasoning", "text": "a\nb"})
        );
        let user = json!({"type": "userMessage", "id": "u1", "clientId": null,
            "content": [{"type": "text", "text": "hi", "text_elements": []}]});
        assert!(thread_item(&user, false, None).is_none());
        assert_eq!(
            thread_item(&user, true, None).unwrap(),
            json!({"type": "user_message", "text": "hi", "messageId": "u1"})
        );
        assert_eq!(
            plan_todo(
                &json!([{"step": "a", "status": "inProgress"}, {"step": " ", "status": "pending"}])
            ),
            json!({"type": "todo", "items": [{"id": "0", "text": "a", "status": "in_progress",
                   "completed": false}]})
        );
    }

    #[test]
    fn unified_exec_envelopes_are_unwrapped() {
        let text = "Chunk ID: 1\nWall time: 0.1s\nProcess exited with code 0\nOutput:\nhello";
        assert_eq!(shell_output(Some(text)).as_deref(), Some("hello"));
        assert_eq!(shell_output(Some("plain")).as_deref(), Some("plain"));
    }

    #[test]
    fn long_diffs_are_cut_like_javascript() {
        let long = "x".repeat(DIFF_LIMIT + 5);
        assert!(truncate_diff(&long).ends_with("\n...[truncated 5 chars]"));
    }
}
