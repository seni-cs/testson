//! Readable lines from an NDJSON stream in the Anthropic Messages format.
//!
//! `claude --output-format stream-json` and `grok --output-format streaming-messages-json` both
//! emit it. Without it, both CLIs print only the final answer, so no progress is visible.

use serde_json::Value;

const COMMAND_PREVIEW: usize = 200;

/// What one stream line contributes.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Lines to show the operator. Empty for events with nothing worth showing.
    Show(Vec<String>),
    /// The run finished with this final answer.
    Result { text: String, is_error: bool },
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
                // The echo ends at its closing quote, and the agent may chain commands after it.
                let line = command[index..].lines().next().unwrap_or_default();
                let message = line.split(['"', '\'']).next().unwrap_or_default();
                return Some(message.trim_end().to_owned());
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
    fn captures_the_result_and_hides_other_events() {
        assert_eq!(
            parse(r#"{"type":"result","is_error":false,"result":"{\"ok\":1}"}"#),
            Event::Result {
                text: r#"{"ok":1}"#.into(),
                is_error: false
            }
        );
        assert_eq!(
            parse(r#"{"type":"user","message":{}}"#),
            Event::Show(vec![])
        );
        assert_eq!(parse("plain text"), Event::Show(vec!["plain text".into()]));
    }
}
