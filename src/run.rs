//! One demo-only game test: run directories, prompt, agent invocation, and the stored test.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::catalog::Game;
use crate::db;
use crate::harness::{Agent, on_path};
use crate::prompt::{
    MISSING_SESSION, TEMPLATE, TemplateValues, fill_template, restore_script,
    session_has_placeholder, slugify,
};
use crate::router::{ASSETS_DIR, ASSETS_URL};

/// The saved login, in the data directory.
pub const SESSION_FILE: &str = "session-block.txt";

/// Private run directories, beside the assets the dashboard serves. They hold the session.
const RUNS_DIR: &str = "runs";

/// The statuses `schema/test-game.schema.json` allows a test.
const STATUSES: [&str; 3] = ["working", "failing", "test_failed"];

/// The run directories, written before the agent starts.
#[derive(Debug)]
pub struct Prepared {
    /// Private: the filled prompt, the restore script, the model log, and the report.
    pub dir: PathBuf,
    /// Served by the dashboard: the video, the trace, and the screenshots.
    pub assets_dir: PathBuf,
    /// URL path `assets_dir` is served at.
    pub data_prefix: String,
    pub url: String,
    pub prompt: String,
    pub session_missing: bool,
}

/// A finished run.
#[derive(Debug)]
pub struct Outcome {
    pub report_path: PathBuf,
    /// The JSON document of the test, as stored.
    pub document: Value,
    pub agent_exit_code: i32,
}

impl Outcome {
    fn text(&self, key: &str) -> &str {
        self.document.get(key).and_then(Value::as_str).unwrap_or("")
    }

    /// `working`, `failing`, or `test_failed`.
    #[must_use]
    pub fn status(&self) -> &str {
        self.text("status")
    }

    /// The game launched in demo mode and a round completed.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.agent_exit_code == 0 && self.status() == "working"
    }

    /// The test reached a verdict on the game. `test_failed` means the test itself broke and is
    /// worth retrying.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.agent_exit_code == 0 && matches!(self.status(), "working" | "failing")
    }
}

/// Checks the tools, pings the model, runs the full test, stores it, and prints a summary.
///
/// The game must already be stored, since its tests refer to it.
///
/// # Errors
///
/// Fails when a tool is missing, the ping fails, the run directories cannot be written, the
/// agent cannot start, or the test cannot be stored.
pub fn test_game(data_dir: &Path, game: &Game, url: &str, agent: &Agent) -> Result<Outcome> {
    if !on_path("playwright-cli") {
        bail!(
            "playwright-cli is not on PATH.\nInstall it with: npm install -g @playwright/cli@latest \
             && playwright-cli install-browser chromium"
        );
    }
    agent.ensure_installed()?;
    println!("Pinging {agent}...");
    agent.ping()?;
    let prepared = prepare(data_dir, &game.name, url)?;
    if prepared.session_missing {
        eprintln!(
            "warning: {} was not found. The agent will run without a restored login.",
            data_dir.join(SESSION_FILE).display()
        );
    }
    println!("Game:   {} ({})", game.name, game.id);
    println!("URL:    {url}");
    println!("Agent:  {agent}");
    println!("Output: {}", prepared.dir.display());
    let outcome = execute(&prepared, agent)?;
    let test_id = db::insert_test(data_dir, &game.id, &outcome.document.to_string())
        .with_context(|| format!("could not store the test of {}", game.name))?;
    println!("Status: {}", outcome.status());
    println!("Summary: {}", outcome.text("summary"));
    println!("Video:  {}", outcome.text("video_path"));
    println!("Trace:  {}", outcome.text("trace_path"));
    println!("Report: {}", outcome.report_path.display());
    println!(
        "Dashboard: http://{}/game/{}#test-{test_id}",
        crate::ADDRESS,
        game.id
    );
    if outcome.agent_exit_code != 0 {
        eprintln!(
            "error: {} exited with status {}. The report above is what it returned.",
            agent.harness, outcome.agent_exit_code
        );
    }
    Ok(outcome)
}

/// Fills the prompt, and writes `runs/<slug>-<timestamp>/` and `assets/<slug>-<timestamp>/`
/// under `data_dir`.
///
/// # Errors
///
/// Fails when the session file still has placeholders or a run directory cannot be written.
pub fn prepare(data_dir: &Path, game_name: &str, url: &str) -> Result<Prepared> {
    let (session, session_missing) = load_session(&data_dir.join(SESSION_FILE))?;
    let slug = slugify(game_name);
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let assets_root = data_dir.join(ASSETS_DIR);
    let dir = create_run_dir(
        &data_dir.join(RUNS_DIR),
        &assets_root,
        &format!("{slug}-{stamp}"),
    )?;
    let folder = dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("run directory name is not valid UTF-8")?
        .to_owned();
    let assets_dir = assets_root.join(&folder);
    fs::create_dir_all(assets_dir.join("screenshots"))
        .with_context(|| format!("could not create {}", assets_dir.display()))?;
    let data_prefix = format!("{ASSETS_URL}/{folder}");
    let prompt = fill_template(
        TEMPLATE,
        TemplateValues {
            game_name,
            betsson_url: url,
            output_dir: absolute(&dir)?
                .to_str()
                .context("run directory is not valid UTF-8")?,
            assets_dir: absolute(&assets_dir)?
                .to_str()
                .context("assets directory is not valid UTF-8")?,
            session_name: &format!("betsson-{slug}-{stamp}"),
            data_prefix: &data_prefix,
            session_block: &session,
        },
    )?;
    write_private(&dir.join("restore-session.js"), &restore_script(&session))?;
    write_private(&dir.join("prompt.filled.txt"), &prompt)?;
    Ok(Prepared {
        dir,
        assets_dir,
        data_prefix,
        url: url.to_owned(),
        prompt,
        session_missing,
    })
}

/// Runs the agent in the run directory and writes `report.json`, the document to store.
///
/// A non-zero agent status still returns the report the agent produced. When the agent returns
/// no report, or one the dashboard cannot show, the document is a `test_failed` test that says
/// why, so the run still shows up on the dashboard.
///
/// # Errors
///
/// Fails when the agent cannot start or a file cannot be written.
pub fn execute(prepared: &Prepared, agent: &Agent) -> Result<Outcome> {
    let log_path = prepared.dir.join("model.log");
    let report_path = prepared.dir.join("report.json");
    let started_at = Utc::now();
    let clock = Instant::now();
    let mut log = create_private(&log_path)?;
    writeln!(log, "# agent: {agent}")?;
    let transcript = agent.run(&prepared.prompt, &prepared.dir, &mut log)?;
    drop(log);
    let log_text = fs::read_to_string(&log_path).unwrap_or_default();
    let report = transcript
        .last_message
        .as_deref()
        .and_then(|message| extract_report(message).ok())
        .map_or_else(
            || extract_report(after_echoed_prompt(&log_text, &prepared.prompt)),
            Ok,
        )
        .and_then(|report| check_report(&report).map(|()| report));
    let document = report.unwrap_or_else(|error| {
        eprintln!("error: {error}. See {}", log_path.display());
        let duration = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        test_failed(prepared, started_at, duration, &format!("{error}."))
    });
    write_private(
        &report_path,
        &(serde_json::to_string_pretty(&document)? + "\n"),
    )?;
    Ok(Outcome {
        report_path,
        document,
        agent_exit_code: transcript.exit_code,
    })
}

/// A test that broke before the agent could report on the game, with whatever evidence it left.
fn test_failed(prepared: &Prepared, started_at: DateTime<Utc>, duration: u64, why: &str) -> Value {
    let mut document = json!({
        "schema_version": 1,
        "status": "test_failed",
        "summary": "The agent did not return a report the dashboard can show, so the run says \
                    nothing about the game.",
        "game_url": prepared.url,
        "started_at": started_at.to_rfc3339_opts(SecondsFormat::Millis, true),
        "duration_ms": duration,
        "steps": [],
        "error": { "message": why },
    });
    for (key, file) in [("video_path", "session.webm"), ("trace_path", "trace.zip")] {
        if prepared.assets_dir.join(file).is_file() {
            document[key] = json!(format!("{}/{file}", prepared.data_prefix));
        }
    }
    document
}

/// Checks what the dashboard and the database rely on, short of the whole schema.
///
/// # Errors
///
/// Fails, naming the field, when the report does not hold it as the schema says.
fn check_report(report: &Value) -> Result<()> {
    let status = report.get("status").and_then(Value::as_str).unwrap_or("");
    if !STATUSES.contains(&status) {
        bail!(
            "the report status {status:?} is not one of {}",
            STATUSES.join(", ")
        );
    }
    if report
        .get("summary")
        .and_then(Value::as_str)
        .is_none_or(|summary| summary.trim().is_empty())
    {
        bail!("the report has no summary");
    }
    let started_at = report.get("started_at").and_then(Value::as_str);
    if started_at.is_none_or(|time| DateTime::parse_from_rfc3339(time).is_err()) {
        bail!("the report started_at is not an RFC 3339 date-time");
    }
    if !report.get("duration_ms").is_some_and(Value::is_u64) {
        bail!("the report duration_ms is not a whole number of milliseconds");
    }
    let steps = report.get("steps").and_then(Value::as_array);
    if !steps.is_some_and(|steps| steps.iter().all(|step| step.get("description").is_some())) {
        bail!("the report steps are not a list of steps with descriptions");
    }
    Ok(())
}

/// The part of `log` after the prompt, when the harness echoed it.
///
/// The prompt holds an example report, which must not be mistaken for the model's answer.
fn after_echoed_prompt<'a>(log: &'a str, prompt: &str) -> &'a str {
    let last_line = prompt.trim_end().lines().last().unwrap_or_default();
    match log.rfind(last_line) {
        Some(index) if !last_line.is_empty() => &log[index + last_line.len()..],
        _ => log,
    }
}

/// Returns the last JSON object in `output` that looks like a report.
///
/// # Errors
///
/// Fails when no object with `schema_version` and `status` is present.
pub fn extract_report(output: &str) -> Result<Value> {
    for (start, _) in output.match_indices('{').rev() {
        let mut deserializer = serde_json::Deserializer::from_str(&output[start..]);
        if let Ok(value) = Value::deserialize(&mut deserializer)
            && value.get("schema_version").is_some_and(Value::is_u64)
            && value.get("status").is_some_and(Value::is_string)
        {
            return Ok(value);
        }
    }
    bail!("model output did not contain a JSON report with schema_version")
}

fn load_session(path: &Path) -> Result<(String, bool)> {
    if !path.is_file() {
        return Ok((MISSING_SESSION.to_owned(), true));
    }
    set_mode(path, 0o600);
    let text =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    if session_has_placeholder(&text) {
        bail!(
            "{SESSION_FILE} still contains REPLACE_WITH_ placeholders.\n\
             Log in to Betsson, copy the localStorage values, and replace the placeholders.\n\
             The session expires. Update this file when demo mode starts asking you to log in."
        );
    }
    Ok((text, false))
}

/// Creates a private run directory named so that neither it nor its assets directory exists.
fn create_run_dir(runs: &Path, assets: &Path, name: &str) -> Result<PathBuf> {
    fs::create_dir_all(runs).with_context(|| format!("could not create {}", runs.display()))?;
    set_mode(runs, 0o700);
    let mut folder = name.to_owned();
    if runs.join(&folder).exists() || assets.join(&folder).exists() {
        folder = format!("{name}-{}", std::process::id());
    }
    let dir = runs.join(folder);
    fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    set_mode(&dir, 0o700);
    Ok(dir)
}

/// The agent runs in the run directory, so the paths it is given must not be relative.
fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("could not resolve {}", path.display()))
}

fn create_private(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options
        .open(path)
        .with_context(|| format!("could not write {}", path.display()))
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    create_private(path)?
        .write_all(contents.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_writes_a_private_prompt_and_restore_script() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(SESSION_FILE),
            "localStorage.setItem('customerId', '7');\n",
        )
        .unwrap();
        let prepared = prepare(
            dir.path(),
            "Los Piratas Golfetes",
            "https://www.betsson.es/casino",
        )
        .unwrap();
        assert!(!prepared.session_missing);
        assert!(prepared.dir.starts_with(dir.path().join(RUNS_DIR)));
        assert!(prepared.assets_dir.join("screenshots").is_dir());
        assert_eq!(
            prepared.assets_dir.file_name(),
            prepared.dir.file_name(),
            "the assets of a run are named after it"
        );
        assert!(
            prepared
                .data_prefix
                .starts_with("/data/assets/los-piratas-golfetes-")
        );
        assert!(prepared.prompt.contains("Game name: Los Piratas Golfetes"));
        // The session must never land where the dashboard serves files from.
        assert!(
            fs::read_dir(&prepared.assets_dir)
                .unwrap()
                .all(|entry| { entry.unwrap().file_name() == "screenshots" })
        );
        assert!(
            prepared
                .prompt
                .contains("localStorage.setItem('customerId', '7');")
        );
        let restore = fs::read_to_string(prepared.dir.join("restore-session.js")).unwrap();
        assert!(restore.contains("localStorage.setItem('customerId', '7');"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(prepared.dir.join("prompt.filled.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn prepare_rejects_placeholders_and_notes_a_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare(dir.path(), "Alpha", "https://www.betsson.es/casino").unwrap();
        assert!(prepared.session_missing);
        assert!(prepared.prompt.contains("session-block.txt was not found"));
        fs::write(
            dir.path().join(SESSION_FILE),
            "localStorage.setItem('session', 'REPLACE_WITH_SESSION');\n",
        )
        .unwrap();
        let error = prepare(dir.path(), "Alpha", "https://www.betsson.es/casino").unwrap_err();
        assert!(error.to_string().contains("REPLACE_WITH_"));
    }

    #[test]
    fn only_a_clean_pass_passes_and_only_test_failed_is_retried() {
        let outcome = |status: &str, code| Outcome {
            report_path: PathBuf::new(),
            document: json!({ "status": status }),
            agent_exit_code: code,
        };
        assert!(outcome("working", 0).passed());
        assert!(!outcome("working", 1).passed());
        assert!(!outcome("failing", 0).passed());
        assert!(outcome("failing", 0).finished());
        assert!(!outcome("test_failed", 0).finished());
        assert!(!outcome("working", 1).finished());
    }

    fn report() -> Value {
        json!({
            "schema_version": 1,
            "status": "working",
            "summary": "Played one demo round.",
            "started_at": "2026-10-12T10:00:00+02:00",
            "duration_ms": 1200,
            "steps": [{ "description": "Opened the lobby", "status": "passed" }],
        })
    }

    #[test]
    fn checks_what_the_dashboard_relies_on() {
        assert!(check_report(&report()).is_ok());
        for (key, value, problem) in [
            ("status", json!("passed"), "status"),
            ("summary", json!(" "), "summary"),
            ("started_at", json!("yesterday"), "started_at"),
            ("duration_ms", json!(-1), "duration_ms"),
            ("steps", json!([{ "status": "passed" }]), "steps"),
        ] {
            let mut broken = report();
            broken[key] = value;
            let error = check_report(&broken).unwrap_err().to_string();
            assert!(error.contains(problem), "{error}");
        }
    }

    #[test]
    fn a_run_without_a_report_is_stored_as_test_failed() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare(dir.path(), "Alpha", "https://www.betsson.es/casino").unwrap();
        fs::write(prepared.assets_dir.join("session.webm"), "").unwrap();
        let document = test_failed(&prepared, Utc::now(), 5, "no report.");
        check_report(&document).unwrap();
        assert_eq!(document["status"], "test_failed");
        assert_eq!(document["error"]["message"], "no report.");
        assert_eq!(
            document["video_path"],
            format!("{}/session.webm", prepared.data_prefix)
        );
        assert!(document.get("trace_path").is_none());
    }

    #[test]
    fn stores_a_report_for_the_dashboard() {
        let dir = tempfile::tempdir().unwrap();
        db::setup(dir.path()).unwrap();
        let game = Game {
            id: "alpha".into(),
            name: "Alpha".into(),
            provider: "P".into(),
            primary_type: "Slots".into(),
            is_available_for_fun: true,
            image_url: String::new(),
        };
        db::upsert_game(dir.path(), &game).unwrap();
        let id = db::insert_test(dir.path(), "alpha", &report().to_string()).unwrap();
        let stored = db::select_test_document(dir.path(), id).unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), report());
        assert!(db::insert_test(dir.path(), "missing", &report().to_string()).is_err());
    }

    #[test]
    fn keeps_the_last_report_object() {
        let log = r#"hello {"nope": true} {"schema_version": 1, "status": "failing"}
{"schema_version": 1, "status": "working", "video_path": "/data/assets/x/session.webm", "note": "a { brace"} trailing"#;
        let report = extract_report(log).unwrap();
        assert_eq!(report["status"], "working");
        assert_eq!(report["note"], "a { brace");
    }

    #[test]
    fn ignores_the_schema_echoed_from_the_prompt() {
        let echoed = format!("user\n{}\nthinking...", crate::prompt::REPORT_SCHEMA);
        assert!(extract_report(&echoed).is_err());
        assert!(extract_report("no json here {\"status\": \"working\"}").is_err());
    }

    #[test]
    fn skips_the_example_report_in_an_echoed_prompt() {
        let prompt = "Example: {\"schema_version\": 1, \"status\": \"working\"}\nReturn JSON.\n";
        let log = format!("user\n{prompt}\ncodex\nno report");
        assert!(extract_report(after_echoed_prompt(&log, prompt)).is_err());
        assert_eq!(after_echoed_prompt("plain log", prompt), "plain log");
    }
}
