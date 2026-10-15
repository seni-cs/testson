//! One demo-only game test: run directories, prompt, agent invocation, and the stored test.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Datelike, Local, SecondsFormat, Utc};
use jsonschema::Validator;
use serde_json::{Value, json};

use crate::browser::{self, Window};
use crate::catalog::Game;
use crate::db;
use crate::harness::{self, Agent, Transcript};
use crate::prompt::{
    CHECK_SESSION, REPORT_SCHEMA, SHELL_UNSAFE, TEMPLATE, TemplateValues, fill_template,
    parse_betsson_url, restore_script, session_has_placeholder, session_script, session_statements,
    slugify, stop_trace_script,
};
use crate::router::{ASSETS_DIR, ASSETS_URL, serves_extension};

/// The saved login, in the data directory.
pub const SESSION_FILE: &str = "session-block.txt";

/// The login as an init script, in the private run directory. Only the restore script refers to
/// it, by path, so it never reaches the prompt or the output of playwright-cli.
const SESSION_SCRIPT: &str = "session.js";

/// Private run directories, beside the assets the dashboard serves. They hold the session.
const RUNS_DIR: &str = "runs";

/// How long closing the browser of a run may take.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(60);

/// `schema/test-game.schema.json`, with its formats checked too.
static REPORT_VALIDATOR: LazyLock<Validator> = LazyLock::new(|| {
    let schema = serde_json::from_str(REPORT_SCHEMA).expect("the report schema is valid JSON");
    jsonschema::draft202012::options()
        .should_validate_formats(true)
        .build(&schema)
        .expect("the report schema is a valid JSON Schema")
});

/// The Playwright trace archive, in the private run directory. It records the restored session,
/// so it must never be served.
const TRACE_FILE: &str = "trace.zip";

/// Directory of a run that takes what the agent left in the assets and must not be served.
const QUARANTINE_DIR: &str = "from-assets";

/// Characters of a harness error kept in a `test_failed` test. The model log keeps all of it.
const ERROR_PREVIEW: usize = 300;

/// How many names `claim_run_dirs` tries for the runs of a game that start in the same second.
const MAX_CLAIMS: u32 = 1000;

/// The `error.reason` the schema gives a test stopped by a missing or expired login session.
const SESSION_REASON: &str = "session";

/// The run directories, written before the agent starts.
#[derive(Debug)]
pub struct Prepared {
    /// Private: the filled prompt, the session, restore, login-check and stop-trace scripts, the
    /// model log, the trace, and the report.
    pub dir: PathBuf,
    /// Served by the dashboard: the video and the screenshots. Never the trace.
    pub assets_dir: PathBuf,
    /// URL path `assets_dir` is served at.
    pub data_prefix: String,
    pub url: String,
    /// The `playwright-cli` session the agent opens the browser in.
    pub session_name: String,
    pub prompt: String,
}

/// The login a run restores, read from `session-block.txt`.
#[derive(Debug)]
pub struct Session {
    /// The `localStorage` lines, or `None` when the file was not found and the agent runs
    /// without a restored login.
    text: Option<String>,
}

impl Session {
    /// Reads `session-block.txt` in `data_dir`. A missing file is not an error: the run goes on
    /// without a login.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or still has `REPLACE_WITH_` placeholders.
    pub fn load(data_dir: &Path) -> Result<Self> {
        Ok(Self {
            text: load_session(&data_dir.join(SESSION_FILE))?,
        })
    }

    /// The file was not found, so the agent runs without a restored login.
    #[must_use]
    pub fn missing(&self) -> bool {
        self.text.is_none()
    }
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

    /// The test reached a verdict on the game, whatever status the agent exited with: the
    /// verdict is stored either way. `test_failed` means the test itself broke and is worth
    /// retrying.
    #[must_use]
    pub fn finished(&self) -> bool {
        matches!(self.status(), "working" | "failing")
    }

    /// The agent stopped the test because the login session was missing or expired, as
    /// `error.reason` says. It is the operator's to fix, and says nothing about the game.
    #[must_use]
    pub fn session_problem(&self) -> bool {
        self.status() == "test_failed"
            && self
                .document
                .pointer("/error/reason")
                .and_then(Value::as_str)
                == Some(SESSION_REASON)
    }
}

/// What a run does when `session-block.txt` is missing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MissingSession {
    /// Warn and let the agent run without a login. It should report the test `test_failed`.
    Warn,
    /// Stop before the ping: the run could not tell anything about the game.
    Refuse,
}

/// Marks an error that stopped a test before the agent ran it, such as a session file that
/// cannot be used, a missing tool, a failed ping, or a run directory that cannot be written. It
/// says nothing about the game, only about the environment.
#[derive(Debug)]
pub struct NotStarted;

impl fmt::Display for NotStarted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("the test did not start")
    }
}

/// Whether `error`, returned by `test_game`, stopped the test before the agent ran it.
#[must_use]
pub fn did_not_start(error: &anyhow::Error) -> bool {
    error.downcast_ref::<NotStarted>().is_some()
}

/// Reads the session, checks the tools, pings the model, runs the full test, stores it, and
/// prints a summary. `missing_session` says whether a missing `session-block.txt` only warns.
///
/// The game must already be stored, since its tests refer to it. A `playwright-cli` other than
/// the release the prompt was written for only warns. The agent is stopped after `timeout`. An
/// interrupt from the operator stops the agent, and testson exits once the browser is closed and
/// the test is stored.
///
/// # Errors
///
/// Fails when the session file cannot be used, a tool is missing, the ping fails, the run
/// directories cannot be written, the agent cannot start, or the test cannot be stored. The
/// errors from before the agent runs are marked [`NotStarted`].
pub fn test_game(
    data_dir: &Path,
    game: &Game,
    url: &str,
    agent: &Agent,
    window: Window,
    timeout: Duration,
    missing_session: MissingSession,
) -> Result<Outcome> {
    let session = start(data_dir, agent, missing_session).context(NotStarted)?;
    let deferred = harness::defer_interrupts();
    let outcome = run_and_store(data_dir, game, url, agent, &session, window, timeout);
    drop(deferred);
    harness::exit_if_interrupted();
    outcome
}

/// The part of [`test_game`] that an interrupt does not cut short. A run directory that cannot
/// be prepared is marked [`NotStarted`] too: the agent has not run yet.
fn run_and_store(
    data_dir: &Path,
    game: &Game,
    url: &str,
    agent: &Agent,
    session: &Session,
    window: Window,
    timeout: Duration,
) -> Result<Outcome> {
    let prepared = prepare(data_dir, &game.name, url, window, session).context(NotStarted)?;
    say!("Game:   {} ({})", game.name, game.id);
    say!("URL:    {url}");
    say!("Agent:  {agent}");
    say!("Limit:  {}s", timeout.as_secs());
    say!("Output: {}", prepared.dir.display());
    let outcome = execute(&prepared, agent, timeout)?;
    let test_id = db::insert_test(data_dir, &game.id, &outcome.document.to_string())
        .with_context(|| format!("could not store the test of {}", game.name))?;
    say!("Status: {}", outcome.status());
    say!("Summary: {}", outcome.text("summary"));
    say!("Video:  {}", outcome.text("video_path"));
    let trace = prepared.dir.join(TRACE_FILE);
    if trace.is_file() {
        say!(
            "Trace:  {} (private: it holds the session)",
            trace.display()
        );
    } else {
        say!("Trace:  none saved");
    }
    say!("Report: {}", outcome.report_path.display());
    say!(
        "Dashboard: http://{}/game/{}#test-{test_id}",
        crate::ADDRESS,
        game.id
    );
    if outcome.agent_exit_code != 0 {
        esay!(
            "error: {} exited with status {}. The report above is what it returned.",
            agent.harness,
            outcome.agent_exit_code
        );
    }
    Ok(outcome)
}

/// Everything before the agent runs: reads the session, checks the tools, and pings the model.
///
/// The session is read first, so that a session file that cannot be used stops the run before
/// the ping spends anything. A `playwright-cli` other than the release the prompt was written
/// for only warns.
fn start(data_dir: &Path, agent: &Agent, missing_session: MissingSession) -> Result<Session> {
    let session = Session::load(data_dir)?;
    if session.missing() {
        let path = data_dir.join(SESSION_FILE);
        if missing_session == MissingSession::Refuse {
            bail!(
                "{} was not found, so no game can be tested with a login. Create it as the README \
                 says (Session).",
                path.display()
            );
        }
        esay!(
            "warning: {} was not found. The agent will run without a restored login.",
            path.display()
        );
    }
    if !browser::playwright_cli_installed() {
        bail!(
            "playwright-cli is not on PATH.\nInstall it with: {}",
            browser::install_hint()
        );
    }
    browser::warn_on_other_version();
    agent.ensure_installed()?;
    say!("Pinging {agent}...");
    agent.ping()?;
    Ok(session)
}

/// Fills the prompt, and writes `runs/<slug>-<timestamp>/` and `assets/<slug>-<timestamp>/`
/// under `data_dir`, with a `-2`, `-3`, ... suffix when another run of the game took the name.
/// The playwright-cli session is named after the run directory too, so no two runs share one.
///
/// # Errors
///
/// Fails when the run or assets directory would hold a character the prompt's shell commands
/// cannot quote, or a run directory cannot be written. A run that fails leaves no directory
/// behind.
pub fn prepare(
    data_dir: &Path,
    game_name: &str,
    url: &str,
    window: Window,
    session: &Session,
) -> Result<Prepared> {
    prepare_with(data_dir, game_name, url, window, session, TEMPLATE)
}

/// `prepare` with the prompt template given, so the tests can make a run fail once its
/// directories are claimed.
fn prepare_with(
    data_dir: &Path,
    game_name: &str,
    url: &str,
    window: Window,
    session: &Session,
    template: &str,
) -> Result<Prepared> {
    let slug = slugify(game_name);
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let (runs, assets) = (data_dir.join(RUNS_DIR), data_dir.join(ASSETS_DIR));
    // The run and assets directories add only the slug and the time to these.
    check_quotable(&absolute(&runs)?)?;
    check_quotable(&absolute(&assets)?)?;
    for root in [&runs, &assets] {
        fs::create_dir_all(root).with_context(|| format!("could not create {}", root.display()))?;
    }
    set_mode(&runs, 0o700);
    let (dir, assets_dir) = claim_run_dirs(&runs, &assets, &format!("{slug}-{stamp}"))?;
    let filled = fill_run(
        &dir,
        &assets_dir,
        template,
        game_name,
        url,
        window,
        session.text.as_deref(),
    );
    match filled {
        Ok(Filled {
            data_prefix,
            session_name,
            prompt,
        }) => Ok(Prepared {
            dir,
            assets_dir,
            data_prefix,
            url: url.to_owned(),
            session_name,
            prompt,
        }),
        Err(error) => {
            // Both were created by this run and hold only what it wrote, the session among it.
            for claimed in [&dir, &assets_dir] {
                let _ = fs::remove_dir_all(claimed);
            }
            Err(error)
        }
    }
}

/// What [`fill_run`] wrote that the run needs.
struct Filled {
    data_prefix: String,
    session_name: String,
    prompt: String,
}

/// Writes the screenshots directory, the session, restore, login-check and stop-trace scripts and
/// the filled prompt of a run whose directories are claimed. `session` is the session file, if
/// there is one.
fn fill_run(
    dir: &Path,
    assets_dir: &Path,
    template: &str,
    game_name: &str,
    url: &str,
    window: Window,
    session: Option<&str>,
) -> Result<Filled> {
    let screenshots = assets_dir.join("screenshots");
    fs::create_dir(&screenshots)
        .with_context(|| format!("could not create {}", screenshots.display()))?;
    let folder = dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("run directory name is not valid UTF-8")?;
    let data_prefix = format!("{ASSETS_URL}/{folder}");
    let session_name = format!("betsson-{folder}");
    let prompt = fill_template(
        template,
        TemplateValues {
            game_name,
            betsson_url: url,
            output_dir: absolute(dir)?
                .to_str()
                .context("run directory is not valid UTF-8")?,
            assets_dir: absolute(assets_dir)?
                .to_str()
                .context("assets directory is not valid UTF-8")?,
            session_name: &session_name,
            open_options: &window.open_options(),
            data_prefix: &data_prefix,
        },
    )?;
    let restore = match session {
        Some(session) => {
            let session_path = absolute(&dir.join(SESSION_SCRIPT))?;
            write_private(&session_path, &session_script(session))?;
            restore_script(
                Some(
                    session_path
                        .to_str()
                        .context("session script path is not valid UTF-8")?,
                ),
                session_statements(session),
            )
        }
        None => restore_script(None, 0),
    };
    write_private(&dir.join("restore-session.js"), &restore)?;
    write_private(&dir.join("check-session.js"), CHECK_SESSION)?;
    let trace_file = absolute(&dir.join(TRACE_FILE))?;
    write_private(
        &dir.join("stop-trace.js"),
        &stop_trace_script(
            trace_file
                .to_str()
                .context("run directory is not valid UTF-8")?,
        ),
    )?;
    write_private(&dir.join("prompt.filled.txt"), &prompt)?;
    Ok(Filled {
        data_prefix,
        session_name,
        prompt,
    })
}

/// Runs the agent in the run directory for at most `timeout`, closes the browser it used, and
/// writes `report.json`, the document to store.
///
/// A non-zero agent status still returns the report the agent produced. The report is read
/// only from the agent's final answer. When the agent returns no report, one the dashboard
/// cannot show, or runs out of time, the document is a `test_failed` test that says why, so the
/// run still shows up on the dashboard.
///
/// # Errors
///
/// Fails when the agent cannot start or a file cannot be written.
pub fn execute(prepared: &Prepared, agent: &Agent, timeout: Duration) -> Result<Outcome> {
    let log_path = prepared.dir.join("model.log");
    let report_path = prepared.dir.join("report.json");
    let started_at = Utc::now();
    let clock = Instant::now();
    let mut log = create_private(&log_path)?;
    writeln!(log, "# agent: {agent}")?;
    let assets = absolute(&prepared.assets_dir)?;
    let transcript = agent.run(&prepared.prompt, &prepared.dir, &assets, &mut log, timeout);
    drop(log);
    // The browser outlives the agent, whether it finished, failed, timed out, or was stopped.
    close_browser(prepared);
    let transcript = transcript?;
    let mut document = verdict(prepared, &transcript, timeout, started_at, clock);
    keep_assets_safe(prepared, &mut document);
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

/// The document to store: the report in the agent's final answer, or a `test_failed` test that
/// says why there is none.
fn verdict(
    prepared: &Prepared,
    transcript: &Transcript,
    timeout: Duration,
    started_at: DateTime<Utc>,
    clock: Instant,
) -> Value {
    final_report(transcript, timeout, &prepared.data_prefix).unwrap_or_else(|error| {
        esay!(
            "error: {error}. See {}",
            prepared.dir.join("model.log").display()
        );
        let duration = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        test_failed(prepared, started_at, duration, &format!("{error}."))
    })
}

/// The report in the agent's final answer, checked for what the dashboard relies on.
///
/// Only the final answer counts. A draft the agent printed along the way, or a report quoted in
/// a tool's output, is not a verdict, so the model log is never searched for one.
///
/// # Errors
///
/// Fails, saying why, when the run timed out, was interrupted, ended in an error, left no final
/// answer, or the answer is not a report the dashboard can show. What [`check_report`] drops
/// from the report is only warned about.
fn final_report(transcript: &Transcript, timeout: Duration, data_prefix: &str) -> Result<Value> {
    if transcript.timed_out {
        // Whatever the agent printed before it was stopped is not a finished test.
        bail!(
            "the agent did not finish within {}s and was stopped",
            timeout.as_secs()
        );
    }
    if transcript.interrupted {
        bail!("the run was interrupted and the agent was stopped");
    }
    let message = transcript.last_message.as_deref();
    if transcript.ended_in_error {
        bail!(
            "the agent run ended in an error: {}",
            preview(message.unwrap_or_default())
        );
    }
    let Some(message) = message else {
        bail!("the agent returned no final message");
    };
    let (report, dropped) = check_report(parse_report(message)?, data_prefix)?;
    for what in dropped {
        esay!("warning: dropped from the report: {what}.");
    }
    Ok(report)
}

/// Reads a report from a final message that holds one JSON object and nothing else, though it may
/// sit in a ```` ```json ```` fence.
///
/// # Errors
///
/// Fails when the message is not one JSON object with `schema_version` and `status`.
fn parse_report(message: &str) -> Result<Value> {
    let report: Value = serde_json::from_str(unfenced(message))
        .map_err(|error| anyhow!("the final message is not one JSON object ({error})"))?;
    if !report.get("schema_version").is_some_and(Value::is_u64)
        || !report.get("status").is_some_and(Value::is_string)
    {
        bail!("the final message is not a report with schema_version and status");
    }
    Ok(report)
}

/// `message` without a byte order mark, surrounding whitespace, or one Markdown code fence around
/// it.
fn unfenced(message: &str) -> &str {
    let text = message.trim_start_matches('\u{feff}').trim();
    let Some(body) = text
        .strip_prefix("```")
        .and_then(|fenced| fenced.strip_suffix("```"))
    else {
        return text;
    };
    // The rest of the fence's first line names a language, such as `json`, `JSON` or `jsonc`.
    match body.split_once('\n') {
        Some((label, rest)) if !label.trim_start().starts_with(['{', '[']) => rest.trim(),
        _ => body.trim(),
    }
}

/// The start of an error message, short enough for the dashboard.
fn preview(text: &str) -> String {
    let text = text.trim();
    let mut shown: String = text.chars().take(ERROR_PREVIEW).collect();
    if shown.len() < text.len() {
        shown.push_str(" …");
    }
    shown
}

/// Closes the browser session of the run, which the agent may have left open. Best effort: a
/// failure is only reported.
fn close_browser(prepared: &Prepared) {
    let mut command = browser::playwright_cli();
    command
        .args(["--session", &prepared.session_name, "close"])
        .current_dir(&prepared.dir);
    match harness::run_quietly(command, CLOSE_TIMEOUT) {
        Ok(status) if status.success() => {}
        Ok(status) => esay!(
            "warning: closing browser session {} exited with {status}",
            prepared.session_name
        ),
        Err(error) => esay!(
            "warning: could not close browser session {}: {error:#}",
            prepared.session_name
        ),
    }
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
    if prepared.assets_dir.join("session.webm").is_file() {
        document["video_path"] = json!(format!("{}/session.webm", prepared.data_prefix));
    }
    document
}

/// Keeps the trace, and whatever else the agent left in the assets that may hold the session,
/// out of what the dashboard serves.
///
/// The prompt keeps the trace in the run directory, but an agent may still report a
/// `trace_path`, copy the trace into the assets as older prompts told it to, or run
/// playwright-cli from the assets so that its working files, raw trace included, land there.
/// Every file in the assets that is not a recording or an image is moved into
/// [`QUARANTINE_DIR`] in the run directory, and the report stops naming it.
fn keep_assets_safe(prepared: &Prepared, document: &mut Value) {
    if let Some(fields) = document.as_object_mut() {
        fields.remove("trace_path");
    }
    let quarantine = prepared.dir.join(QUARANTINE_DIR);
    let mut moved = Vec::new();
    for file in unservable_files(&prepared.assets_dir) {
        let Ok(relative) = file.strip_prefix(&prepared.assets_dir) else {
            continue;
        };
        let target = quarantine.join(relative);
        let result = target
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| fs::rename(&file, &target));
        match result {
            Ok(()) => moved.push(asset_url(&prepared.data_prefix, relative)),
            Err(error) => esay!(
                "warning: {} may hold the session and is served by the dashboard. Delete it: \
                 {error}",
                file.display()
            ),
        }
    }
    if !moved.is_empty() {
        esay!(
            "warning: moved {} file(s) the dashboard must not serve into {}",
            moved.len(),
            quarantine.display()
        );
        drop_references(document, &moved);
    }
}

/// Every file under `dir` the dashboard must not serve: anything but a recording or an image,
/// and every symbolic link, since the server would follow it.
fn unservable_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            files.extend(unservable_files(&path));
        } else if kind.is_symlink() || !is_servable(&path) {
            files.push(path);
        }
    }
    files
}

fn is_servable(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(serves_extension)
}

/// The URL path a file of the assets, given relative to them, is served at.
fn asset_url(data_prefix: &str, relative: &Path) -> String {
    let parts: Vec<String> = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect();
    format!("{data_prefix}/{}", parts.join("/"))
}

/// Removes the video and the screenshots of `document` that point at one of `urls`.
fn drop_references(document: &mut Value, urls: &[String]) {
    let named = |value: &Value| {
        value
            .as_str()
            .is_some_and(|path| urls.iter().any(|url| url == path))
    };
    if let Some(fields) = document.as_object_mut()
        && fields.get("video_path").is_some_and(named)
    {
        fields.remove("video_path");
    }
    let steps = document.get_mut("steps").and_then(Value::as_array_mut);
    for step in steps.into_iter().flatten() {
        if let Some(screenshots) = step.get_mut("screenshots").and_then(Value::as_array_mut) {
            screenshots.retain(|screenshot| !screenshot.get("path").is_some_and(named));
        }
    }
}

/// Checks a report against `schema/test-game.schema.json` and what the dashboard and the database
/// rely on beyond it, and returns it with its dates in the form the database reads, along with
/// what was dropped from it.
///
/// The report comes from the agent, so nothing in it is trusted. The dashboard links to the video,
/// the trace, the screenshots and the game, so those must be files of this run, under
/// `data_prefix`, and a page on Betsson. One that is not is dropped rather than failing the
/// report, so that a stray address does not cost the verdict on the game.
///
/// # Errors
///
/// Fails, naming the field, when the report does not follow the schema or its `started_at` is a
/// date the database cannot store.
fn check_report(mut report: Value, data_prefix: &str) -> Result<(Value, Vec<String>)> {
    let dropped = drop_bad_evidence(&mut report, data_prefix);
    // Before the schema, which would refuse some of the forms chrono reads, such as a space for
    // the `T`.
    if let Some(text) = report.get("started_at").and_then(Value::as_str) {
        let utc = normalise_date_time(text)
            .context("the report /started_at is not a date-time the database can store")?;
        report["started_at"] = json!(utc);
    }
    if let Some(error) = REPORT_VALIDATOR.iter_errors(&report).next() {
        let field = match error.instance_path().as_str() {
            "" => "/",
            path => path,
        };
        // Masked, so that neither a whole document nor a stray token ends up in the message.
        bail!(
            "the report does not follow the schema at {field}: {}",
            error.masked()
        );
    }
    if report["summary"]
        .as_str()
        .is_none_or(|summary| summary.trim().is_empty())
    {
        bail!("the report has no summary");
    }
    Ok((report, dropped))
}

/// Removes from a report the game URL, video, trace, and screenshots the dashboard must not
/// link to, and the screenshot times it cannot read, and says what went.
///
/// Values of the wrong type are left for the schema to refuse.
fn drop_bad_evidence(report: &mut Value, data_prefix: &str) -> Vec<String> {
    let mut dropped = Vec::new();
    let Some(object) = report.as_object_mut() else {
        return dropped;
    };
    let game_url = object.get("game_url").and_then(Value::as_str);
    if game_url.is_some_and(|url| parse_betsson_url(url).is_err()) {
        object.remove("game_url");
        dropped.push("/game_url, which is not https on a betsson host".to_owned());
    }
    // The trace records the session, so it stays in the private run directory and is never
    // linked, wherever the report says it is.
    if object.remove("trace_path").is_some() {
        dropped.push("/trace_path, since the trace holds the session and stays private".to_owned());
    }
    let video_path = object.get("video_path").and_then(Value::as_str);
    if video_path.is_some_and(|path| !is_run_asset(path, data_prefix)) {
        object.remove("video_path");
        dropped.push(format!(
            "/video_path, which is not a file under {data_prefix}/"
        ));
    }
    let steps = object.get_mut("steps").and_then(Value::as_array_mut);
    for (step_index, step) in steps.into_iter().flatten().enumerate() {
        let Some(screenshots) = step.get_mut("screenshots").and_then(Value::as_array_mut) else {
            continue;
        };
        let count = screenshots.len();
        screenshots.retain(|screenshot| {
            let path = screenshot.get("path").and_then(Value::as_str);
            path.is_none_or(|path| is_run_asset(path, data_prefix))
        });
        if screenshots.len() < count {
            dropped.push(format!(
                "{} of /steps/{step_index}/screenshots, which are not files under {data_prefix}/",
                count - screenshots.len()
            ));
        }
        for (index, screenshot) in screenshots.iter_mut().enumerate() {
            let Some(text) = screenshot.get("taken_at").and_then(Value::as_str) else {
                continue;
            };
            if let Some(utc) = normalise_date_time(text) {
                screenshot["taken_at"] = json!(utc);
            } else if let Some(screenshot) = screenshot.as_object_mut() {
                screenshot.remove("taken_at");
                dropped.push(format!(
                    "/steps/{step_index}/screenshots/{index}/taken_at, which is not a date-time"
                ));
            }
        }
    }
    dropped
}

/// Whether `path` is the URL path of a file of this run, under `data_prefix`.
///
/// The dashboard uses it as a link or an image source, so it must be a plain path that stays
/// among the run's assets: no scheme, query or fragment, and no `..`, backslash or percent
/// sign, since a browser reads an encoded `%2e%2e` as `..`.
fn is_run_asset(path: &str, data_prefix: &str) -> bool {
    path.strip_prefix(data_prefix)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|file| {
            !file.is_empty() && !file.contains("..") && !file.contains(['\\', '?', '#', ':', '%'])
        })
}

/// Rewrites an RFC 3339 date-time in UTC with milliseconds, such as `2026-10-12T08:00:00.000Z`,
/// or returns nothing for one the database cannot store.
///
/// RFC 3339 allows forms SQLite's date functions do not read, such as a lowercase `t` or `z`, a
/// space for the `T`, a leap second, or an offset that moves the date out of the years 0 to
/// 9999. The `started_at` column is generated from the date, so any of them would keep the test
/// from being stored.
fn normalise_date_time(text: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|time| time.with_timezone(&Utc))
        // chrono keeps a leap second as a fraction of a second of one second or more.
        .filter(|time| time.timestamp_subsec_nanos() < 1_000_000_000)
        .filter(|time| (0..=9999).contains(&time.year()))
        .map(|time| time.to_rfc3339_opts(SecondsFormat::Millis, true))
}

/// The session file, or `None` when it does not exist.
fn load_session(path: &Path) -> Result<Option<String>> {
    if !path.is_file() {
        return Ok(None);
    }
    set_mode(path, 0o600);
    let text =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    // Windows Notepad starts the file with a byte order mark, which would hide a first-line
    // comment and end up inside the first JavaScript statement.
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned();
    if session_has_placeholder(&text) {
        bail!(
            "{SESSION_FILE} still contains REPLACE_WITH_ placeholders.\n\
             Log in to Betsson, copy the localStorage values, and replace the placeholders.\n\
             The session expires. Update this file when demo mode starts asking you to log in."
        );
    }
    Ok(Some(text))
}

/// Claims a private run directory and its assets directory, both named `name`, or `name-2`,
/// `name-3`, and so on when a run that started in the same second took it first.
///
/// Each directory is claimed with `fs::create_dir`, which fails when it already exists, so two
/// runs never share one, whether they are in one process or in two. `runs` and `assets` must
/// exist.
fn claim_run_dirs(runs: &Path, assets: &Path, name: &str) -> Result<(PathBuf, PathBuf)> {
    for attempt in 1..=MAX_CLAIMS {
        let folder = if attempt == 1 {
            name.to_owned()
        } else {
            format!("{name}-{attempt}")
        };
        let dir = runs.join(&folder);
        if !claim(&dir)? {
            continue;
        }
        set_mode(&dir, 0o700);
        let assets_dir = assets.join(&folder);
        let assets_claimed = claim(&assets_dir);
        if !matches!(assets_claimed, Ok(true)) {
            // Left by a run that no longer has its run directory, or not creatable at all: give
            // the name up. The run directory was created just above, so it is still empty.
            let _ = fs::remove_dir(&dir);
        }
        if assets_claimed? {
            return Ok((dir, assets_dir));
        }
    }
    bail!(
        "could not claim a run directory for {name} in {}",
        runs.display()
    )
}

/// Creates `dir`, or returns `false` when it already exists.
fn claim(dir: &Path) -> Result<bool> {
    match fs::create_dir(dir) {
        Ok(()) => Ok(true),
        Err(error) if taken(&error, dir) => Ok(false),
        Err(error) => Err(error).with_context(|| format!("could not create {}", dir.display())),
    }
}

/// Whether `error`, from creating `dir`, means that something already holds the name.
///
/// Windows reports a name still held by a directory being deleted, until the last handle to it
/// closes, as access denied rather than as already existing. Access denied counts as taken only
/// when the name can be seen, so a directory that cannot be written still fails.
fn taken(error: &io::Error, dir: &Path) -> bool {
    match error.kind() {
        ErrorKind::AlreadyExists => true,
        ErrorKind::PermissionDenied => cfg!(windows) && fs::symlink_metadata(dir).is_ok(),
        _ => false,
    }
}

/// Refuses a path the prompt cannot put in a shell command.
///
/// The prompt quotes the run and assets directories in commands such as
/// `run-code --filename '<dir>/restore-session.js'` and `video-start "<dir>/session.webm"`. A
/// quote, `$`, a backtick, or a line break
/// would end the quoting or run a command, and no one escape suits every shell an agent may use.
/// A single backslash is literal there, but bash turns `\\` into `\` inside double quotes, which
/// would change a network path such as `\\nas\share`.
///
/// # Errors
///
/// Fails, naming the path and the character, when the path holds one of [`SHELL_UNSAFE`] or a
/// doubled backslash.
fn check_quotable(path: &Path) -> Result<()> {
    let text = path
        .to_str()
        .with_context(|| format!("{} is not valid UTF-8", path.display()))?;
    if let Some(character) = text.chars().find(|c| SHELL_UNSAFE.contains(c)) {
        bail!(
            "{} contains {character:?}, which cannot be quoted in the agent's shell commands. \
             Use a data directory whose path has no quotes, $, backticks, or line breaks.",
            path.display()
        );
    }
    if text.contains(r"\\") {
        bail!(
            "{} contains \\\\, which bash reads as a single \\ inside quotes. Map the network \
             share to a drive letter and use a data directory on it.",
            path.display()
        );
    }
    Ok(())
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
    use std::collections::HashSet;
    use std::sync::Barrier;
    use std::thread;

    use super::*;
    use crate::harness::Harness;

    /// Prepares a run with the session stored in `dir`.
    fn prepare_in(dir: &Path, game_name: &str, window: Window) -> Result<Prepared> {
        let session = Session::load(dir)?;
        prepare(
            dir,
            game_name,
            "https://www.betsson.es/casino",
            window,
            &session,
        )
    }

    #[test]
    fn prepare_writes_a_private_prompt_and_restore_script() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(SESSION_FILE),
            "localStorage.setItem('customerId', '7');\n\
             localStorage.setItem('session', 'SECRET_TOKEN_MARKER');\n",
        )
        .unwrap();
        assert!(!Session::load(dir.path()).unwrap().missing());
        let prepared = prepare_in(dir.path(), "Los Piratas Golfetes", Window::Headless).unwrap();
        // The agent opens the Chromium the install steps provide, without a window when asked.
        assert!(
            prepared
                .prompt
                .contains("open \"https://www.betsson.es/casino\" --browser=chromium\n")
        );
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
        assert!(
            prepared
                .session_name
                .starts_with("betsson-los-piratas-golfetes-")
        );
        assert!(
            prepared
                .prompt
                .contains(&format!("--session {} close", prepared.session_name)),
            "the session closed after the run is the one the agent opens"
        );
        // The session must never land where the dashboard serves files from.
        assert!(
            fs::read_dir(&prepared.assets_dir)
                .unwrap()
                .all(|entry| { entry.unwrap().file_name() == "screenshots" })
        );
        // The login reaches the page only through the private session script, loaded by path.
        let session = fs::read_to_string(prepared.dir.join(SESSION_SCRIPT)).unwrap();
        assert!(session.contains("localStorage.setItem('session', 'SECRET_TOKEN_MARKER');"));
        let restore = fs::read_to_string(prepared.dir.join("restore-session.js")).unwrap();
        let session_path = absolute(&prepared.dir.join(SESSION_SCRIPT)).unwrap();
        assert!(
            restore.contains(&serde_json::Value::from(session_path.to_str().unwrap()).to_string())
        );
        for text in [
            prepared.prompt.as_str(),
            &fs::read_to_string(prepared.dir.join("prompt.filled.txt")).unwrap(),
            &restore,
        ] {
            assert!(!text.contains("SECRET_TOKEN_MARKER"));
            assert!(!text.contains("localStorage.setItem("));
        }
        // The trace records the session too, so it is saved beside the restore script.
        let stop_trace = fs::read_to_string(prepared.dir.join("stop-trace.js")).unwrap();
        let trace_file = prepared.dir.join(TRACE_FILE);
        let trace_literal = Value::from(trace_file.to_str().unwrap()).to_string();
        assert!(stop_trace.contains(&trace_literal), "{stop_trace}");
        let assets = absolute(&prepared.assets_dir).unwrap();
        let assets = assets.to_str().unwrap();
        assert!(prepared.prompt.contains(assets));
        for line in prepared.prompt.lines() {
            if line.to_lowercase().contains("trace") {
                assert!(!line.contains(assets), "{line}");
                assert!(!line.contains(&prepared.data_prefix), "{line}");
            }
        }
        assert!(!restore.contains("hasAuth"));
        let check = fs::read_to_string(prepared.dir.join("check-session.js")).unwrap();
        assert_eq!(check, CHECK_SESSION);
        assert!(!prepared.assets_dir.join("check-session.js").exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in ["prompt.filled.txt", SESSION_SCRIPT, "restore-session.js"] {
                let mode = fs::metadata(prepared.dir.join(file))
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(mode & 0o777, 0o600, "{file}");
            }
        }
    }

    #[test]
    fn rejects_placeholders_and_notes_a_missing_session() {
        let dir = tempfile::tempdir().unwrap();
        assert!(Session::load(dir.path()).unwrap().missing());
        let prepared = prepare_in(dir.path(), "Alpha", Window::Headed).unwrap();
        // The restore script tells the agent, and there is no session script to load.
        let restore = fs::read_to_string(prepared.dir.join("restore-session.js")).unwrap();
        assert!(restore.contains("session-block.txt was not found"));
        assert!(!prepared.dir.join(SESSION_SCRIPT).exists());
        fs::write(
            dir.path().join(SESSION_FILE),
            "localStorage.setItem('session', 'REPLACE_WITH_SESSION');\n",
        )
        .unwrap();
        let error = Session::load(dir.path()).unwrap_err();
        assert!(error.to_string().contains("REPLACE_WITH_"));
    }

    /// The playwright-cli session the prompt tells the agent to use.
    fn session_name(prepared: &Prepared) -> &str {
        prepared
            .prompt
            .lines()
            .find_map(|line| line.strip_prefix("Playwright session name: "))
            .unwrap()
    }

    /// The run directories, assets directories, and sessions of `runs`, each counted once.
    fn distinct(runs: &[Prepared]) -> [usize; 3] {
        let dirs: HashSet<_> = runs.iter().map(|run| &run.dir).collect();
        let assets: HashSet<_> = runs.iter().map(|run| &run.assets_dir).collect();
        let sessions: HashSet<_> = runs.iter().map(session_name).collect();
        [dirs.len(), assets.len(), sessions.len()]
    }

    #[test]
    fn runs_of_a_game_in_the_same_second_get_their_own_directories_and_session() {
        let dir = tempfile::tempdir().unwrap();
        let runs: Vec<_> = (0..3)
            .map(|_| prepare_in(dir.path(), "Alpha", Window::Headed).unwrap())
            .collect();
        assert_eq!(distinct(&runs), [3; 3]);
        for run in &runs {
            assert_eq!(run.assets_dir.file_name(), run.dir.file_name());
            let folder = run.dir.file_name().unwrap().to_str().unwrap();
            assert_eq!(session_name(run), format!("betsson-{folder}"));
        }
    }

    #[test]
    fn runs_started_at_once_never_share_a_directory() {
        const THREADS: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let barrier = Barrier::new(THREADS);
        let runs: Vec<_> = thread::scope(|scope| {
            let handles: Vec<_> = (0..THREADS)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        prepare_in(dir.path(), "Alpha", Window::Headed).unwrap()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect()
        });
        assert_eq!(distinct(&runs), [THREADS; 3]);
    }

    #[test]
    fn skips_a_name_whose_run_or_assets_directory_exists() {
        let dir = tempfile::tempdir().unwrap();
        let (runs, assets) = (dir.path().join(RUNS_DIR), dir.path().join(ASSETS_DIR));
        fs::create_dir(&runs).unwrap();
        fs::create_dir_all(assets.join("alpha-1")).unwrap();
        let (run, run_assets) = claim_run_dirs(&runs, &assets, "alpha-1").unwrap();
        assert_eq!(run, runs.join("alpha-1-2"));
        assert_eq!(run_assets, assets.join("alpha-1-2"));
        assert!(
            !runs.join("alpha-1").exists(),
            "a name given up leaves no run directory behind"
        );
        let (run, run_assets) = claim_run_dirs(&runs, &assets, "alpha-1").unwrap();
        assert_eq!(run, runs.join("alpha-1-3"));
        assert_eq!(run_assets, assets.join("alpha-1-3"));
    }

    /// The names in `dir`.
    fn entries(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_run_whose_assets_cannot_be_claimed_gives_its_run_directory_up() {
        let dir = tempfile::tempdir().unwrap();
        let runs = dir.path().join(RUNS_DIR);
        fs::create_dir(&runs).unwrap();
        let missing = dir.path().join(ASSETS_DIR);
        assert!(claim_run_dirs(&runs, &missing, "alpha-1").is_err());
        assert_eq!(entries(&runs), Vec::<String>::new());
    }

    #[test]
    fn a_run_that_cannot_be_written_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(SESSION_FILE),
            "localStorage.setItem('customerId', '7');\n",
        )
        .unwrap();
        let session = Session::load(dir.path()).unwrap();
        let error = prepare_with(
            dir.path(),
            "Alpha",
            "https://www.betsson.es/casino",
            Window::Headed,
            &session,
            "{{GAME_NAME}} without the other placeholders",
        )
        .unwrap_err();
        assert!(error.to_string().contains("missing"), "{error}");
        assert_eq!(entries(&dir.path().join(RUNS_DIR)), Vec::<String>::new());
        assert_eq!(entries(&dir.path().join(ASSETS_DIR)), Vec::<String>::new());
    }

    #[test]
    fn access_denied_takes_a_name_only_on_windows_and_only_when_it_exists() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing");
        let error = |kind| io::Error::from(kind);
        assert!(taken(&error(ErrorKind::AlreadyExists), dir.path()));
        assert_eq!(
            taken(&error(ErrorKind::PermissionDenied), dir.path()),
            cfg!(windows)
        );
        assert!(!taken(&error(ErrorKind::PermissionDenied), &missing));
        assert!(!taken(&error(ErrorKind::NotFound), &missing));
    }

    /// Runs `test_game` in `dir` with an agent CLI that must never be reached: every case here
    /// stops on the session file, before the tools are looked for and the model is pinged.
    fn test_without_agent(dir: &Path, missing_session: MissingSession) -> anyhow::Error {
        let game = Game {
            id: "alpha".into(),
            name: "Alpha".into(),
            provider: String::new(),
            primary_type: String::new(),
            is_available_for_fun: true,
            image_url: String::new(),
        };
        let agent = Agent::new(Harness::Grok, None, None);
        test_game(
            dir,
            &game,
            "https://www.betsson.es/casino",
            &agent,
            Window::Headless,
            Duration::from_secs(60),
            missing_session,
        )
        .unwrap_err()
    }

    #[test]
    fn a_session_with_placeholders_stops_the_test_before_the_ping() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(SESSION_FILE),
            "localStorage.setItem('session', 'REPLACE_WITH_SESSION');\n",
        )
        .unwrap();
        for missing_session in [MissingSession::Warn, MissingSession::Refuse] {
            let error = test_without_agent(dir.path(), missing_session);
            assert!(did_not_start(&error), "{error:#}");
            assert!(format!("{error:#}").contains("REPLACE_WITH_"), "{error:#}");
        }
        assert!(!dir.path().join(RUNS_DIR).exists());
    }

    #[test]
    fn a_missing_session_stops_the_test_when_refused() {
        let dir = tempfile::tempdir().unwrap();
        let error = test_without_agent(dir.path(), MissingSession::Refuse);
        assert!(did_not_start(&error), "{error:#}");
        assert!(
            format!("{error:#}").contains("session-block.txt was not found"),
            "{error:#}"
        );
        assert!(!dir.path().join(RUNS_DIR).exists());
    }

    #[test]
    fn a_session_problem_is_a_test_failed_with_the_session_reason() {
        let outcome = |document| Outcome {
            report_path: PathBuf::new(),
            document,
            agent_exit_code: 0,
        };
        let failed = |reason: Value| json!({ "status": "test_failed", "error": { "message": "x", "reason": reason } });
        assert!(outcome(failed(json!("session"))).session_problem());
        assert!(!outcome(failed(json!("other"))).session_problem());
        assert!(
            !outcome(json!({ "status": "test_failed", "error": { "message": "x" } }))
                .session_problem()
        );
        assert!(
            !outcome(json!({ "status": "failing", "error": { "reason": "session" } }))
                .session_problem()
        );
        // The reason is the one the schema, and so the prompt, gives.
        let schema: Value = serde_json::from_str(crate::prompt::REPORT_SCHEMA).unwrap();
        assert_eq!(schema["properties"]["error"]["$ref"], "#/$defs/test_error");
        assert_eq!(
            schema["$defs"]["test_error"]["properties"]["reason"]["oneOf"][0]["const"],
            SESSION_REASON
        );
        assert!(crate::prompt::TEMPLATE.contains(r#"Set "reason": "session" in "error""#));
    }

    #[test]
    fn prepare_strips_a_byte_order_mark_from_the_session() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join(SESSION_FILE),
            "\u{feff}// REPLACE_WITH_ your values below\nlocalStorage.setItem('customerId', '7');\n",
        )
        .unwrap();
        let prepared = prepare_in(dir.path(), "Alpha", Window::Headed).unwrap();
        assert!(!prepared.prompt.contains('\u{feff}'));
        let session = fs::read_to_string(prepared.dir.join(SESSION_SCRIPT)).unwrap();
        assert!(!session.contains('\u{feff}'));
        assert!(session.contains("\n// REPLACE_WITH_ your values below\n"));
    }

    #[test]
    fn prepare_refuses_paths_the_prompt_cannot_quote() {
        let dir = tempfile::tempdir().unwrap();
        for (name, character) in [
            ("it's", "'"),
            ("cost$HOME", "$"),
            ("tick`id`", "`"),
            ("say\"hi\"", "\""),
            ("two\nlines", "\\n"),
        ] {
            let data_dir = dir.path().join(name);
            let error = prepare_in(&data_dir, "Alpha", Window::Headed)
                .unwrap_err()
                .to_string();
            assert!(error.contains(character), "{name:?}: {error}");
            assert!(error.contains(RUNS_DIR), "{name:?}: {error}");
            assert!(!data_dir.exists(), "nothing is written for {name:?}");
        }
        // Inside double quotes, bash turns `\\` into `\`, so a network path would change.
        let unc = Path::new(r"\\nas\share\data-dir");
        let error = prepare_in(unc, "Alpha", Window::Headed)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(r"\\") && error.contains("drive letter"),
            "{error}"
        );
        // Spaces, and the single backslashes of Windows paths, are quoted safely.
        let spaced = dir.path().join("data dir");
        prepare_in(&spaced, "Alpha", Window::Headed).unwrap();
    }

    #[test]
    fn only_a_clean_pass_passes_and_any_verdict_finishes() {
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
        // A verdict the agent reported before exiting with an error is stored, so it counts.
        assert!(outcome("working", 1).finished());
        assert!(outcome("failing", 2).finished());
        assert!(!outcome("test_failed", 1).finished());
    }

    fn report() -> Value {
        json!({
            "schema_version": 1,
            "status": "working",
            "summary": "Played one demo round.",
            // As check_report stores it, so a checked report equals this one.
            "started_at": "2026-10-12T08:00:00.000Z",
            "duration_ms": 1200,
            "steps": [{ "description": "Opened the lobby", "status": "passed" }],
        })
    }

    const PREFIX: &str = "/data/assets/alpha-20261012-100000";

    /// A report with every field the dashboard turns into a link or a date.
    fn full_report() -> Value {
        json!({
            "schema_version": 1,
            "status": "failing",
            "summary": "Demo mode never loaded.",
            "game_url": "https://www.betsson.es/casino/games/alpha",
            "started_at": "2026-10-12T10:00:00+02:00",
            "duration_ms": 1200,
            "video_path": format!("{PREFIX}/session.webm"),
            "steps": [{
                "description": "Clicked Modo prueba",
                "status": "failed",
                "error": { "message": "no demo control" },
                "screenshots": [{
                    "path": format!("{PREFIX}/screenshots/lobby.png"),
                    "taken_at": "2026-10-12T10:00:01.5+02:00",
                }],
                "console_messages": [{ "level": "error", "text": "boom" }],
                "failed_requests": [
                    { "url": "https://www.betsson.es/api", "method": "GET", "status_code": 500 },
                ],
            }],
        })
    }

    fn check_error(report: Value) -> String {
        check_report(report, PREFIX).unwrap_err().to_string()
    }

    /// The checked report, which must have lost nothing.
    fn check_whole(report: Value) -> Value {
        let (checked, dropped) = check_report(report, PREFIX).unwrap();
        assert!(dropped.is_empty(), "{dropped:?}");
        checked
    }

    #[test]
    fn checks_what_the_dashboard_relies_on() {
        check_whole(report());
        check_whole(full_report());
        for (key, value, problem) in [
            ("status", json!("passed"), "status"),
            ("summary", json!(" "), "summary"),
            ("started_at", json!("yesterday"), "started_at"),
            ("duration_ms", json!(-1), "duration_ms"),
            ("steps", json!([{ "status": "passed" }]), "steps"),
        ] {
            let mut broken = report();
            broken[key] = value;
            let error = check_error(broken);
            assert!(error.contains(problem), "{error}");
        }
    }

    /// [`full_report`] with the member at `parent` and `key` set to `value`, or removed.
    fn changed(parent: &str, key: &str, value: Option<Value>) -> Value {
        let mut report = full_report();
        let object = report.pointer_mut(parent).unwrap().as_object_mut().unwrap();
        match value {
            Some(value) => object.insert(key.to_owned(), value),
            None => object.remove(key),
        };
        report
    }

    #[test]
    fn a_test_stopped_by_the_session_says_so_in_its_error() {
        let mut stopped = report();
        stopped["status"] = json!("test_failed");
        stopped["error"] = json!({ "message": "logged out", "reason": "session" });
        check_whole(stopped.clone());
        stopped["error"]["reason"] = json!("network");
        let error = check_error(stopped);
        assert!(error.contains("at /error/reason:"), "{error}");
        // Only the error of the whole test has a reason, not the error of a step.
        let mut step = full_report();
        step["steps"][0]["error"]["reason"] = json!("session");
        let error = check_error(step);
        assert!(error.contains("at /steps/0/error:"), "{error}");
    }

    #[test]
    fn rejects_reports_that_do_not_follow_the_schema() {
        let step = "/steps/0";
        for (parent, key, value, field) in [
            ("", "duration_ms", Some(json!(1.5)), "at /duration_ms:"),
            ("", "extra", Some(json!(true)), "at /:"),
            ("", "error", Some(json!({ "message": "x" })), "at /:"),
            ("", "status", Some(json!("test_failed")), "at /:"),
            ("", "steps", Some(json!({})), "at /steps:"),
            ("", "game_url", Some(json!(5)), "at /game_url:"),
            ("", "video_path", Some(json!([])), "at /video_path:"),
            (
                step,
                "screenshots",
                Some(json!("a.png")),
                "at /steps/0/screenshots:",
            ),
            (
                step,
                "screenshots",
                Some(json!([{ "path": 5 }])),
                "at /steps/0/screenshots/0/path:",
            ),
            (step, "error", None, "at /steps/0:"),
            (
                step,
                "console_messages",
                Some(json!([{ "level": "info", "text": "x" }])),
                "at /steps/0/console_messages/0/level:",
            ),
            (
                step,
                "failed_requests",
                Some(json!([{ "url": "https://a.b/", "method": "GET" }])),
                "at /steps/0/failed_requests/0:",
            ),
        ] {
            let error = check_error(changed(parent, key, value));
            assert!(error.contains("does not follow the schema"), "{error}");
            assert!(error.contains(field), "{key}: {error}");
        }
    }

    #[test]
    fn keeps_the_urls_the_browser_prints_as_they_are() {
        // Chrome and Playwright print these unencoded. The dashboard only shows them as text.
        let urls = [
            "https://www.betsson.es/api/games?ids[]=1&ids[]=2",
            "https://cdn.example.com/a b/{id}/x|y^z",
            "https://cdn.example.com/señor/[1]",
            "chrome-extension://abc/script.js",
        ];
        let mut report = full_report();
        report["steps"][0]["failed_requests"] = urls
            .iter()
            .map(|url| json!({ "url": url, "method": "GET", "failure": "net::ERR_FAILED" }))
            .collect();
        report["steps"][0]["console_messages"] = urls
            .iter()
            .map(|url| json!({ "level": "warning", "text": "x", "source_url": url }))
            .collect();
        let checked = check_whole(report.clone());
        assert_eq!(checked["status"], "failing");
        for key in ["failed_requests", "console_messages"] {
            assert_eq!(checked["steps"][0][key], report["steps"][0][key]);
        }
    }

    #[test]
    fn drops_links_out_of_the_run_or_off_betsson() {
        for (key, value) in [
            ("game_url", "javascript:alert(1)"),
            ("game_url", "https://evil.example/betsson"),
            ("game_url", "http://www.betsson.es/casino"),
            ("game_url", "https://games.provider.example/launch?id=1"),
            ("game_url", "not a url"),
            ("trace_path", "javascript:alert(1)"),
            ("trace_path", "https://evil.example/trace.zip"),
            ("trace_path", "//evil.example/trace.zip"),
            ("trace_path", &format!("{PREFIX}/trace.zip")),
            ("video_path", "/data/assets/other-run/session.webm"),
            ("video_path", PREFIX),
            ("video_path", &format!("{PREFIX}/")),
            ("video_path", &format!("{PREFIX}-2/session.webm")),
            ("video_path", &format!("{PREFIX}/../x/session.webm")),
            ("video_path", &format!("{PREFIX}/%2e%2e/x/session.webm")),
            ("video_path", &format!("{PREFIX}/a\\..\\b.webm")),
            ("video_path", &format!("{PREFIX}/session.webm?x")),
            ("video_path", &format!("{PREFIX}/session.webm#x")),
        ] {
            let mut report = full_report();
            report[key] = json!(value);
            let (checked, dropped) = check_report(report, PREFIX).unwrap();
            // The verdict on the game stays; only the link goes.
            assert_eq!(checked["status"], "failing");
            assert!(checked.get(key).is_none(), "{value}");
            assert_eq!(dropped.len(), 1, "{value}: {dropped:?}");
            assert!(dropped[0].contains(key), "{value}: {dropped:?}");
        }
        let mut report = full_report();
        let screenshots = report["steps"][0]["screenshots"].as_array_mut().unwrap();
        screenshots.insert(0, json!({ "path": "javascript:alert(1)" }));
        screenshots.push(json!({ "path": "/data/assets/other-run/a.png" }));
        let (checked, dropped) = check_report(report, PREFIX).unwrap();
        assert_eq!(
            checked["steps"][0]["screenshots"],
            check_whole(full_report())["steps"][0]["screenshots"]
        );
        assert_eq!(dropped.len(), 1, "{dropped:?}");
        assert!(
            dropped[0].starts_with("2 of /steps/0/screenshots,"),
            "{dropped:?}"
        );
    }

    #[test]
    fn normalises_dates_to_utc_milliseconds() {
        for (date, stored) in [
            ("2026-10-12t10:00:00z", "2026-10-12T10:00:00.000Z"),
            ("2026-10-12 10:00:00Z", "2026-10-12T10:00:00.000Z"),
            ("2026-10-12T10:00:00+02:00", "2026-10-12T08:00:00.000Z"),
            (
                "2026-10-12T10:00:00.123456-01:30",
                "2026-10-12T11:30:00.123Z",
            ),
        ] {
            let mut report = full_report();
            report["started_at"] = json!(date);
            assert_eq!(check_whole(report)["started_at"], stored, "{date}");
        }
        let checked = check_whole(full_report());
        assert_eq!(
            checked["steps"][0]["screenshots"][0]["taken_at"],
            "2026-10-12T08:00:01.500Z"
        );
        for date in ["2026-12-31T23:59:60Z", "0000-01-01T00:00:00+01:00"] {
            let mut broken = full_report();
            broken["started_at"] = json!(date);
            let error = check_error(broken);
            assert!(error.contains("/started_at"), "{date}: {error}");
        }
        // A screenshot time is not stored in a column, so a bad one only loses the time.
        for date in ["2026-12-31T23:59:60Z", "soon"] {
            let mut report = full_report();
            report["steps"][0]["screenshots"][0]["taken_at"] = json!(date);
            let (checked, dropped) = check_report(report, PREFIX).unwrap();
            let screenshot = &checked["steps"][0]["screenshots"][0];
            assert!(screenshot.get("taken_at").is_none(), "{date}");
            assert!(screenshot.get("path").is_some(), "{date}");
            assert_eq!(
                dropped,
                ["/steps/0/screenshots/0/taken_at, which is not a date-time"]
            );
        }
    }

    #[test]
    fn a_lowercase_date_is_stored_with_its_started_at() {
        let dir = tempfile::tempdir().unwrap();
        db::setup(dir.path()).unwrap();
        db::upsert_game(dir.path(), &game()).unwrap();
        let mut lowercase = report();
        lowercase["started_at"] = json!("2026-10-12t10:00:00z");
        // As the agent wrote it, SQLite cannot read the date and the test would be lost.
        assert!(db::insert_test(dir.path(), "alpha", &lowercase.to_string()).is_err());
        let checked = check_whole(lowercase);
        db::insert_test(dir.path(), "alpha", &checked.to_string()).unwrap();
        let stored = serde_json::to_value(db::select_game(dir.path(), "alpha").unwrap()).unwrap();
        assert_eq!(stored["tests"][0]["started_at"], "2026-10-12T10:00:00.000Z");
    }

    #[test]
    fn a_run_without_a_report_is_stored_as_test_failed() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare_in(dir.path(), "Alpha", Window::Headed).unwrap();
        fs::write(prepared.assets_dir.join("session.webm"), "").unwrap();
        // A trace left in the assets, as older prompts asked, must not be linked.
        fs::write(prepared.assets_dir.join(TRACE_FILE), "").unwrap();
        let document = test_failed(&prepared, Utc::now(), 5, "no report.");
        // What testson writes itself goes through the checks the agent's reports go through,
        // and loses nothing to them.
        let checked = check_report(document.clone(), &prepared.data_prefix).unwrap();
        assert_eq!(checked, (document.clone(), Vec::new()));
        assert_eq!(document["status"], "test_failed");
        assert_eq!(document["error"]["message"], "no report.");
        assert_eq!(
            document["video_path"],
            format!("{}/session.webm", prepared.data_prefix)
        );
        assert!(document.get("trace_path").is_none());
    }

    #[test]
    fn only_recordings_and_images_stay_in_the_assets() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare_in(dir.path(), "Alpha", Window::Headed).unwrap();
        let assets = &prepared.assets_dir;
        let prefix = &prepared.data_prefix;
        // What an agent might leave: a copied trace, and the working files of playwright-cli
        // when it runs from the assets.
        fs::create_dir_all(assets.join(".playwright-cli/traces")).unwrap();
        for (file, contents) in [
            ("session.webm", "video"),
            ("screenshots/round.PNG", "image"),
            ("screenshots/notes.txt", "session"),
            (TRACE_FILE, "session"),
            (".playwright-cli/traces/trace-1.trace", "session"),
            (".playwright-cli/page-1.yml", "snapshot"),
        ] {
            fs::write(assets.join(file), contents).unwrap();
        }
        let mut document = report();
        document["trace_path"] = json!(format!("{prefix}/{TRACE_FILE}"));
        document["video_path"] = json!(format!("{prefix}/session.webm"));
        document["steps"][0]["screenshots"] = json!([
            { "path": format!("{prefix}/screenshots/round.PNG") },
            { "path": format!("{prefix}/screenshots/notes.txt") },
        ]);

        keep_assets_safe(&prepared, &mut document);

        assert!(document.get("trace_path").is_none());
        assert_eq!(document["video_path"], format!("{prefix}/session.webm"));
        assert_eq!(
            document["steps"][0]["screenshots"],
            json!([{ "path": format!("{prefix}/screenshots/round.PNG") }])
        );
        assert_eq!(unservable_files(assets), Vec::<PathBuf>::new());
        assert!(assets.join("session.webm").is_file());
        assert!(assets.join("screenshots/round.PNG").is_file());
        let quarantine = prepared.dir.join(QUARANTINE_DIR);
        for file in [
            "screenshots/notes.txt",
            TRACE_FILE,
            ".playwright-cli/traces/trace-1.trace",
            ".playwright-cli/page-1.yml",
        ] {
            assert!(!assets.join(file).exists(), "{file}");
            assert!(quarantine.join(file).is_file(), "{file}");
        }
    }

    #[test]
    fn a_run_that_timed_out_or_was_interrupted_is_test_failed_even_with_a_report() {
        let transcript = |timed_out, interrupted| Transcript {
            timed_out,
            interrupted,
            ..Transcript::from_stream(1, Some((report().to_string(), false)))
        };
        let timeout = Duration::from_secs(1_200);
        assert_eq!(
            final_report(&transcript(false, false), timeout, PREFIX).unwrap(),
            report()
        );
        let error = final_report(&transcript(true, false), timeout, PREFIX).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the agent did not finish within 1200s and was stopped"
        );
        let error = final_report(&transcript(false, true), timeout, PREFIX).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the run was interrupted and the agent was stopped"
        );
    }

    fn game() -> Game {
        Game {
            id: "alpha".into(),
            name: "Alpha".into(),
            provider: "P".into(),
            primary_type: "Slots".into(),
            is_available_for_fun: true,
            image_url: String::new(),
        }
    }

    #[test]
    fn stores_a_report_for_the_dashboard() {
        let dir = tempfile::tempdir().unwrap();
        db::setup(dir.path()).unwrap();
        db::upsert_game(dir.path(), &game()).unwrap();
        let id = db::insert_test(dir.path(), "alpha", &report().to_string()).unwrap();
        let stored = db::select_test_document(dir.path(), id).unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&stored).unwrap(), report());
        assert!(db::insert_test(dir.path(), "missing", &report().to_string()).is_err());
    }

    fn transcript(last_message: Option<&str>, ended_in_error: bool) -> Transcript {
        Transcript {
            last_message: last_message.map(str::to_owned),
            ended_in_error,
            ..Transcript::from_stream(0, None)
        }
    }

    /// A timeout no run in these tests reaches.
    const TIMEOUT: Duration = Duration::from_secs(1_200);

    fn prepared() -> (tempfile::TempDir, Prepared) {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare_in(dir.path(), "Alpha", Window::Headed).unwrap();
        (dir, prepared)
    }

    #[test]
    fn stores_the_report_in_the_final_message() {
        let (_dir, prepared) = prepared();
        let message = report().to_string();
        let document = verdict(
            &prepared,
            &transcript(Some(&message), false),
            TIMEOUT,
            Utc::now(),
            Instant::now(),
        );
        assert_eq!(document, report());
    }

    #[test]
    fn a_draft_in_the_log_is_not_the_verdict() {
        let (_dir, prepared) = prepared();
        let mut draft = report();
        draft["summary"] = json!("Draft while the round is still loading.");
        fs::write(
            prepared.dir.join("model.log"),
            format!("[PROGRESS] Building final JSON report\n{draft}\n"),
        )
        .unwrap();
        let document = verdict(
            &prepared,
            &transcript(None, false),
            TIMEOUT,
            Utc::now(),
            Instant::now(),
        );
        assert_eq!(document["status"], "test_failed");
        assert_eq!(
            document["error"]["message"],
            "the agent returned no final message."
        );
    }

    /// Feeds a claude event stream, line by line, through the harness's own relay and stores the
    /// verdict, the way `execute` does once the agent exits.
    fn verdict_of_stream(events: &[Value]) -> (Value, String) {
        let (_dir, prepared) = prepared();
        let mut log = Vec::new();
        let mut relay = crate::harness::Relay::new(std::io::sink(), &mut log, true);
        for event in events {
            relay.line(true, event.to_string()).unwrap();
        }
        let transcript = Transcript::from_stream(0, relay.finish());
        let document = verdict(&prepared, &transcript, TIMEOUT, Utc::now(), Instant::now());
        (document, String::from_utf8(log).unwrap())
    }

    #[test]
    fn a_draft_streamed_before_the_agent_stopped_is_not_the_verdict() {
        let mut draft = report();
        draft["summary"] = json!("Draft while the round is still loading.");
        let draft = draft.to_string();
        let say = |text: String| {
            json!({ "type": "assistant", "message": { "content": [
                { "type": "text", "text": text }
            ] } })
        };
        // The agent prints a draft, then stops without a result event.
        let (document, log) = verdict_of_stream(&[
            say("[PROGRESS] Building final JSON report".into()),
            say(draft.clone()),
        ]);
        assert!(
            log.lines().any(|line| line == draft),
            "the draft is in the model log: {log}"
        );
        assert_eq!(document["status"], "test_failed");
        assert_eq!(
            document["error"]["message"],
            "the agent returned no final message."
        );
    }

    #[test]
    fn an_error_result_without_text_is_explained_by_its_subtype() {
        let (document, log) = verdict_of_stream(&[json!({
            "type": "result", "subtype": "error_max_turns", "is_error": true
        })]);
        assert_eq!(document["status"], "test_failed");
        assert_eq!(
            document["error"]["message"],
            "the agent run ended in an error: error_max_turns, with no message."
        );
        assert!(log.contains("agent error: error_max_turns"), "{log}");
        let (document, _) = verdict_of_stream(&[json!({
            "type": "result", "subtype": "success", "is_error": false, "result": report().to_string()
        })]);
        assert_eq!(document, report());
    }

    #[test]
    fn an_error_result_is_test_failed_even_when_it_holds_a_report() {
        let (_dir, prepared) = prepared();
        let message = report().to_string();
        let document = verdict(
            &prepared,
            &transcript(Some(&message), true),
            TIMEOUT,
            Utc::now(),
            Instant::now(),
        );
        assert_eq!(document["status"], "test_failed");
        let why = document["error"]["message"].as_str().unwrap();
        assert!(
            why.starts_with("the agent run ended in an error: "),
            "{why}"
        );
        let long = "x".repeat(ERROR_PREVIEW * 2);
        let why = final_report(&transcript(Some(&long), true), TIMEOUT, PREFIX)
            .unwrap_err()
            .to_string();
        assert!(
            why.len() < ERROR_PREVIEW + 50 && why.ends_with('…'),
            "{why}"
        );
        assert!(
            final_report(&transcript(None, true), TIMEOUT, PREFIX)
                .unwrap_err()
                .to_string()
                .contains("ended in an error")
        );
    }

    #[test]
    fn reads_only_one_json_object_from_the_final_message() {
        let report = report().to_string();
        for message in [
            report.clone(),
            format!("\n  {report}\n"),
            format!("```json\n{report}\n```"),
            format!("```\n{report}\n```\n"),
            format!("```JSON\n{report}\n```"),
            format!("```jsonc\r\n{report}\r\n```"),
            format!("\u{feff}{report}"),
            format!("\u{feff}```json\n{report}\n```"),
            format!("```{report}```"),
        ] {
            assert_eq!(
                final_report(&transcript(Some(&message), false), TIMEOUT, PREFIX).unwrap()["status"],
                "working"
            );
        }
        let draft = r#"{"schema_version": 1, "status": "failing"}"#;
        for message in [
            format!("Here is the report:\n{report}"),
            format!("{report}\nDone."),
            format!("{draft}\n{report}"),
            format!("```json\n{report}\n```\nDone."),
            format!("```json {report}```"),
            format!("```json\nHere it is:\n{report}\n```"),
            "{\"status\": \"working\"}".to_owned(),
            String::new(),
        ] {
            assert!(
                final_report(&transcript(Some(&message), false), TIMEOUT, PREFIX).is_err(),
                "{message}"
            );
        }
    }

    #[test]
    fn ignores_the_schema_echoed_from_the_prompt() {
        assert!(parse_report(crate::prompt::REPORT_SCHEMA).is_err());
    }
}
