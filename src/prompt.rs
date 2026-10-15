//! Prompt filling, session restore, and Betsson URL checks.

use anyhow::{Result, bail};
use url::Url;

pub const TEMPLATE: &str = include_str!("../prompts/test-game.txt");
pub const REPORT_SCHEMA: &str = include_str!("../schema/test-game.schema.json");

/// The `check-session.js` body. Run after the reload, it waits for the page to settle, samples
/// the site header until three samples agree, and returns `{ loggedIn: true | false | null }`,
/// so an expired session is not mistaken for a game that has no demo mode.
pub const CHECK_SESSION: &str = include_str!("../prompts/check-session.js");

const SLUG_LIMIT: usize = 80;

/// How long the restore script waits for the reloaded document.
const RELOAD_TIMEOUT_MS: u32 = 30_000;

/// What the restore script tells the agent when `session-block.txt` is absent.
pub const MISSING_SESSION: &str = "session-block.txt was not found. Do not invent a session. If \
     demo mode requires a login, return status \"test_failed\" and tell the operator to create \
     session-block.txt.";

/// Values substituted into the prompt template.
#[derive(Debug, Clone, Copy)]
pub struct TemplateValues<'a> {
    pub game_name: &'a str,
    pub betsson_url: &'a str,
    /// Private run directory: the restore script and whatever the agent works with.
    pub output_dir: &'a str,
    /// Directory the dashboard serves, where the video and the screenshots go. Never the trace,
    /// which records the session.
    pub assets_dir: &'a str,
    pub session_name: &'a str,
    /// Options of `playwright-cli open`: the browser, and whether it shows a window.
    pub open_options: &'a str,
    /// URL path `assets_dir` is served at, the prefix of video and screenshot paths.
    pub data_prefix: &'a str,
}

/// The hosts the login session may be injected into: the market the catalogue and
/// `session-block.txt` are for.
const BETSSON_HOSTS: [&str; 2] = ["www.betsson.es", "betsson.es"];

/// Characters that end the quoting of a value the prompt puts in a shell command, or run one:
/// the prompt quotes paths and the URL with `"` or `'`, and the agent may use any shell.
pub const SHELL_UNSAFE: [char; 6] = ['"', '\'', '$', '`', '\n', '\r'];

/// Accepts an `https` URL on one of [`BETSSON_HOSTS`], and returns it normalised. Used as a clap
/// value parser.
///
/// # Errors
///
/// Fails when the URL is not `https` on an allowed host, names a user or a port, or keeps a
/// character of [`SHELL_UNSAFE`] or a backslash after normalising.
pub fn parse_betsson_url(url: &str) -> Result<String, String> {
    let parsed = Url::parse(url)
        .ok()
        .filter(|parsed| {
            parsed.scheme() == "https"
                && parsed
                    .host_str()
                    .is_some_and(|host| BETSSON_HOSTS.contains(&host))
                && parsed.username().is_empty()
                && parsed.password().is_none()
                && parsed.port().is_none()
        })
        .ok_or_else(|| {
            format!(
                "must be https on {}, for example https://www.betsson.es/casino",
                BETSSON_HOSTS.join(" or ")
            )
        })?;
    // Normalising percent-encodes quotes and backticks and drops line breaks, but keeps `$` and
    // `'`, and a backslash in the query or fragment, which would expand inside or end the quoting of
    // `open "<url>"`. `%` stays allowed: it is literal inside quotes in bash and PowerShell, and
    // the prompt never runs these commands through cmd.
    let normalised = parsed.as_str();
    if let Some(character) = normalised
        .chars()
        .find(|c| SHELL_UNSAFE.contains(c) || *c == '\\')
    {
        return Err(format!(
            "contains {character:?}, which cannot be quoted in the agent's shell commands"
        ));
    }
    Ok(normalised.to_owned())
}

/// Builds a filesystem slug of at most 80 characters. An empty name becomes `game`.
///
/// Common accented Latin letters become their ASCII letters. When a letter or digit of another
/// script is dropped, or a name of symbols such as emoji leaves nothing, eight hex digits of a
/// hash of the name are appended, so that `Золото` and `Серебро` do not share an identifier.
/// Slugs of plain ASCII names have no hash.
#[must_use]
pub fn slugify(name: &str) -> String {
    let lowercase = name.trim().to_lowercase();
    let mut slug = String::new();
    let mut dash = false;
    let mut dropped_letter = false;
    let mut dropped_other = false;
    let mut buffer = [0; 4];
    for character in lowercase.chars() {
        if is_combining_accent(character) {
            continue;
        }
        let letters = if character.is_ascii_alphanumeric() {
            Some(&*character.encode_utf8(&mut buffer))
        } else {
            latin_to_ascii(character)
        };
        let Some(letters) = letters else {
            dropped_letter |= character.is_alphanumeric();
            dropped_other |= !character.is_ascii();
            dash = true;
            continue;
        };
        if dash && !slug.is_empty() {
            slug.push('-');
        }
        dash = false;
        slug.push_str(letters);
    }
    let hash = (dropped_letter || (dropped_other && slug.is_empty()))
        .then(|| format!("-{:08x}", fnv1a(lowercase.as_bytes())));
    if slug.is_empty() {
        slug.push_str("game");
    }
    // The slug is ASCII, so truncating bytes keeps whole characters.
    slug.truncate(SLUG_LIMIT - hash.as_ref().map_or(0, String::len));
    let mut slug = slug.trim_end_matches('-').to_owned();
    slug.push_str(hash.as_deref().unwrap_or_default());
    slug
}

/// The 32-bit FNV-1a hash: small, stable across builds and platforms, and dependency-free.
fn fnv1a(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0x811c_9dc5, |hash, &byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

/// The ASCII letters of a lowercase accented Latin letter, without adding a dependency.
fn latin_to_ascii(character: char) -> Option<&'static str> {
    Some(match character {
        'á' | 'à' | 'ä' | 'â' | 'ã' | 'å' | 'ā' | 'ă' | 'ą' => "a",
        'ć' | 'č' | 'ç' => "c",
        'ď' | 'đ' | 'ð' => "d",
        'é' | 'è' | 'ë' | 'ê' | 'ē' | 'ė' | 'ę' | 'ě' => "e",
        'ğ' | 'ģ' => "g",
        'í' | 'ì' | 'ï' | 'î' | 'ī' | 'į' | 'ı' => "i",
        'ķ' => "k",
        'ł' | 'ļ' | 'ľ' | 'ĺ' => "l",
        'ñ' | 'ń' | 'ň' | 'ņ' => "n",
        'ó' | 'ò' | 'ö' | 'ô' | 'õ' | 'ø' | 'ō' | 'ő' => "o",
        'ř' | 'ŕ' => "r",
        'ś' | 'š' | 'ş' | 'ș' => "s",
        'ť' | 'ţ' | 'ț' => "t",
        'ú' | 'ù' | 'ü' | 'û' | 'ū' | 'ů' | 'ű' | 'ų' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        'þ' => "th",
        _ => return None,
    })
}

/// A combining accent, as in a decomposed `n` + `◌̃`, or the dot `İ` lowercases to. It belongs
/// to the letter before it.
fn is_combining_accent(character: char) -> bool {
    ('\u{300}'..='\u{36f}').contains(&character)
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

/// Builds `session.js`, the private init script that restores the login, from a session file.
///
/// Blank lines and `#` comments are dropped. `//` comments stay, because they are valid
/// JavaScript. The statements run in the top frame only, so the frames of other origins the page
/// embeds never receive the login.
#[must_use]
pub fn session_script(session: &str) -> String {
    let lines = session_lines(session);
    let body = if lines.is_empty() {
        "/* session-block.txt had no statements */".to_owned()
    } else {
        lines.join("\n")
    };
    format!("if (window.top === window) {{\n{body}\n}}\n")
}

/// How many statements of a session file [`session_script`] restores: its lines, less blank
/// lines and comments.
#[must_use]
pub fn session_statements(session: &str) -> usize {
    session_lines(session)
        .iter()
        .filter(|line| !line.trim_start().starts_with("//"))
        .count()
}

/// The lines of a session file that go into [`session_script`]: all but blank lines and `#`
/// comments.
fn session_lines(session: &str) -> Vec<&str> {
    session
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .collect()
}

/// Builds `restore-session.js`, which the agent runs with `playwright-cli run-code --filename`.
///
/// playwright-cli prints the code it runs, so this script holds no token. It loads
/// `session_file`, the [`session_script`] output, as an init script, reloads so the page starts
/// with the login, and removes the init script again. It returns `restoredStatements`, the
/// [`session_statements`] of the file, and never whether the login is valid: reading back the keys
/// it has just written would make an expired session look the same as a live one, so
/// [`CHECK_SESSION`] answers that from the page. `None` means `session-block.txt` was not found:
/// the script then says so, with `restoredStatements: 0`, and leaves the page alone.
///
/// playwright-cli returns from `run-code` as soon as the page shows a dialog, without the
/// result. So while the page loads, `alert`, `confirm`, and `prompt` answer as if dismissed
/// without showing anything, and a dialog that still shows is answered by the script: a
/// `beforeunload` prompt is accepted so the reload goes on, any other dialog is dismissed. The
/// reload waits for the document only, with a timeout, so a page that never finishes loading
/// makes the restore fail instead of hang.
#[must_use]
pub fn restore_script(session_file: Option<&str>, statements: usize) -> String {
    let Some(session_file) = session_file else {
        let message = serde_json::Value::from(MISSING_SESSION);
        return format!(
            "async (page) => ({{\n  session: 'missing',\n  restoredStatements: 0,\n  \
             message: {message}\n}})\n"
        );
    };
    let path = serde_json::Value::from(session_file);
    format!(
        "async (page) => {{\n  \
         const answer = (dialog) =>\n    \
         (dialog.type() === 'beforeunload' ? dialog.accept() : dialog.dismiss()).catch(() => {{}});\n  \
         page.on('dialog', answer);\n  \
         const quiet = await page.addInitScript(() => {{\n    \
         const shown = {{ alert: window.alert, confirm: window.confirm, prompt: window.prompt }};\n    \
         window.alert = () => undefined;\n    \
         window.confirm = () => false;\n    \
         window.prompt = () => null;\n    \
         window.addEventListener('load', () => Object.assign(window, shown), {{ once: true }});\n  \
         }});\n  \
         const restore = await page.addInitScript({{ path: {path} }});\n  \
         try {{\n    \
         await page.reload({{ waitUntil: 'domcontentloaded', timeout: {RELOAD_TIMEOUT_MS} }});\n  \
         }} finally {{\n    \
         await restore.dispose();\n    \
         await quiet.dispose();\n    \
         page.off('dialog', answer);\n  \
         }}\n  \
         return {{ restoredStatements: {statements} }};\n\
         }}\n"
    )
}

/// Builds the `stop-trace.js` body, which stops the trace and saves it as an archive at
/// `trace_file`.
///
/// `playwright-cli tracing-stop` leaves only the raw, unarchived trace files, so the archive is
/// written through `run-code` instead. The path is a JSON string, which is also a valid
/// JavaScript string, so the backslashes of a Windows path survive.
#[must_use]
pub fn stop_trace_script(trace_file: &str) -> String {
    let path = serde_json::Value::from(trace_file);
    format!(
        "async (page) => {{\n  \
         await page.context().tracing.stop({{ path: {path} }});\n  \
         return 'Trace saved in the private run directory.';\n\
         }}\n"
    )
}

/// Replaces every `{{PLACEHOLDER}}` in `template` and fails when one is absent.
///
/// The template is read once, and each value is copied as it is, so a value that holds a
/// placeholder, such as a game named `{{SCHEMA}}`, is never expanded. A `{{` that does not
/// start an uppercase placeholder is copied too.
///
/// # Errors
///
/// Fails when the template lacks one of the placeholders, or has one that is not known.
pub fn fill_template(template: &str, values: TemplateValues<'_>) -> Result<String> {
    let pairs = [
        ("GAME_NAME", values.game_name),
        ("BETSSON_URL", values.betsson_url),
        ("OUTPUT_DIR", values.output_dir),
        ("ASSETS_DIR", values.assets_dir),
        ("SESSION_NAME", values.session_name),
        ("OPEN_OPTIONS", values.open_options),
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
    let mut filled = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        filled.push_str(&rest[..start]);
        let inner = &rest[start + 2..];
        let key = inner
            .find("}}")
            .map(|end| &inner[..end])
            .filter(|key| is_placeholder_key(key));
        let Some(key) = key else {
            // Not a placeholder: keep one brace and look again from the next one.
            filled.push('{');
            rest = &rest[start + 1..];
            continue;
        };
        let Some((_, value)) = pairs.iter().find(|(name, _)| *name == key) else {
            bail!("prompt template has an unknown placeholder {{{{{key}}}}}");
        };
        filled.push_str(value);
        rest = &inner[key.len() + 2..];
    }
    filled.push_str(rest);
    Ok(filled)
}

/// Whether `key` has the shape of a placeholder name: uppercase letters and underscores.
fn is_placeholder_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte == b'_')
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
        open_options: "--browser=chromium --headed",
        data_prefix: "/data/assets/alpha",
    };

    #[test]
    fn accepts_betsson_hosts_only() {
        assert!(parse_betsson_url("https://www.betsson.es/casino").is_ok());
        assert_eq!(
            parse_betsson_url("https://WWW.BETSSON.ES/casino").unwrap(),
            "https://www.betsson.es/casino"
        );
        assert!(parse_betsson_url("https://betsson.es").is_ok());
        assert!(parse_betsson_url("http://www.betsson.es/casino").is_err());
        assert!(parse_betsson_url("https://notbetsson.com").is_err());
        assert!(parse_betsson_url("https://betsson.example.com").is_err());
        assert!(parse_betsson_url("https://www.betsson.co.uk").is_err());
        assert!(parse_betsson_url("https://betsson.xyz").is_err());
        assert!(parse_betsson_url("https://www.betsson.evil").is_err());
        assert!(parse_betsson_url("https://casino.betsson.es").is_err());
        assert!(parse_betsson_url("https://www.betsson.es.evil.com").is_err());
        assert!(parse_betsson_url("https://www.betsson.es@evil.com").is_err());
        assert!(parse_betsson_url("https://www.betsson.es:8443/casino").is_err());
        let error = parse_betsson_url("https://betsson.xyz").unwrap_err();
        assert!(error.contains("www.betsson.es"), "{error}");
    }

    #[test]
    fn rejects_urls_that_break_shell_quoting() {
        for url in [
            "https://www.betsson.es/$(id)",
            "https://www.betsson.es/casino#it's",
            "https://www.betsson.es/casino#$HOME",
            "https://www.betsson.es/casino?x=\\",
            "https://www.betsson.es/casino#a\\b",
        ] {
            let error = parse_betsson_url(url).unwrap_err();
            assert!(error.contains("cannot be quoted"), "{url}: {error}");
        }
        // Quotes, backticks, and line breaks are encoded or dropped, so they never reach a shell.
        let encoded = parse_betsson_url("https://www.betsson.es/\"`x`\"\n").unwrap();
        assert!(!encoded.contains(['"', '`', '\n']), "{encoded}");
        // A backslash in the path is a slash to the URL parser, and `%` is literal in quotes.
        assert_eq!(
            parse_betsson_url("https://www.betsson.es\\casino?q=a%20b").unwrap(),
            "https://www.betsson.es/casino?q=a%20b"
        );
    }

    #[test]
    fn slugifies_names() {
        assert_eq!(slugify("Los Piratas Golfetes"), "los-piratas-golfetes");
        assert_eq!(slugify("!!!"), "game");
        assert_eq!(slugify(""), "game");
        assert_eq!(slugify(&"a".repeat(90)).chars().count(), 80);
    }

    #[test]
    fn slugs_transliterate_latin_letters() {
        assert_eq!(slugify("  Señor Slot  "), "senor-slot");
        assert_eq!(
            slugify("ÁÀÄÂÃÅ éèëê ÍÌÏÎ óòöôõ ÚÙÜÛ"),
            "aaaaaa-eeee-iiii-ooooo-uuuu"
        );
        assert_eq!(slugify("Ñandú Çava"), "nandu-cava");
        assert_eq!(slugify("Straße Æsir Øre Œuvre"), "strasse-aesir-ore-oeuvre");
        // A decomposed accent is dropped rather than splitting the word.
        assert_eq!(slugify("Sen\u{303}or"), "senor");
        assert_eq!(
            slugify("Złoty Śnieg Żółć Źdźbło"),
            "zloty-snieg-zolc-zdzblo"
        );
        assert_eq!(slugify("Pąk Węża Ćma Koń"), "pak-weza-cma-kon");
        assert_eq!(
            slugify("Šťastný Čert Žák Řeka Ďábel Kůň Ěcho"),
            "stastny-cert-zak-reka-dabel-kun-echo"
        );
        assert_eq!(slugify("Őrült Űrhajó Ÿes"), "orult-urhajo-yes");
        assert_eq!(
            slugify("Ağaç Şeker Işık İstanbul"),
            "agac-seker-isik-istanbul"
        );
        assert_eq!(slugify("Đorđe Þór Ðoð Ålesund"), "dorde-thor-dod-alesund");
        assert_eq!(slugify("Țară Ăla Șah"), "tara-ala-sah");
        assert_eq!(
            slugify("Māra Ģirts Ķēde Ļauts Ņina Ėglė Įlanka Ųla"),
            "mara-girts-kede-lauts-nina-egle-ilanka-ula"
        );
    }

    #[test]
    fn slugs_of_other_scripts_get_a_stable_hash() {
        // Plain ASCII names and punctuation-only names are unchanged.
        assert_eq!(slugify("Book of Dead!"), "book-of-dead");
        assert_eq!(slugify("!!!"), "game");
        let gold = slugify("Золото");
        let silver = slugify("Серебро");
        let dragon = slugify("大三元");
        let slot = slugify("🎰");
        let cherry = slugify("🍒");
        for slug in [&gold, &silver, &dragon, &slot, &cherry] {
            let hash = slug
                .strip_prefix("game-")
                .unwrap_or_else(|| panic!("{slug}"));
            assert_eq!(hash.len(), 8, "{slug}");
            assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()), "{slug}");
        }
        assert_ne!(gold, silver);
        assert_ne!(slot, cherry);
        assert_eq!(
            gold,
            slugify("  ЗОЛОТО "),
            "the hash is stable and ignores case"
        );
        let mixed = slugify("Fortune 88 · 卡");
        assert!(
            mixed.starts_with("fortune-88-") && mixed.len() == 19,
            "{mixed}"
        );
        assert_ne!(mixed, slugify("Fortune 88 · 福"));
        // A name the table transliterates entirely needs no hash.
        assert_eq!(slugify("Złoty"), "zloty");
    }

    #[test]
    fn fnv1a_matches_the_reference_values() {
        assert_eq!(fnv1a(b""), 0x811c_9dc5);
        assert_eq!(fnv1a(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a(b"foobar"), 0xbf9c_f968);
    }

    #[test]
    fn slugs_do_not_end_in_a_dash_after_truncation() {
        let name = format!("{} {}", "a".repeat(79), "b".repeat(10));
        assert_eq!(slugify(&name), "a".repeat(79));
        let long = slugify(&format!("{} x", "ß".repeat(40)));
        assert_eq!(long, "ss".repeat(40));
        let hashed = slugify(&format!("{} {}", "a".repeat(70), "卡".repeat(5)));
        assert_eq!(hashed.len(), 79, "{hashed}");
        assert!(
            hashed.starts_with(&format!("{}-", "a".repeat(70))),
            "{hashed}"
        );
        let cut = slugify(&format!("{} b 卡", "a".repeat(70)));
        assert!(
            cut.starts_with(&format!("{}-", "a".repeat(70))) && !cut.contains("--"),
            "{cut}"
        );
        assert!(cut.len() <= 80, "{cut}");
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
    fn session_script_keeps_javascript_comments() {
        let session = "# drop\nlocalStorage.setItem('customerId', '1');\n\n// keep\n";
        let script = session_script(session);
        assert!(script.contains("localStorage.setItem('customerId', '1');"));
        assert!(script.contains("// keep\n}"));
        assert!(!script.contains("# drop"));
        assert!(script.starts_with("if (window.top === window) {\n"));
        assert!(session_script("# only\n").contains("no statements"));
    }

    #[test]
    fn restore_script_loads_the_session_by_path() {
        let script = restore_script(Some(r"C:\runs\alpha\session.js"), 2);
        assert!(script.starts_with("async (page) => {\n"));
        assert!(script.contains(r#"page.addInitScript({ path: "C:\\runs\\alpha\\session.js" })"#));
        assert!(
            script
                .contains("await page.reload({ waitUntil: 'domcontentloaded', timeout: 30000 });")
        );
        assert!(script.contains("await restore.dispose();"));
        // A dialog during the reload must not cut the result short or block the page.
        assert!(script.contains("page.on('dialog', answer);"));
        assert!(script.contains("page.off('dialog', answer);"));
        assert!(script.contains("window.alert = () => undefined;"));
        assert!(script.contains("await quiet.dispose();"));
        assert!(!script.contains("setItem"));
        let missing = restore_script(None, 0);
        assert!(missing.contains("session-block.txt was not found"));
        assert!(missing.contains(r#"\"test_failed\""#));
        assert!(!missing.contains("addInitScript"));
    }

    #[test]
    fn stop_trace_script_keeps_a_windows_path_intact() {
        let script = stop_trace_script(r"C:\data-dir\runs\alpha\trace.zip");
        assert!(script.starts_with("async (page) => {\n"));
        assert!(
            script.contains(r#"tracing.stop({ path: "C:\\data-dir\\runs\\alpha\\trace.zip" })"#)
        );
    }

    #[test]
    fn the_trace_stays_out_of_the_served_assets() {
        let filled = fill_template(TEMPLATE, VALUES).unwrap();
        assert!(filled.contains("run-code --filename '/tmp/out/stop-trace.js'"));
        // A trace records the restored session, so no line about it may send it to the assets.
        for line in filled
            .lines()
            .filter(|line| line.to_lowercase().contains("trace"))
        {
            assert!(!line.contains(VALUES.assets_dir), "{line}");
            assert!(!line.contains(VALUES.data_prefix), "{line}");
            assert!(!line.contains("/data/assets"), "{line}");
        }
    }

    #[test]
    fn restore_script_does_not_claim_a_login() {
        // Reading back the keys it has just written said nothing of an expired session.
        let session = "localStorage.setItem('session', 'x');\n\
                       localStorage.setItem('customerId', '1');\n// note\n";
        assert_eq!(session_statements(session), 2);
        let script = restore_script(Some("/runs/alpha/session.js"), session_statements(session));
        assert!(!script.contains("hasAuth"));
        assert!(!script.contains("getItem"));
        assert!(script.contains("return { restoredStatements: 2 };"));
        assert!(restore_script(None, 0).contains("restoredStatements: 0"));
    }

    #[test]
    fn check_session_reads_the_page_without_acting_on_it() {
        assert!(CHECK_SESSION.starts_with("async (page) => {"));
        assert!(CHECK_SESSION.contains("loggedIn"));
        assert!(CHECK_SESSION.contains("iniciar sesión"));
        assert!(CHECK_SESSION.contains("depositar"));
        // The logged-out header is read only once the page settles and three samples agree.
        assert!(CHECK_SESSION.contains("networkidle"));
        assert!(CHECK_SESSION.contains("agreeing >= 3"));
        // Generic words such as "Entrar" also label live tables and promotions.
        assert!(CHECK_SESSION.contains("getByRole('banner')"));
        assert!(!CHECK_SESSION.contains("entrar"));
        // A line comment would swallow the body if a shell joined the lines.
        assert!(
            !CHECK_SESSION
                .lines()
                .any(|line| line.trim_start().starts_with("//"))
        );
        for action in [
            ".click(",
            ".fill(",
            ".type(",
            ".press(",
            ".goto(",
            "localStorage",
        ] {
            assert!(
                !CHECK_SESSION.contains(action),
                "check-session.js uses {action}"
            );
        }
    }

    #[test]
    fn fill_requires_every_placeholder() {
        let filled = fill_template(
            "{{GAME_NAME}}|{{BETSSON_URL}}|{{OUTPUT_DIR}}|{{ASSETS_DIR}}|{{SESSION_NAME}}|{{OPEN_OPTIONS}}|{{DATA_PREFIX}}|{{SCHEMA}}",
            VALUES,
        )
        .unwrap();
        assert!(filled.starts_with(
            "Alpha|https://www.betsson.es/casino|/tmp/out|/tmp/assets|betsson-alpha|--browser=chromium --headed|/data/assets/alpha|{"
        ));
        let error = fill_template("{{GAME_NAME}}", VALUES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("{{BETSSON_URL}}"));
        assert!(error.contains("{{OPEN_OPTIONS}}"));
        assert!(error.contains("{{SCHEMA}}"));
    }

    const EVERY_PLACEHOLDER: &str = "{{GAME_NAME}}|{{BETSSON_URL}}|{{OUTPUT_DIR}}|{{ASSETS_DIR}}|\
                                     {{SESSION_NAME}}|{{OPEN_OPTIONS}}|{{DATA_PREFIX}}|{{SCHEMA}}";

    #[test]
    fn fill_copies_values_verbatim() {
        let values = TemplateValues {
            game_name: "Evil {{OPEN_OPTIONS}} {{SCHEMA}} {{OUTPUT_DIR}}",
            open_options: "secret();",
            ..VALUES
        };
        let filled = fill_template(EVERY_PLACEHOLDER, values).unwrap();
        assert!(
            filled.starts_with("Evil {{OPEN_OPTIONS}} {{SCHEMA}} {{OUTPUT_DIR}}|https://"),
            "{filled}"
        );
        assert_eq!(filled.matches("secret();").count(), 1);
        assert_eq!(
            filled.matches("\"$schema\"").count(),
            REPORT_SCHEMA.matches("\"$schema\"").count(),
            "the schema is inserted once"
        );
    }

    #[test]
    fn fill_refuses_unknown_placeholders_and_keeps_other_braces() {
        let error = fill_template(&format!("{EVERY_PLACEHOLDER}{{{{GAME_ID}}}}"), VALUES)
            .unwrap_err()
            .to_string();
        assert!(error.contains("{{GAME_ID}}"), "{error}");
        let filled = fill_template(
            &format!("{{{{{{GAME_NAME}}}}}} {{{{x}}}} {{{{ {EVERY_PLACEHOLDER}"),
            VALUES,
        )
        .unwrap();
        assert!(filled.starts_with("{Alpha} {{x}} {{ Alpha|"), "{filled}");
    }

    #[test]
    fn shipped_template_has_every_placeholder() {
        let filled = fill_template(TEMPLATE, VALUES).unwrap();
        assert!(!filled.contains("{{"));
        assert!(filled.contains("\"schema_version\""));
        assert!(
            filled.contains("open \"https://www.betsson.es/casino\" --browser=chromium --headed\n")
        );
        // testson chooses the window: the template itself never asks for one.
        let headless = fill_template(
            TEMPLATE,
            TemplateValues {
                open_options: "--browser=chromium",
                ..VALUES
            },
        )
        .unwrap();
        assert!(headless.contains("open \"https://www.betsson.es/casino\" --browser=chromium\n"));
    }

    #[test]
    fn shipped_template_checks_the_login_after_the_reload() {
        assert!(!TEMPLATE.contains("hasAuth"));
        // The restore script reloads the page itself.
        let restore = TEMPLATE
            .find("run-code --filename '{{OUTPUT_DIR}}/restore-session.js'")
            .unwrap();
        let check = TEMPLATE
            .find("run-code --filename '{{OUTPUT_DIR}}/check-session.js'")
            .expect("the template runs check-session.js");
        assert!(check > restore, "the login is checked after the reload");
        assert!(TEMPLATE.contains("could not positively confirm a logged-in state"));
        assert!(TEMPLATE.contains("run-code --filename '{{OUTPUT_DIR}}/restore-session.js'"));
        assert!(!TEMPLATE.contains("$(cat"));
        assert!(TEMPLATE.contains("Treat false like null"));
        assert!(TEMPLATE.contains("If the check script fails"));
    }

    #[test]
    fn shipped_template_runs_the_restore_script_from_its_file() {
        assert!(!TEMPLATE.contains("SESSION_BLOCK"));
        assert!(!TEMPLATE.contains("$(cat"));
        assert!(
            TEMPLATE.contains("run-code --filename '{{OUTPUT_DIR}}/restore-session.js'"),
            "playwright-cli must read the restore script from its file"
        );
    }

    /// The commands the template runs, each the first word after the session and global flags.
    fn template_commands() -> Vec<&'static str> {
        TEMPLATE
            .split("--session {{SESSION_NAME}} ")
            .skip(1)
            .filter_map(|rest| rest.split_whitespace().find(|word| !word.starts_with("--")))
            .map(|word| word.trim_end_matches('`'))
            .collect()
    }

    #[test]
    fn shipped_template_runs_only_existing_playwright_cli_commands() {
        // Checked against `playwright-cli --help` of version 0.1.22.
        let known = [
            "open",
            "close",
            "reload",
            "resize",
            "hover",
            "find",
            "eval",
            "run-code",
            "console",
            "requests",
            "tab-list",
            "tab-select",
            "video-start",
            "video-stop",
            "tracing-start",
            "tracing-stop",
        ];
        let commands = template_commands();
        assert!(commands.contains(&"requests"));
        for command in commands {
            assert!(known.contains(&command), "unknown command {command:?}");
        }
    }

    #[test]
    fn shipped_template_collects_and_clears_evidence_per_step() {
        for command in [
            "console warning",
            "requests",
            "console --clear",
            "requests --clear",
        ] {
            assert!(
                TEMPLATE.contains(&format!("--session {{{{SESSION_NAME}}}} {command}\n")),
                "missing {command:?}"
            );
        }
        assert!(TEMPLATE.contains("before every `goto`, `reload`"));
    }

    #[test]
    fn shipped_template_checks_canvas_clicks_land_in_the_game() {
        assert!(TEMPLATE.contains(
            ".contentFrame().locator('<inner iframe selector>').contentFrame().locator('canvas')"
        ));
        assert!(TEMPLATE.contains("document.elementFromPoint(<x>, <y>)"));
    }

    #[test]
    fn shipped_template_strips_queries_from_recorded_urls() {
        assert!(TEMPLATE.contains(
            "Strip the query string and the fragment from every URL you record in a failed \
             request or a console source_url: keep only the scheme, host, and path."
        ));
    }

    #[test]
    fn shipped_template_speaks_to_the_agent_only() {
        for text in [
            "Edit session-block.txt",
            "filled by `testson test`",
            "REPLACE_WITH_",
        ] {
            assert!(!TEMPLATE.contains(text), "{text:?}");
        }
    }

    #[test]
    fn example_report_has_no_values_to_copy() {
        let example = &TEMPLATE[TEMPLATE.find("Example shape").unwrap()..];
        for literal in [
            "\"duration_ms\": 0",
            "\"action\": \"goto\"",
            "\"browser\": \"chromium\"",
            "1440",
            "900",
        ] {
            assert!(!example.contains(literal), "{literal:?}");
        }
    }
}
