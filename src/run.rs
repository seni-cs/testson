//! One game test in one mode: run directories, prompt, agent invocation, the replay of the script
//! an agent run left, and the stored tests.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Local, SecondsFormat, Utc};
use clap::ValueEnum;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::catalog::{Fetcher, Game};
use crate::db;
use crate::harness::{Agent, find_on_path};
use crate::prompt::{
    CAPTURE_MESSAGES, COLLECT_MESSAGES, NO_BALANCE_INSTRUCTIONS, REAL_MONEY_INSTRUCTIONS, TEMPLATE,
    TemplateValues, fill_template, session_has_auth, slugify,
};
use crate::report::{self, GameVersion};
use crate::router::{ASSETS_DIR, ASSETS_URL};
use crate::script::{self, CAPTURE_FILE, COLLECT_FILE};

/// Which test account a test plays with, and so what it checks. Every game is tested in both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The account with balance plays one real-money round at the minimum stake.
    RealMoney,
    /// The account without balance tries to bet, and the game must say the balance is short.
    NoBalance,
}

impl Mode {
    pub const ALL: [Self; 2] = [Self::RealMoney, Self::NoBalance];

    /// The name `schema/test-game.schema.json` and the database give the mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RealMoney => "real_money",
            Self::NoBalance => "no_balance",
        }
    }

    /// The saved login of the account of this mode, in the data directory: a Playwright storage
    /// state that `testson login` writes.
    #[must_use]
    pub fn session_file(self) -> &'static str {
        match self {
            Self::RealMoney => "session-real-money.json",
            Self::NoBalance => "session-no-balance.json",
        }
    }

    /// The username and password `testson login` logs in to the account of this mode with.
    #[must_use]
    pub fn credentials_file(self) -> &'static str {
        match self {
            Self::RealMoney => "credentials-real-money.env",
            Self::NoBalance => "credentials-no-balance.env",
        }
    }

    /// The Chrome profile `testson login` logs in to the account of this mode in, so the two
    /// accounts never share cookies.
    #[must_use]
    pub fn browser_profile(self) -> &'static str {
        match self {
            Self::RealMoney => "browser-real-money",
            Self::NoBalance => "browser-no-balance",
        }
    }

    fn instructions(self) -> &'static str {
        match self {
            Self::RealMoney => REAL_MONEY_INSTRUCTIONS,
            Self::NoBalance => NO_BALANCE_INSTRUCTIONS,
        }
    }

    /// Used in the names of the run directories and the browser session.
    fn slug(self) -> &'static str {
        match self {
            Self::RealMoney => "real-money",
            Self::NoBalance => "no-balance",
        }
    }
}

impl std::fmt::Display for Mode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.slug())
    }
}

/// What tests a game: the agent, or the script an agent run left for the version of the game.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Runner {
    /// The script when the game has one, and the agent when it has none or it no longer fits.
    Auto,
    /// The agent, whether or not there is a script.
    Agent,
    /// The script only, without falling back to the agent.
    Script,
}

/// Private run directories, beside the assets the dashboard serves. They hold the session.
pub const RUNS_DIR: &str = "runs";

/// The copy of the saved session in a run directory, which the agent loads by its path.
pub const SESSION_COPY: &str = "session.json";

/// The storage state of a run whose mode has no saved session: it restores nothing.
const EMPTY_SESSION: &str = "{\"cookies\": [], \"origins\": []}\n";

/// How long one agent run may take before it is stopped.
const RUN_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// The run directories, written before the agent starts.
#[derive(Debug)]
pub struct Prepared {
    /// Private: the saved session, the postMessage scripts, the report and, for the agent, the
    /// filled prompt, the model log, the script it wrote and the trace.
    pub dir: PathBuf,
    /// Served by the dashboard: the video and the screenshots.
    pub assets_dir: PathBuf,
    /// URL path `assets_dir` is served at.
    pub data_prefix: String,
    pub url: String,
    pub mode: Mode,
    /// The playwright-cli session the agent drives the browser in.
    pub session_name: String,
    pub prompt: String,
    pub session_missing: bool,
}

/// A finished run.
#[derive(Debug)]
pub struct Outcome {
    pub report_path: PathBuf,
    /// The JSON document of the test, as stored.
    pub document: Value,
    /// `None` when the agent was stopped for taking too long. A replay runs no agent, and counts
    /// as `Some(0)`.
    pub agent_exit_code: Option<i32>,
    /// The replay script the agent left, once it follows the schema and the safety rules.
    pub script: Option<String>,
}

impl Outcome {
    fn text(&self, key: &str) -> &str {
        self.document.get(key).and_then(Value::as_str).unwrap_or("")
    }

    /// Why the script of a replay is retired, `step_failed` or `version_changed`, when it
    /// stopped because the script no longer fits the game, which the agent then tests again. A
    /// step that failed after the spin retires it as `step_failed`.
    #[must_use]
    pub fn script_broke(&self) -> Option<&'static str> {
        match self.text("script_outcome") {
            "step_failed" | "failed_after_spin" => Some("step_failed"),
            "version_changed" => Some("version_changed"),
            _ => None,
        }
    }

    /// The replay broke after it pressed spin. The agent would play another round, so it tests
    /// the game at a later test instead of now.
    #[must_use]
    pub fn broke_after_spin(&self) -> bool {
        self.text("script_outcome") == "failed_after_spin"
    }

    /// `working`, `failing`, or `test_failed`.
    #[must_use]
    pub fn status(&self) -> &str {
        self.text("status")
    }

    /// The game behaved as the mode expects.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.agent_exit_code == Some(0) && self.status() == "working"
    }

    /// The test broke because the saved session no longer logs in. The prompt has the agent say
    /// so by naming `testson login`.
    #[must_use]
    pub fn needs_login(&self) -> bool {
        self.status() == "test_failed"
            && self
                .document
                .pointer("/error/message")
                .and_then(Value::as_str)
                .is_some_and(|message| message.contains("testson login"))
    }

    /// The test reached a verdict on the game. `test_failed` means the test itself broke and is
    /// worth retrying.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.agent_exit_code == Some(0) && matches!(self.status(), "working" | "failing")
    }
}

/// The tests one call of [`test_game`] ran, in order: a replay, the agent, the replay of the
/// script the agent left, or some of them.
#[derive(Debug, Default)]
pub struct Tested {
    pub outcomes: Vec<Outcome>,
}

impl Tested {
    /// The latest of the tests that reached a verdict found the game working.
    #[must_use]
    pub fn passed(&self) -> bool {
        self.outcomes
            .iter()
            .rfind(|outcome| outcome.finished())
            .is_some_and(Outcome::passed)
    }

    /// One of the tests reached a verdict on the game.
    #[must_use]
    pub fn finished(&self) -> bool {
        self.outcomes.iter().any(Outcome::finished)
    }

    /// None of the tests reached a verdict, and one broke because the saved session no longer
    /// logs in.
    #[must_use]
    pub fn needs_login(&self) -> bool {
        !self.finished() && self.outcomes.iter().any(Outcome::needs_login)
    }
}

/// Tests a game in one mode, stores every test it runs, and prints a summary of each.
///
/// With [`Runner::Auto`], a game whose current version has a script is replayed without an
/// agent. When the replay stops because the script no longer fits the game, the agent tests it.
/// An agent test that finds the game working leaves a script, which is replayed at once, as a
/// test of its own. The game must already be stored, since its tests refer to it.
///
/// # Errors
///
/// Fails when a tool is missing, the ping fails, the run directories cannot be written, the
/// agent cannot start, a test cannot be stored, or [`Runner::Script`] finds no script.
pub fn test_game(
    data_dir: &Path,
    fetcher: &impl Fetcher,
    game: &Game,
    url: &str,
    mode: Mode,
    agent: &Agent,
    runner: Runner,
) -> Result<Tested> {
    ensure_playwright_cli()?;
    let current = current_version(data_dir, fetcher, &game.id)?;
    let mut tested = Tested::default();
    if runner != Runner::Agent {
        // A new version of the game is found by the replay, before it bets: the ETag a request
        // of testson gets may not be the one the browser gets.
        match db::active_script(data_dir, &game.id, mode.as_str())? {
            Some(stored) => {
                let outcome = replay_script(data_dir, game, url, mode, &stored, current.as_ref())?;
                let broke = outcome.script_broke();
                let after_spin = outcome.broke_after_spin();
                tested.outcomes.push(outcome);
                let Some(reason) = broke else {
                    return Ok(tested);
                };
                db::retire_script(data_dir, stored.id, reason)?;
                if after_spin {
                    eprintln!(
                        "warning: the script of {} ({mode}) failed after its spin, so the agent \
                         tests it at the next test rather than play another round now.",
                        game.name
                    );
                    return Ok(tested);
                }
                if runner == Runner::Script {
                    return Ok(tested);
                }
                println!(
                    "The script no longer fits {} ({mode}): the agent tests it.",
                    game.name
                );
            }
            None if runner == Runner::Script => bail!(
                "{} has no script for {mode}. Test it with the agent first: --runner agent",
                game.name
            ),
            None => {}
        }
    }
    let (outcome, test_id) = run_agent(data_dir, game, url, mode, agent, current.as_ref())?;
    let script = outcome.script.clone().filter(|_| outcome.passed());
    tested.outcomes.push(outcome);
    if let Some(script) = script {
        let script_id = db::insert_script(data_dir, &game.id, &script, test_id)?;
        println!(
            "Saved script {script_id} for {} ({mode}). Replaying it.",
            game.name
        );
        let stored = db::StoredScript {
            id: script_id,
            script,
        };
        let replayed = replay_script(data_dir, game, url, mode, &stored, current.as_ref())?;
        if let Some(reason) = replayed.script_broke() {
            db::retire_script(data_dir, stored.id, reason)?;
            eprintln!(
                "warning: the script of {} ({mode}) did not replay, so the agent tests it again \
                 next time.",
                game.name
            );
        }
        tested.outcomes.push(replayed);
    }
    Ok(tested)
}

/// The version of the game its provider serves now: the version of the file of the game client
/// the latest test of the game read, asked for again without a browser. `None` when no test has seen one yet
/// or the server does not answer with one.
///
/// # Errors
///
/// Fails when the database cannot be read. A server that does not answer is only warned about.
pub fn current_version(
    data_dir: &Path,
    fetcher: &impl Fetcher,
    game_id: &str,
) -> Result<Option<GameVersion>> {
    let Some(known) = db::latest_version(data_dir, game_id)? else {
        return Ok(None);
    };
    match fetcher.etag(&known.build_url) {
        Ok(Some(etag)) => Ok(Some(GameVersion {
            build_url: known.build_url,
            etag,
        })),
        Ok(None) => {
            eprintln!(
                "warning: {} came without an ETag or a Last-Modified date.",
                known.build_url
            );
            Ok(None)
        }
        Err(error) => {
            eprintln!("warning: could not check the version of the game: {error:#}");
            Ok(None)
        }
    }
}

/// Pings the agent, runs the test with it, stores the test, prints a summary, and returns the
/// test and the number it is stored under.
fn run_agent(
    data_dir: &Path,
    game: &Game,
    url: &str,
    mode: Mode,
    agent: &Agent,
    current: Option<&GameVersion>,
) -> Result<(Outcome, i64)> {
    agent.ensure_installed()?;
    println!("Pinging {agent}...");
    agent.ping()?;
    let prepared = prepare(data_dir, game, url, mode)?;
    if prepared.session_missing {
        eprintln!(
            "warning: {} was not found. The agent will run without a restored login. Run \
             `testson login --mode {mode}` to save one.",
            data_dir.join(mode.session_file()).display()
        );
    }
    println!("Game:   {} ({})", game.name, game.id);
    println!("Mode:   {mode}");
    println!("URL:    {url}");
    println!("Agent:  {agent}");
    println!("Output: {}", prepared.dir.display());
    let outcome = execute(&prepared, agent, current);
    close_browser(&prepared);
    let outcome = outcome?;
    let test_id = store(data_dir, game, &prepared, &outcome)?;
    match outcome.agent_exit_code {
        Some(0) => {}
        Some(code) => eprintln!(
            "error: {} exited with status {code}. The report above is what it returned.",
            agent.harness
        ),
        None => eprintln!(
            "error: {} was stopped after {} minutes.",
            agent.harness,
            RUN_TIMEOUT.as_secs() / 60
        ),
    }
    Ok((outcome, test_id))
}

/// Replays a stored script, stores the test, and prints a summary.
fn replay_script(
    data_dir: &Path,
    game: &Game,
    url: &str,
    mode: Mode,
    stored: &db::StoredScript,
    current: Option<&GameVersion>,
) -> Result<Outcome> {
    let prepared = prepare_run(data_dir, &game.name, url, mode)?;
    println!("Game:   {} ({})", game.name, game.id);
    println!("Mode:   {mode}");
    println!("Script: {}", stored.id);
    println!("Output: {}", prepared.dir.display());
    let mut document = match script::parse(&stored.script, mode) {
        // A script that met the rules when it was saved may not meet stricter ones.
        Err(error) => not_replayed(
            &prepared,
            stored.id,
            "step_failed",
            "The script no longer meets the rules of a replay, so it was not run and the agent \
             tests the game again.",
            &format!("{error:#}"),
        ),
        // Without a login, the replay would only find the lobby logged out.
        Ok(_) if !has_login(&prepared) => not_replayed(
            &prepared,
            stored.id,
            "blocked",
            "The saved session holds no login, so the replay did not start.",
            &format!("Save the session again with `testson login --mode {mode}`."),
        ),
        Ok(parsed) => {
            let mut browser = script::Cli {
                program: find_on_path("playwright-cli").unwrap_or_else(|| "playwright-cli".into()),
                session: prepared.session_name.clone(),
                dir: prepared.dir.clone(),
            };
            let context = script::Replay {
                mode,
                url,
                script_id: stored.id,
                run_dir: &prepared.dir,
                assets_dir: &prepared.assets_dir,
                data_prefix: &prepared.data_prefix,
            };
            let document = script::replay(&mut browser, &context, &parsed);
            close_browser(&prepared);
            document
        }
    };
    record_version(&mut document, current);
    let report_path = prepared.dir.join("report.json");
    write_private(
        &report_path,
        &(serde_json::to_string_pretty(&document)? + "\n"),
    )?;
    let outcome = Outcome {
        report_path,
        document,
        agent_exit_code: Some(0),
        script: None,
    };
    store(data_dir, game, &prepared, &outcome)?;
    Ok(outcome)
}

/// The test of a replay that did not start: the script no longer meets the rules, or there is no
/// login.
fn not_replayed(
    prepared: &Prepared,
    script_id: i64,
    outcome: &str,
    summary: &str,
    why: &str,
) -> Value {
    json!({
        "schema_version": 3,
        "mode": prepared.mode.as_str(),
        "runner": "script",
        "script_id": script_id,
        "script_outcome": outcome,
        "status": "test_failed",
        "summary": summary,
        "game_url": prepared.url,
        "started_at": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
        "duration_ms": 0,
        "steps": [],
        "error": { "message": why },
    })
}

/// Whether the copy of the saved session in a run directory holds a Betsson login.
fn has_login(prepared: &Prepared) -> bool {
    fs::read_to_string(prepared.dir.join(SESSION_COPY))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|state| session_has_auth(&state))
}

/// Records on a test the version checked before it ran, which `watch` compares with the version
/// it checks next, the same way. The version the browser saw is kept only when there is none:
/// the server may send the browser another ETag than it sends testson, and the replay compares
/// the browser's with the script's. A test that saw no version, such as a refusal of the lobby
/// before the game loaded, still counts for the version checked.
fn record_version(document: &mut Value, current: Option<&GameVersion>) {
    if let (Some(current), Some(object)) = (current, document.as_object_mut()) {
        object.insert("game_version".to_owned(), json!(current));
    }
}

/// Stores a test and prints its summary.
fn store(data_dir: &Path, game: &Game, prepared: &Prepared, outcome: &Outcome) -> Result<i64> {
    let test_id = db::insert_test(data_dir, &game.id, &outcome.document.to_string())
        .with_context(|| format!("could not store the test of {}", game.name))?;
    println!();
    println!("Runner:    {}", outcome.text("runner"));
    println!("Status:    {}", outcome.status());
    println!("Summary:   {}", outcome.text("summary"));
    if let Some(message) = outcome
        .document
        .pointer("/error/message")
        .and_then(Value::as_str)
    {
        println!("Error:     {message}");
    }
    if let Some(etag) = outcome
        .document
        .pointer("/game_version/etag")
        .and_then(Value::as_str)
    {
        println!("Version:   {etag}");
    }
    if !outcome.text("video_path").is_empty() {
        println!("Video:     {}", outcome.text("video_path"));
    }
    println!("Report:    {}", outcome.report_path.display());
    // Only the agent records a trace.
    let traces = prepared.dir.join(".playwright-cli").join("traces");
    if traces.is_dir() {
        println!("Trace:     {}", traces.display());
    }
    println!(
        "Dashboard: http://{}/game/{}#test-{test_id}",
        crate::ADDRESS,
        game.id
    );
    Ok(test_id)
}

/// Fails with install instructions when `playwright-cli` is not on `PATH`.
///
/// # Errors
///
/// Fails when it is missing.
pub fn ensure_playwright_cli() -> Result<()> {
    if find_on_path("playwright-cli").is_none() {
        bail!(
            "playwright-cli is not on PATH.\nInstall it with: npm install -g @playwright/cli@0.1.22 \
             && playwright-cli install-browser chromium"
        );
    }
    Ok(())
}

/// Writes the run directories of an agent run, as [`prepare_run`] does, and fills the prompt.
///
/// # Errors
///
/// Fails when the session file of the mode is not a storage state or a run directory cannot be
/// written.
pub fn prepare(data_dir: &Path, game: &Game, url: &str, mode: Mode) -> Result<Prepared> {
    let mut prepared = prepare_run(data_dir, &game.name, url, mode)?;
    // The agent checks its script with this same program, so it learns what a replay refuses
    // while it can still fix it.
    let program = std::env::current_exe()
        .ok()
        .and_then(|path| path.to_str().map(str::to_owned))
        .unwrap_or_else(|| "testson".to_owned());
    let check_script = format!(
        "'{program}' check-script '{}' --mode {mode}",
        absolute(&prepared.dir.join("script.json"))?.display()
    );
    prepared.prompt = fill_template(
        TEMPLATE,
        TemplateValues {
            game_name: &game.name,
            provider: or_unknown(&game.provider),
            game_type: or_unknown(&game.primary_type),
            mode: mode.as_str(),
            mode_instructions: mode.instructions(),
            betsson_url: url,
            output_dir: absolute(&prepared.dir)?
                .to_str()
                .context("run directory is not valid UTF-8")?,
            assets_dir: absolute(&prepared.assets_dir)?
                .to_str()
                .context("assets directory is not valid UTF-8")?,
            session_name: &prepared.session_name,
            data_prefix: &prepared.data_prefix,
            check_script: &check_script,
        },
    )?;
    write_private(&prepared.dir.join("prompt.filled.txt"), &prepared.prompt)?;
    Ok(prepared)
}

/// What the catalogue says of a game, or that it does not say.
fn or_unknown(text: &str) -> &str {
    if text.is_empty() { "unknown" } else { text }
}

/// Writes `runs/<slug>-<mode>-<timestamp>/` and `assets/<slug>-<mode>-<timestamp>/` under
/// `data_dir`, with the restore script and the postMessage scripts in the private one. The
/// prompt is left empty: a replay needs none.
///
/// # Errors
///
/// Fails when the session file of the mode still has placeholders or a run directory cannot be
/// written.
pub fn prepare_run(data_dir: &Path, game_name: &str, url: &str, mode: Mode) -> Result<Prepared> {
    let (session, session_missing) = load_session(&data_dir.join(mode.session_file()))?;
    let slug = format!("{}-{mode}", slugify(game_name));
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
    let session_name = format!("betsson-{folder}");
    write_private(&dir.join(SESSION_COPY), &session)?;
    write_private(&dir.join(CAPTURE_FILE), CAPTURE_MESSAGES)?;
    write_private(&dir.join(COLLECT_FILE), COLLECT_MESSAGES)?;
    Ok(Prepared {
        dir,
        assets_dir,
        data_prefix,
        url: url.to_owned(),
        mode,
        session_name,
        prompt: String::new(),
        session_missing,
    })
}

/// Runs the agent in the run directory and writes `report.json`, the document to store.
///
/// A non-zero agent status still returns the report the agent produced. When the agent returns
/// no report, one that does not follow the schema, or is stopped after [`RUN_TIMEOUT`], the
/// document is a `test_failed` test that says why, so the run still shows up on the dashboard.
/// The mode of the document is the one the run was prepared for, whatever the agent wrote, and
/// its runner is the agent. The replay script the agent wrote is kept when it follows the schema
/// and the safety rules, and is written for the version of the game the report names. The
/// version the document records is then `current`, as [`record_version`] says.
///
/// # Errors
///
/// Fails when the agent cannot start or a file cannot be written.
pub fn execute(
    prepared: &Prepared,
    agent: &Agent,
    current: Option<&GameVersion>,
) -> Result<Outcome> {
    let log_path = prepared.dir.join("model.log");
    let report_path = prepared.dir.join("report.json");
    let started_at = Utc::now();
    let clock = Instant::now();
    let mut log = create_private(&log_path)?;
    writeln!(log, "# agent: {agent}")?;
    let transcript = agent.run(&prepared.prompt, &prepared.dir, &mut log, RUN_TIMEOUT)?;
    drop(log);
    let log_text = fs::read_to_string(&log_path).unwrap_or_default();
    let report = if transcript.exit_code.is_none() {
        Err(anyhow::anyhow!(
            "the agent did not finish within {} minutes and was stopped",
            RUN_TIMEOUT.as_secs() / 60
        ))
    } else {
        transcript
            .last_message
            .as_deref()
            .and_then(|message| extract_report(message).ok())
            .map_or_else(
                || extract_report(after_echoed_prompt(&log_text, &prepared.prompt)),
                Ok,
            )
            .and_then(|mut report| {
                if let Some(object) = report.as_object_mut() {
                    // The trace records the session, so it is never served or linked.
                    object.remove("trace_path");
                    object.remove("script_id");
                    object.remove("script_outcome");
                    object.insert("mode".to_owned(), json!(prepared.mode.as_str()));
                    object.insert("runner".to_owned(), json!("agent"));
                }
                report::validate(&report)
                    .context("the report does not follow the schema")
                    .map(|()| report)
            })
    };
    let mut document = report.unwrap_or_else(|error| {
        eprintln!("error: {error:#}. See {}", log_path.display());
        let duration = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        test_failed(prepared, started_at, duration, &format!("{error:#}."))
    });
    // The script is checked against the version the agent saw, before it is replaced.
    let script = written_script(prepared, &document);
    record_version(&mut document, current);
    write_private(
        &report_path,
        &(serde_json::to_string_pretty(&document)? + "\n"),
    )?;
    Ok(Outcome {
        report_path,
        document,
        agent_exit_code: transcript.exit_code,
        script,
    })
}

/// The replay script the agent wrote to `script.json`, when it follows the schema and the
/// safety rules and is written for the version of the game the report names.
fn written_script(prepared: &Prepared, document: &Value) -> Option<String> {
    let path = prepared.dir.join("script.json");
    let text = fs::read_to_string(&path).ok()?;
    let checked = script::parse(&text, prepared.mode).and_then(|script| {
        let reported = document
            .pointer("/game_version/etag")
            .and_then(Value::as_str);
        if reported.is_some_and(|etag| !report::same_version(etag, &script.game_version.etag)) {
            return Err(anyhow!(
                "it was written for ETag {}, but the report names {}",
                script.game_version.etag,
                reported.unwrap_or_default()
            ));
        }
        Ok(())
    });
    match checked {
        Ok(()) => Some(text),
        Err(error) => {
            eprintln!(
                "warning: {} is not kept as a script: {error:#}",
                path.display()
            );
            None
        }
    }
}

/// A test that broke before the agent could report on the game, with whatever evidence it left.
fn test_failed(prepared: &Prepared, started_at: DateTime<Utc>, duration: u64, why: &str) -> Value {
    let mut document = json!({
        "schema_version": 3,
        "mode": prepared.mode.as_str(),
        "runner": "agent",
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

/// Closes the browser the agent opened, which it may have left open when it failed or was
/// stopped.
fn close_browser(prepared: &Prepared) {
    let _ = playwright_cli(&prepared.dir, &prepared.session_name, "close", &[]);
}

/// Runs `playwright-cli --session <session> <command> <args>` in `dir`, and fails with what it
/// printed when it fails.
///
/// # Errors
///
/// Fails when `playwright-cli` cannot start or exits with an error.
pub fn playwright_cli(dir: &Path, session: &str, command: &str, args: &[&str]) -> Result<Output> {
    let program = find_on_path("playwright-cli").unwrap_or_else(|| "playwright-cli".into());
    let output = Command::new(program)
        .args(["--session", session, command])
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .context("playwright-cli failed to start")?;
    if !output.status.success() {
        bail!(
            "playwright-cli {command} failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
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
    last_json_object(output, |value| {
        value.get("schema_version").is_some_and(Value::is_u64)
            && value.get("status").is_some_and(Value::is_string)
    })
    .context("model output did not contain a JSON report with schema_version")
}

/// Returns the last JSON object in `output` that `accept` takes.
fn last_json_object(output: &str, accept: impl Fn(&Value) -> bool) -> Option<Value> {
    output.match_indices('{').rev().find_map(|(start, _)| {
        let mut deserializer = serde_json::Deserializer::from_str(&output[start..]);
        Value::deserialize(&mut deserializer)
            .ok()
            .filter(|value| accept(value))
    })
}

/// Reads the saved session of a mode, or an empty one when there is none, and says whether it was
/// missing.
fn load_session(path: &Path) -> Result<(String, bool)> {
    if !path.is_file() {
        return Ok((EMPTY_SESSION.to_owned(), true));
    }
    set_mode(path, 0o600);
    let text =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    let state: Value = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not a saved session. Save it again with `testson login`.",
            path.display()
        )
    })?;
    if !session_has_auth(&state) {
        eprintln!(
            "warning: {} holds no Betsson login. Save it again with `testson login`.",
            path.display()
        );
    }
    Ok((text, false))
}

/// Creates a private run directory named so that neither it nor its assets directory exists.
pub fn create_run_dir(runs: &Path, assets: &Path, name: &str) -> Result<PathBuf> {
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
pub fn absolute(path: &Path) -> Result<PathBuf> {
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

pub fn write_private(path: &Path, contents: &str) -> Result<()> {
    create_private(path)?
        .write_all(contents.as_bytes())
        .with_context(|| format!("could not write {}", path.display()))
}

pub fn set_mode(path: &Path, mode: u32) {
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

    /// A slot of Relax named `name`.
    fn named(name: &str) -> Game {
        Game {
            id: slugify(name),
            name: name.to_owned(),
            provider: "Relax".into(),
            primary_type: "Slots".into(),
            is_available_for_fun: true,
            image_url: String::new(),
        }
    }

    const SESSION: &str = r#"{"cookies": [], "origins": [{"origin": "https://www.betsson.es", "localStorage": [{"name": "session", "value": "token-7"}]}]}"#;

    #[test]
    fn prepare_writes_a_private_prompt_and_session() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(Mode::RealMoney.session_file()), SESSION).unwrap();
        let prepared = prepare(
            dir.path(),
            &named("Los Piratas Golfetes"),
            "https://www.betsson.es/casino",
            Mode::RealMoney,
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
                .starts_with("/data/assets/los-piratas-golfetes-real-money-")
        );
        assert!(prepared.prompt.contains("Game name: Los Piratas Golfetes"));
        assert!(prepared.prompt.contains("Provider: Relax"));
        assert!(prepared.prompt.contains("Game type: Slots"));
        assert!(prepared.prompt.contains("Mode real_money"));
        // The session must never land where the dashboard serves files from.
        assert!(
            fs::read_dir(&prepared.assets_dir)
                .unwrap()
                .all(|entry| { entry.unwrap().file_name() == "screenshots" })
        );
        // The login stays out of the prompt, which the model provider keeps.
        assert!(!prepared.prompt.contains("token-7"));
        assert!(prepared.prompt.contains(SESSION_COPY));
        let copy = fs::read_to_string(prepared.dir.join(SESSION_COPY)).unwrap();
        assert_eq!(copy, SESSION);
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
    fn prepare_rejects_a_broken_session_and_notes_a_missing_one() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://www.betsson.es/casino";
        let prepared = prepare(dir.path(), &named("Alpha"), url, Mode::NoBalance).unwrap();
        assert!(prepared.session_missing);
        assert!(prepared.prompt.contains("Mode no_balance"));
        let copy = fs::read_to_string(prepared.dir.join(SESSION_COPY)).unwrap();
        assert_eq!(copy, EMPTY_SESSION);
        fs::write(
            dir.path().join(Mode::NoBalance.session_file()),
            "localStorage.setItem('session', 'old format');\n",
        )
        .unwrap();
        let error = prepare(dir.path(), &named("Alpha"), url, Mode::NoBalance).unwrap_err();
        assert!(error.to_string().contains("testson login"));
        // Each mode reads the session of its own account.
        assert!(prepare(dir.path(), &named("Alpha"), url, Mode::RealMoney).is_ok());
    }

    #[test]
    fn only_a_clean_pass_passes_and_only_test_failed_is_retried() {
        let outcome = |status: &str, code: i32| Outcome {
            report_path: PathBuf::new(),
            document: json!({ "status": status }),
            agent_exit_code: Some(code),
            script: None,
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
            "schema_version": 3,
            "mode": "real_money",
            "runner": "agent",
            "status": "working",
            "summary": "Played one real-money round.",
            "started_at": "2026-10-12T10:00:00+02:00",
            "duration_ms": 1200,
            "steps": [{ "description": "Opened the lobby", "status": "passed" }],
        })
    }

    #[test]
    fn a_run_without_a_report_is_stored_as_test_failed() {
        let dir = tempfile::tempdir().unwrap();
        let prepared = prepare(
            dir.path(),
            &named("Alpha"),
            "https://www.betsson.es/casino",
            Mode::NoBalance,
        )
        .unwrap();
        fs::write(prepared.assets_dir.join("session.webm"), "").unwrap();
        let document = test_failed(&prepared, Utc::now(), 5, "no report.");
        report::validate(&document).unwrap();
        assert_eq!(document["status"], "test_failed");
        assert_eq!(document["mode"], "no_balance");
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
