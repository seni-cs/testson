//! Prompt filling, saved sessions, and Betsson URL checks.

use anyhow::{Result, bail};
use serde_json::Value;
use url::Url;

pub const TEMPLATE: &str = include_str!("../prompts/test-game.txt");
pub const REPORT_SCHEMA: &str = include_str!("../schema/test-game.schema.json");
pub const SCRIPT_SCHEMA: &str = include_str!("../schema/test-script.schema.json");
/// Written to each run directory: the agent and the replay run them to capture the postMessages
/// of the page and read them back.
pub const CAPTURE_MESSAGES: &str = include_str!("../prompts/capture-messages.js");
pub const COLLECT_MESSAGES: &str = include_str!("../prompts/collect-messages.js");
/// The rules and criteria of each mode, put into the template at `{{MODE_INSTRUCTIONS}}`.
pub const REAL_MONEY_INSTRUCTIONS: &str = include_str!("../prompts/real-money.txt");
pub const NO_BALANCE_INSTRUCTIONS: &str = include_str!("../prompts/no-balance.txt");

const SLUG_LIMIT: usize = 80;

/// Values substituted into the prompt template.
#[derive(Debug, Clone, Copy)]
pub struct TemplateValues<'a> {
    pub game_name: &'a str,
    /// The provider as the catalogue names it, such as `PragmaticPlay`.
    pub provider: &'a str,
    /// The primary type of the game as the catalogue names it, such as `Slots`.
    pub game_type: &'a str,
    /// The mode as the schema names it, such as `real_money`.
    pub mode: &'a str,
    /// [`REAL_MONEY_INSTRUCTIONS`] or [`NO_BALANCE_INSTRUCTIONS`].
    pub mode_instructions: &'a str,
    pub betsson_url: &'a str,
    /// Private run directory: the saved session and whatever the agent works with.
    pub output_dir: &'a str,
    /// Directory the dashboard serves, where the video and screenshots go.
    pub assets_dir: &'a str,
    pub session_name: &'a str,
    /// URL path `assets_dir` is served at, the prefix of video and screenshot paths.
    pub data_prefix: &'a str,
    /// The command that checks the replay script the agent wrote against the rules of a replay.
    pub check_script: &'a str,
}

/// Accepts an `https` URL whose registrable label is `betsson`. Used as a clap value parser.
///
/// # Errors
///
/// Fails when the URL is not `https` on a `betsson` host.
pub fn parse_betsson_url(url: &str) -> Result<String, String> {
    let valid =
        Url::parse(url).is_ok_and(|parsed| parsed.scheme() == "https" && is_betsson(&parsed));
    if valid {
        Ok(url.to_owned())
    } else {
        Err("must be https on a betsson host, for example https://www.betsson.es/casino".into())
    }
}

/// Whether the registrable label of the host of `url` is `betsson`.
fn is_betsson(url: &Url) -> bool {
    let host = url.host_str().unwrap_or("").to_lowercase();
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    labels.len() >= 2 && labels[labels.len() - 2] == "betsson"
}

/// Builds a filesystem slug of at most 80 characters. An empty name becomes `game`.
#[must_use]
pub fn slugify(name: &str) -> String {
    let mut slug = String::new();
    let mut dash = false;
    for character in name.trim().to_lowercase().chars() {
        if character.is_ascii_alphanumeric() {
            if dash && !slug.is_empty() {
                slug.push('-');
            }
            dash = false;
            slug.push(character);
        } else {
            dash = true;
        }
    }
    if slug.is_empty() {
        slug.push_str("game");
    }
    slug.chars().take(SLUG_LIMIT).collect()
}

/// The localStorage keys Betsson keeps the login in. Any one of them means a logged-in page.
const AUTH_KEYS: [&str; 3] = ["session", "authSession", "authReducer"];

/// Whether a Playwright storage state, as `state-save` writes it, holds a Betsson login.
#[must_use]
pub fn session_has_auth(state: &Value) -> bool {
    state["origins"].as_array().is_some_and(|origins| {
        origins.iter().any(|origin| {
            let betsson = origin["origin"]
                .as_str()
                .and_then(|text| Url::parse(text).ok())
                .is_some_and(|url| url.scheme() == "https" && is_betsson(&url));
            betsson
                && origin["localStorage"].as_array().is_some_and(|items| {
                    items.iter().any(|item| {
                        item["name"]
                            .as_str()
                            .is_some_and(|name| AUTH_KEYS.contains(&name))
                            && item["value"]
                                .as_str()
                                .is_some_and(|value| !value.is_empty())
                    })
                })
        })
    })
}

/// Replaces every `{{PLACEHOLDER}}` in `template` and fails when one is absent.
///
/// # Errors
///
/// Fails when the template lacks one of the placeholders.
pub fn fill_template(template: &str, values: TemplateValues<'_>) -> Result<String> {
    let pairs = [
        ("GAME_NAME", values.game_name),
        ("PROVIDER", values.provider),
        ("GAME_TYPE", values.game_type),
        ("MODE", values.mode),
        (
            "MODE_INSTRUCTIONS",
            values.mode_instructions.trim_end_matches('\n'),
        ),
        ("BETSSON_URL", values.betsson_url),
        ("OUTPUT_DIR", values.output_dir),
        ("ASSETS_DIR", values.assets_dir),
        ("SESSION_NAME", values.session_name),
        ("DATA_PREFIX", values.data_prefix),
        ("CHECK_SCRIPT", values.check_script),
        ("SCHEMA", REPORT_SCHEMA.trim_end_matches('\n')),
        ("SCRIPT_SCHEMA", SCRIPT_SCHEMA.trim_end_matches('\n')),
    ];
    fill(template, &pairs)
}

/// Replaces every `{{KEY}}` of `pairs` in `template` and fails when one is absent.
///
/// # Errors
///
/// Fails when the template lacks one of the placeholders.
fn fill(template: &str, pairs: &[(&str, &str)]) -> Result<String> {
    let missing: Vec<String> = pairs
        .iter()
        .map(|(key, _)| format!("{{{{{key}}}}}"))
        .filter(|placeholder| !template.contains(placeholder))
        .collect();
    if !missing.is_empty() {
        bail!("prompt template is missing {}", missing.join(", "));
    }
    Ok(pairs
        .iter()
        .fold(template.to_owned(), |filled, (key, value)| {
            filled.replace(&format!("{{{{{key}}}}}"), value)
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALUES: TemplateValues<'static> = TemplateValues {
        game_name: "Alpha",
        provider: "Relax",
        game_type: "Slots",
        mode: "no_balance",
        mode_instructions: "Mode no_balance.\n",
        betsson_url: "https://www.betsson.es/casino",
        output_dir: "/tmp/out",
        assets_dir: "/tmp/assets",
        session_name: "betsson-alpha",
        data_prefix: "/data/assets/alpha",
        check_script: "testson check-script",
    };

    #[test]
    fn accepts_betsson_hosts_only() {
        assert!(parse_betsson_url("https://www.betsson.es/casino").is_ok());
        assert!(parse_betsson_url("https://WWW.BETSSON.ES/casino").is_ok());
        assert!(parse_betsson_url("https://betsson.es").is_ok());
        assert!(parse_betsson_url("http://www.betsson.es/casino").is_err());
        assert!(parse_betsson_url("https://notbetsson.com").is_err());
        assert!(parse_betsson_url("https://betsson.example.com").is_err());
        assert!(parse_betsson_url("https://www.betsson.co.uk").is_err());
    }

    #[test]
    fn slugifies_names() {
        assert_eq!(slugify("Los Piratas Golfetes"), "los-piratas-golfetes");
        assert_eq!(slugify("  Señor Slot  "), "se-or-slot");
        assert_eq!(slugify("!!!"), "game");
        assert_eq!(slugify(&"a".repeat(90)).chars().count(), 80);
    }

    #[test]
    fn a_session_needs_a_betsson_login() {
        let state = |origin: &str, name: &str, value: &str| {
            serde_json::json!({
                "cookies": [],
                "origins": [{ "origin": origin, "localStorage": [{ "name": name, "value": value }] }],
            })
        };
        assert!(session_has_auth(&state(
            "https://www.betsson.es",
            "authSession",
            "t"
        )));
        assert!(!session_has_auth(&state(
            "https://www.betsson.es",
            "authSession",
            ""
        )));
        assert!(!session_has_auth(&state(
            "https://www.betsson.es",
            "theme",
            "dark"
        )));
        assert!(!session_has_auth(&state(
            "https://example.com",
            "session",
            "t"
        )));
        assert!(!session_has_auth(&state(
            "http://www.betsson.es",
            "session",
            "t"
        )));
        assert!(!session_has_auth(&serde_json::json!({ "cookies": [] })));
    }

    #[test]
    fn fill_requires_every_placeholder() {
        let filled = fill_template(
            "{{GAME_NAME}}|{{PROVIDER}}|{{GAME_TYPE}}|{{MODE}}|{{MODE_INSTRUCTIONS}}|{{BETSSON_URL}}|{{OUTPUT_DIR}}|{{ASSETS_DIR}}|{{SESSION_NAME}}|{{DATA_PREFIX}}|{{CHECK_SCRIPT}}|{{SCHEMA}}|{{SCRIPT_SCHEMA}}",
            VALUES,
        )
        .unwrap();
        assert!(filled.starts_with(
            "Alpha|Relax|Slots|no_balance|Mode no_balance.|https://www.betsson.es/casino|/tmp/out|/tmp/assets|betsson-alpha|/data/assets/alpha|testson check-script|{"
        ));
        let error = fill_template("{{GAME_NAME}}", VALUES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("{{BETSSON_URL}}"));
        assert!(error.contains("{{SCHEMA}}"));
    }

    #[test]
    fn shipped_template_has_every_placeholder() {
        for instructions in [REAL_MONEY_INSTRUCTIONS, NO_BALANCE_INSTRUCTIONS] {
            let values = TemplateValues {
                mode_instructions: instructions,
                ..VALUES
            };
            let filled = fill_template(TEMPLATE, values).unwrap();
            assert!(!filled.contains("{{"));
            assert!(filled.contains("\"schema_version\""));
            assert!(filled.contains("\"script_version\""));
            assert!(filled.contains(instructions.trim_end()));
        }
    }

    /// Runs collect-messages.js with node against a page whose frame received `messages`.
    fn collect(messages: &serde_json::Value) -> String {
        let script = format!(
            "const collect = {COLLECT_MESSAGES};\n\
             const received = {messages};\n\
             const frame = {{\n  \
               evaluate: async () => received.map((data, i) => ({{ at: String(i), origin: 'o', data }})),\n  \
               url: () => 'https://game.example/index.html',\n\
             }};\n\
             collect({{ frames: () => [frame], mainFrame: () => frame }})\n  \
               .then((collected) => console.log(JSON.stringify(collected.map((m) => m.data))));\n"
        );
        let output = std::process::Command::new("node")
            .args(["-e", &script])
            .output()
            .expect("node runs, as playwright-cli needs it");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    #[test]
    fn tokens_are_redacted_in_messages_sent_as_json_text() {
        let collected = collect(&serde_json::json!([
            r#"{"type":"init","token":"t-1","nested":{"sessionId":"s-2","ok":1}}"#,
            { "text": r#"{"jwt":"j-3","value":2}"#, "authKey": "k-4" },
            "plain text",
            "[1, 2",
        ]));
        for secret in ["t-1", "s-2", "j-3", "k-4"] {
            assert!(!collected.contains(secret), "{secret} leaked: {collected}");
        }
        assert!(collected.contains(r#"\"ok\":1"#), "{collected}");
        assert!(collected.contains("plain text") && collected.contains("[1, 2"));
    }
}
