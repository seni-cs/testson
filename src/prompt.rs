//! Prompt filling, session restore, and Betsson URL checks.

use anyhow::{Result, bail};
use url::Url;

pub const TEMPLATE: &str = include_str!("../prompts/test-game.txt");
pub const REPORT_SCHEMA: &str = include_str!("../schema/test-game.schema.json");

const SLUG_LIMIT: usize = 80;

/// Values substituted into the prompt template.
#[derive(Debug, Clone, Copy)]
pub struct TemplateValues<'a> {
    pub game_name: &'a str,
    pub betsson_url: &'a str,
    /// Private run directory: the restore script and whatever the agent works with.
    pub output_dir: &'a str,
    /// Directory the dashboard serves, where the video and screenshots go.
    pub assets_dir: &'a str,
    pub session_name: &'a str,
    /// URL path `assets_dir` is served at, the prefix of video and screenshot paths.
    pub data_prefix: &'a str,
}

/// Accepts an `https` URL whose registrable label is `betsson`. Used as a clap value parser.
///
/// # Errors
///
/// Fails when the URL is not `https` on a `betsson` host.
pub fn parse_betsson_url(url: &str) -> Result<String, String> {
    let valid = Url::parse(url).is_ok_and(|parsed| {
        let host = parsed.host_str().unwrap_or("").to_lowercase();
        let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
        parsed.scheme() == "https" && labels.len() >= 2 && labels[labels.len() - 2] == "betsson"
    });
    if valid {
        Ok(url.to_owned())
    } else {
        Err("must be https on a betsson host, for example https://www.betsson.es/casino".into())
    }
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

/// Whether a non-comment line still contains a `REPLACE_WITH_` placeholder.
#[must_use]
pub fn session_has_placeholder(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        let comment = trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#');
        !comment && line.contains("REPLACE_WITH_")
    })
}

/// Builds the `restore-session.js` body from a session file. A missing file is an empty one, and
/// the script then reports `hasAuth: false`.
///
/// Blank lines and `#` comments are dropped. `//` comments stay, because they are valid
/// JavaScript inside `page.evaluate`.
#[must_use]
pub fn restore_script(session: &str) -> String {
    let statements: Vec<&str> = session
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .collect();
    let body = if statements.is_empty() {
        "/* session-block.txt had no statements */".to_owned()
    } else {
        statements.join("\n")
    };
    format!(
        "async (page) => {{\n  \
         await page.evaluate(() => {{\n\
         {body}\n  \
         }});\n  \
         return await page.evaluate(() => ({{\n    \
         customerId: localStorage.getItem('customerId'),\n    \
         hasAuth: !!(localStorage.getItem('session') || localStorage.getItem('authSession') || localStorage.getItem('authReducer'))\n  \
         }}));\n\
         }}\n"
    )
}

/// Replaces every `{{PLACEHOLDER}}` in `template` and fails when one is absent.
///
/// # Errors
///
/// Fails when the template lacks one of the placeholders.
pub fn fill_template(template: &str, values: TemplateValues<'_>) -> Result<String> {
    let pairs = [
        ("GAME_NAME", values.game_name),
        ("BETSSON_URL", values.betsson_url),
        ("OUTPUT_DIR", values.output_dir),
        ("ASSETS_DIR", values.assets_dir),
        ("SESSION_NAME", values.session_name),
        ("DATA_PREFIX", values.data_prefix),
        ("SCHEMA", REPORT_SCHEMA.trim_end_matches('\n')),
    ];
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
        betsson_url: "https://www.betsson.es/casino",
        output_dir: "/tmp/out",
        assets_dir: "/tmp/assets",
        session_name: "betsson-alpha",
        data_prefix: "/data/assets/alpha",
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
    fn placeholders_ignore_comments() {
        let commented =
            "// REPLACE_WITH_TOKEN\n# REPLACE_WITH_TOKEN\n\nlocalStorage.setItem('a', 'b');\n";
        assert!(!session_has_placeholder(commented));
        assert!(session_has_placeholder(
            "localStorage.setItem('a', 'REPLACE_WITH_SESSION');\n"
        ));
    }

    #[test]
    fn restore_script_keeps_javascript_comments() {
        let session = "# drop\nlocalStorage.setItem('customerId', '1');\n\n// keep\n";
        let script = restore_script(session);
        assert!(script.contains("localStorage.setItem('customerId', '1');"));
        assert!(script.contains("// keep"));
        assert!(!script.contains("# drop"));
        assert!(script.starts_with("async (page) => {\n"));
        assert!(restore_script("# only\n").contains("no statements"));
    }

    #[test]
    fn fill_requires_every_placeholder() {
        let filled = fill_template(
            "{{GAME_NAME}}|{{BETSSON_URL}}|{{OUTPUT_DIR}}|{{ASSETS_DIR}}|{{SESSION_NAME}}|{{DATA_PREFIX}}|{{SCHEMA}}",
            VALUES,
        )
        .unwrap();
        assert!(filled.starts_with(
            "Alpha|https://www.betsson.es/casino|/tmp/out|/tmp/assets|betsson-alpha|/data/assets/alpha|{"
        ));
        let error = fill_template("{{GAME_NAME}}", VALUES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("{{BETSSON_URL}}"));
        assert!(error.contains("{{SCHEMA}}"));
    }

    #[test]
    fn shipped_template_has_every_placeholder() {
        let filled = fill_template(TEMPLATE, VALUES).unwrap();
        assert!(!filled.contains("{{"));
        assert!(filled.contains("\"schema_version\""));
    }
}
