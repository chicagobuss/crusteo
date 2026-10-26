//! pi's tool calls as timeline items: Paseo's `pi/tool-call-mapper.ts`.
//!
//! pi's built-in tools are `bash`, `read`, `edit`, `write`, `find`, `grep`
//! and `ls`. A call whose arguments do not fit its tool's shape, and every
//! other tool, is shown as `unknown` with its raw input and result.

use serde_json::{Map, Value, json};

/// A tool call as it started: its name and its arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub name: String,
    pub args: Value,
}

/// Whether `args` has every key of `required` as a string, and every key of
/// `numbers` (when present) as a number.
fn fits(args: &Value, required: &[&str], strings: &[&str], numbers: &[&str]) -> bool {
    let Some(map) = args.as_object() else {
        return false;
    };
    required
        .iter()
        .all(|k| map.get(*k).is_some_and(Value::is_string))
        && strings
            .iter()
            .all(|k| map.get(*k).is_none_or(|v| v.is_string()))
        && numbers
            .iter()
            .all(|k| map.get(*k).is_none_or(|v| v.is_number()))
}

/// The edit as `(path, oldText, newText)`, from pi's `edits` list or the
/// older single-edit argument names.
fn edit_args(args: &Value) -> Option<(String, Option<String>, Option<String>)> {
    let path = args["path"].as_str()?.to_string();
    if let Some(edits) = args["edits"].as_array() {
        let valid = edits
            .iter()
            .all(|e| e["oldText"].is_string() && e["newText"].is_string());
        if valid {
            let first = edits.first();
            let text = |key: &str| first.and_then(|e| e[key].as_str()).map(str::to_string);
            return Some((path, text("oldText"), text("newText")));
        }
    }
    let old = args["old_string"]
        .as_str()
        .or(args["oldString"].as_str())
        .filter(|s| !s.is_empty())?;
    let new = args["new_string"].as_str().or(args["newString"].as_str())?;
    Some((path, Some(old.to_string()), Some(new.to_string())))
}

/// A tool result as Paseo reads it: a string, an object, or nothing.
pub fn result(raw: &Value) -> Value {
    match raw {
        Value::String(_) | Value::Object(_) => raw.clone(),
        _ => Value::Null,
    }
}

/// The text of a result: the string itself, its `output`, `stdout` or
/// `text`, or its text content blocks.
pub fn result_text(result: &Value) -> Option<String> {
    if let Some(text) = result.as_str() {
        return Some(text.to_string());
    }
    let object = result.as_object()?;
    for key in ["output", "stdout", "text"] {
        if let Some(text) = object
            .get(key)
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            return Some(text.to_string());
        }
    }
    let parts: Vec<&str> = object
        .get("content")?
        .as_array()?
        .iter()
        .filter(|b| b["type"] == "text")
        .filter_map(|b| b["text"].as_str())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

fn put(detail: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        detail.insert(key.into(), value);
    }
}

/// The `detail` of a pi tool call, from its arguments and, once it has
/// one, its (partial or final) result.
pub fn detail(call: &Call, result: &Value) -> Value {
    let args = &call.args;
    let string_result = result.as_str().map(|s| Value::from(s.to_string()));
    let mut detail = Map::new();
    match call.name.as_str() {
        "bash" if fits(args, &["command"], &[], &["timeout"]) => {
            detail.insert("type".into(), "shell".into());
            detail.insert("command".into(), args["command"].clone());
            match result {
                Value::String(text) => {
                    detail.insert("output".into(), text.clone().into());
                }
                Value::Object(object) => {
                    put(&mut detail, "output", result_text(result).map(Value::from));
                    let code = object
                        .get("exitCode")
                        .or_else(|| object.get("code"))
                        .filter(|c| c.is_number())
                        .cloned()
                        .unwrap_or(Value::Null);
                    detail.insert("exitCode".into(), code);
                }
                _ => {}
            }
        }
        "read" if fits(args, &["path"], &[], &["offset", "limit"]) => {
            detail.insert("type".into(), "read".into());
            detail.insert("filePath".into(), args["path"].clone());
            put(&mut detail, "content", result_text(result).map(Value::from));
            put(&mut detail, "offset", args.get("offset").cloned());
            put(&mut detail, "limit", args.get("limit").cloned());
        }
        "edit" if edit_args(args).is_some() => {
            let (path, old, new) = edit_args(args).unwrap_or_default();
            detail.insert("type".into(), "edit".into());
            detail.insert("filePath".into(), path.into());
            put(&mut detail, "oldString", old.map(Value::from));
            put(&mut detail, "newString", new.map(Value::from));
            put(
                &mut detail,
                "unifiedDiff",
                result["details"]["diff"].as_str().map(Value::from),
            );
        }
        "write" if fits(args, &["path", "content"], &[], &[]) => {
            detail.insert("type".into(), "write".into());
            detail.insert("filePath".into(), args["path"].clone());
            detail.insert("content".into(), args["content"].clone());
        }
        "find" if fits(args, &["pattern"], &["path"], &["limit"]) => {
            detail.insert("type".into(), "search".into());
            detail.insert("query".into(), args["pattern"].clone());
            detail.insert("toolName".into(), "search".into());
            put(&mut detail, "content", string_result);
        }
        "grep" if fits(args, &["pattern"], &["path", "glob"], &["context", "limit"]) => {
            detail.insert("type".into(), "search".into());
            detail.insert("query".into(), args["pattern"].clone());
            detail.insert("toolName".into(), "grep".into());
            put(&mut detail, "content", string_result);
        }
        "ls" if fits(args, &[], &["path"], &["limit"]) => {
            detail.insert("type".into(), "search".into());
            let query = args["path"].as_str().unwrap_or("ls");
            detail.insert("query".into(), query.into());
            put(&mut detail, "content", string_result);
        }
        _ => {
            return json!({"type": "unknown", "input": args, "output": result});
        }
    }
    Value::Object(detail)
}

/// A `tool_call` timeline item.
pub fn item(call_id: &str, call: &Call, status: &str, result: &Value, error: Value) -> Value {
    json!({"type": "tool_call", "callId": call_id, "name": call.name,
           "detail": detail(call, result), "status": status, "error": error})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, args: Value) -> Call {
        Call {
            name: name.into(),
            args,
        }
    }

    #[test]
    fn details_match_paseo() {
        let bash = call("bash", json!({"command": "ls"}));
        assert_eq!(
            detail(&bash, &Value::Null),
            json!({"type": "shell", "command": "ls"})
        );
        // pi 1.0's result: content blocks and a structured copy Paseo ignores.
        let done = json!({"content": [{"type": "text", "text": "a\nb\n"}],
                          "structuredContent": {"output": "a\nb\n", "exit_code": 0}});
        assert_eq!(
            detail(&bash, &done),
            json!({"type": "shell", "command": "ls", "output": "a\nb\n", "exitCode": null})
        );
        assert_eq!(
            detail(&bash, &json!({"content": []})),
            json!({"type": "shell", "command": "ls", "exitCode": null}),
            "a partial result without text yet"
        );
        let read = call("read", json!({"path": "/x", "offset": 2}));
        assert_eq!(
            detail(
                &read,
                &json!({"content": [{"type": "text", "text": "one"}]})
            ),
            json!({"type": "read", "filePath": "/x", "content": "one", "offset": 2})
        );
        let edit = call(
            "edit",
            json!({"path": "/x", "edits": [{"oldText": "a", "newText": "b"}]}),
        );
        assert_eq!(
            detail(&edit, &json!({"details": {"diff": "-a\n+b"}})),
            json!({"type": "edit", "filePath": "/x", "oldString": "a", "newString": "b",
                   "unifiedDiff": "-a\n+b"})
        );
        let legacy = call(
            "edit",
            json!({"path": "/x", "old_string": "a", "new_string": ""}),
        );
        assert_eq!(detail(&legacy, &Value::Null)["newString"], "");
        let ls = call("ls", json!({}));
        assert_eq!(
            detail(&ls, &json!("a\nb")),
            json!({"type": "search", "query": "ls", "content": "a\nb"})
        );
        let grep = call("grep", json!({"pattern": "fn", "ignoreCase": true}));
        assert_eq!(detail(&grep, &Value::Null)["toolName"], "grep");
        let other = call("web_search", json!({"q": 1}));
        assert_eq!(
            detail(&other, &json!({"ok": true})),
            json!({"type": "unknown", "input": {"q": 1}, "output": {"ok": true}})
        );
        assert_eq!(
            detail(&call("bash", json!({"cmd": "ls"})), &Value::Null)["type"],
            "unknown",
            "arguments of the wrong shape"
        );
    }
}
