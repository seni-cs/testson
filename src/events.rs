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
    Result {
        text: String,
        is_error: bool,
        /// How the run ended, such as `success` or `error_max_turns`, when the stream says.
        subtype: Option<String>,
    },
}

impl Event {
    /// What a final answer says, or for an error with no text, how the run ended.
    ///
    /// Claude reports a run stopped by its turn limit or a failure during execution as an error
    /// result whose only explanation is its subtype.
    #[must_use]
    pub fn answer(text: String, is_error: bool, subtype: Option<String>) -> String {
        if !is_error || !text.trim().is_empty() {
            return text;
        }
        match subtype.filter(|subtype| !subtype.trim().is_empty()) {
            Some(subtype) => format!("{subtype}, with no message"),
            None => "no message and no reason".to_owned(),
        }
    }
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
            subtype: event
                .get("subtype")
                .and_then(Value::as_str)
                .map(str::to_owned),
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

/// The message of the `[PROGRESS]` echo that starts at `index` in `command`.
///
/// The agent may chain other commands after the echo. A quoted message ends at the quote that
/// closes the one it is in, so an apostrophe inside double quotes is kept. An unquoted one ends
/// at the first shell separator.
fn progress_message(command: &str, index: usize) -> String {
    let line_start = command[..index]
        .rfind('\n')
        .map_or(0, |newline| newline + 1);
    let rest = command[index..].lines().next().unwrap_or_default();
    let message = match open_quote(&command[line_start..index]) {
        Some(quote) => quoted(rest, quote),
        None => unquoted(rest).to_owned(),
    };
    message.trim_end().to_owned()
}

/// The quote still open at the end of `prefix`, the part of the line before `[PROGRESS]`.
///
/// This finds the quote of `echo "$(date) [PROGRESS] …"` too, not only one right before it.
fn open_quote(prefix: &str) -> Option<char> {
    let mut open = None;
    let mut chars = prefix.chars();
    while let Some(character) = chars.next() {
        match (open, character) {
            (Some('\''), '\'') | (Some('"'), '"') => open = None,
            (None | Some('"'), '\\') | (Some('"'), '`') => {
                chars.next();
            }
            (None, '"' | '\'') => open = Some(character),
            _ => {}
        }
    }
    open
}

/// `text` up to the closing `quote`, or all of it when the quote is not closed on this line.
///
/// The escapes that stand for one character inside the quotes are undone: in single quotes,
/// POSIX `'\''` and PowerShell `''`; in double quotes, the shell's backslash escapes and
/// PowerShell's `` `" `` and `""`.
fn quoted(text: &str, quote: char) -> String {
    let escapes: &[(&str, char)] = if quote == '\'' {
        &[("'\\''", '\''), ("''", '\'')]
    } else {
        &[
            ("\\\"", '"'),
            ("\\\\", '\\'),
            ("\\$", '$'),
            ("\\`", '`'),
            ("`\"", '"'),
            ("\"\"", '"'),
        ]
    };
    let mut message = String::new();
    let mut rest = text;
    while let Some(character) = rest.chars().next() {
        if let Some((escape, literal)) = escapes.iter().find(|(escape, _)| rest.starts_with(escape))
        {
            message.push(*literal);
            rest = &rest[escape.len()..];
        } else if character == quote {
            break;
        } else {
            message.push(character);
            rest = &rest[character.len_utf8()..];
        }
    }
    message
}

/// `text` up to the first `;`, `&`, `|` or `>`.
fn unquoted(text: &str) -> &str {
    let end = [";", "&", "|", ">"]
        .iter()
        .filter_map(|separator| text.find(separator))
        .min()
        .unwrap_or(text.len());
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(command: &str) -> Option<String> {
        describe(&serde_json::json!({
            "type": "tool_use",
            "name": "Bash",
            "input": {"command": command}
        }))
    }

    #[test]
    fn a_quoted_progress_message_ends_at_its_matching_quote() {
        assert_eq!(
            progress("echo '[PROGRESS] Opening the lobby' && playwright-cli open x").as_deref(),
            Some("[PROGRESS] Opening the lobby")
        );
        assert_eq!(
            progress(r#"echo "[PROGRESS] Opening the game's rules"; playwright-cli click e3"#)
                .as_deref(),
            Some("[PROGRESS] Opening the game's rules")
        );
        assert_eq!(
            progress(r#"echo "[PROGRESS] Clicking \"Jugar\" in demo mode" && x"#).as_deref(),
            Some(r#"[PROGRESS] Clicking "Jugar" in demo mode"#)
        );
        assert_eq!(
            progress("cd /tmp\necho \"[PROGRESS] Saving the report\"\nplaywright-cli close")
                .as_deref(),
            Some("[PROGRESS] Saving the report")
        );
    }

    #[test]
    fn a_progress_message_keeps_escaped_quotes_and_finds_its_opening_quote() {
        for (command, message) in [
            (
                r"echo '[PROGRESS] Opening the game'\''s rules' && x",
                "[PROGRESS] Opening the game's rules",
            ),
            (
                "Write-Output '[PROGRESS] Opening the game''s rules'; x",
                "[PROGRESS] Opening the game's rules",
            ),
            (
                r#"Write-Output "[PROGRESS] Clicking `"Jugar`" now"; x"#,
                r#"[PROGRESS] Clicking "Jugar" now"#,
            ),
            (
                r#"echo "$(date +%T) [PROGRESS] Spinning'n'waiting"; x"#,
                "[PROGRESS] Spinning'n'waiting",
            ),
            (
                r#"echo "$(date "+%T") [PROGRESS] It's spinning" | tee x"#,
                "[PROGRESS] It's spinning",
            ),
            (r"cd 'C:\x' && echo [PROGRESS] Done; x", "[PROGRESS] Done"),
        ] {
            assert_eq!(progress(command).as_deref(), Some(message), "{command}");
        }
    }

    #[test]
    fn an_unquoted_progress_message_ends_at_a_shell_separator() {
        for (command, message) in [
            (
                "echo [PROGRESS] Spinning once && playwright-cli click e1",
                "[PROGRESS] Spinning once",
            ),
            ("echo [PROGRESS] Waiting; sleep 2", "[PROGRESS] Waiting"),
            ("echo [PROGRESS] Logging | tee -a x", "[PROGRESS] Logging"),
            ("echo [PROGRESS] Noting > note.txt", "[PROGRESS] Noting"),
            ("echo [PROGRESS] Waiting & sleep 2", "[PROGRESS] Waiting"),
            (
                "echo [PROGRESS] Done\nplaywright-cli close",
                "[PROGRESS] Done",
            ),
        ] {
            assert_eq!(progress(command).as_deref(), Some(message), "{command}");
        }
    }

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
                is_error: false,
                subtype: None,
            }
        );
        assert_eq!(
            parse(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#),
            Event::Result {
                text: String::new(),
                is_error: true,
                subtype: Some("error_max_turns".into()),
            }
        );
        assert_eq!(
            parse(r#"{"type":"user","message":{}}"#),
            Event::Show(vec![])
        );
        assert_eq!(parse("plain text"), Event::Show(vec!["plain text".into()]));
    }

    #[test]
    fn an_error_without_text_is_explained_by_its_subtype() {
        let subtype = || Some("error_during_execution".to_owned());
        assert_eq!(
            Event::answer(String::new(), true, subtype()),
            "error_during_execution, with no message"
        );
        assert_eq!(
            Event::answer(" \n".into(), true, None),
            "no message and no reason"
        );
        assert_eq!(
            Event::answer("Rate limited".into(), true, subtype()),
            "Rate limited"
        );
        assert_eq!(Event::answer(String::new(), false, subtype()), "");
    }
}
