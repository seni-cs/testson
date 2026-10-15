//! Readable lines from an NDJSON stream in the Anthropic Messages format.
//!
//! `claude --output-format stream-json` and `grok --output-format streaming-messages-json` both
//! emit it. Without it, both CLIs print only the final answer, so no progress is visible.

use serde_json::Value;

const COMMAND_PREVIEW: usize = 200;

/// What one stream line contributes.
#[derive(Debug, PartialEq)]
pub enum Event {
    /// Lines to show the operator. Empty for events with nothing worth showing.
    Show(Vec<String>),
    /// The run finished with this final answer, and what it cost when the CLI says.
    Result {
        text: String,
        is_error: bool,
        cost_usd: Option<f64>,
    },
}

/// Interprets one stdout line. A line that is not a stream event is shown as it is.
pub fn parse(line: &str) -> Event {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Event::Show(vec![line.to_owned()]);
    };
    match event.get("type").and_then(Value::as_str) {
        Some("assistant") => Event::Show(
            event
                .pointer("/message/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(describe)
                .collect(),
        ),
        Some("result") => Event::Result {
            text: event
                .get("result")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            is_error: event
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            cost_usd: event.get("total_cost_usd").and_then(Value::as_f64),
        },
        Some(_) => Event::Show(Vec::new()),
        None => Event::Show(vec![line.to_owned()]),
    }
}

/// A text block as it is, a shell call as its command, and a `[PROGRESS]` echo as the message.
fn describe(block: &Value) -> Option<String> {
    match block.get("type")?.as_str()? {
        "text" => {
            Some(block.get("text")?.as_str()?.trim().to_owned()).filter(|text| !text.is_empty())
        }
        "tool_use" => {
            let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
            let Some(command) = block.pointer("/input/command").and_then(Value::as_str) else {
                return Some(format!("[{name}]"));
            };
            if let Some(index) = command.find("[PROGRESS]") {
                return Some(progress_message(command, index));
            }
            let first = command.lines().next().unwrap_or_default();
            let mut preview: String = first.chars().take(COMMAND_PREVIEW).collect();
            if preview.len() < command.len() {
                preview.push_str(" …");
            }
            Some(format!("$ {preview}"))
        }
        _ => None,
    }
}

/// The message of the `[PROGRESS]` echo at `index`. The agent may chain commands after it, so
/// a quoted message ends at the quote it was opened with, and an unquoted one at the first shell
/// separator.
fn progress_message(command: &str, index: usize) -> String {
    let line_start = command[..index]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let quote = command[line_start..index]
        .chars()
        .rev()
        .find(|character| matches!(character, '"' | '\''));
    let rest = command[index..].lines().next().unwrap_or_default();
    let end = match quote {
        Some(quote) => rest.find(quote),
        None => rest.find([';', '&', '|', '>']),
    };
    rest[..end.unwrap_or(rest.len())].trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_progress_commands_and_text() {
        let line = r#"{"type":"assistant","message":{"content":[
            {"type":"text","text":"Opening the lobby."},
            {"type":"tool_use","name":"Bash","input":{"command":"echo \"[PROGRESS] Opening target URL\"; playwright-cli open x"}},
            {"type":"tool_use","name":"run_terminal_command","input":{"command":"playwright-cli --session s open https://www.betsson.es/casino\nsecond line"}},
            {"type":"tool_use","name":"Read","input":{"file_path":"/x"}},
            {"type":"thinking","thinking":"hidden"}
        ]}}"#;
        assert_eq!(
            parse(&line.replace('\n', "")),
            Event::Show(vec![
                "Opening the lobby.".into(),
                "[PROGRESS] Opening target URL".into(),
                "$ playwright-cli --session s open https://www.betsson.es/casino …".into(),
                "[Read]".into(),
            ])
        );
    }

    #[test]
    fn keeps_an_apostrophe_inside_a_progress_message() {
        assert_eq!(
            progress_message(r#"echo "[PROGRESS] Opening the game's rules" && x"#, 6),
            "[PROGRESS] Opening the game's rules"
        );
        assert_eq!(
            progress_message("echo [PROGRESS] Reloading; playwright-cli reload", 5),
            "[PROGRESS] Reloading"
        );
    }

    #[test]
    fn captures_the_result_and_hides_other_events() {
        assert_eq!(
            parse(r#"{"type":"result","is_error":false,"result":"{\"ok\":1}"}"#),
            Event::Result {
                text: r#"{"ok":1}"#.into(),
                is_error: false,
                cost_usd: None,
            }
        );
        assert_eq!(
            parse(r#"{"type":"result","is_error":true,"result":"no","total_cost_usd":0.25}"#),
            Event::Result {
                text: "no".into(),
                is_error: true,
                cost_usd: Some(0.25),
            }
        );
        assert_eq!(
            parse(r#"{"type":"user","message":{}}"#),
            Event::Show(vec![])
        );
        assert_eq!(parse("plain text"), Event::Show(vec!["plain text".into()]));
    }
}
