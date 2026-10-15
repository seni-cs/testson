/// Writes a line on a standard stream, best effort: `say!` on stdout, `esay!` on stderr.
///
/// Rust ignores `SIGPIPE`, so `println!` and `eprintln!` panic once a reader such as `| head -1`
/// exits or the terminal goes away. The output is only for the operator, so a line that cannot
/// be written is dropped and the command carries on.
///
/// The tests keep `println!` and `eprintln!`, because the test harness captures only those.
macro_rules! write_line {
    ($stream:ident, $fallback:ident; $($arg:tt)*) => {{
        #[cfg(not(test))]
        {
            use std::io::Write as _;
            let _ = writeln!(std::io::$stream(), $($arg)*);
        }
        #[cfg(test)]
        $fallback!($($arg)*);
    }};
}

/// `println!` that does not panic when stdout is closed. See `write_line!`.
macro_rules! say {
    ($($arg:tt)*) => { write_line!(stdout, println; $($arg)*) };
}

/// `eprintln!` that does not panic when stderr is closed. See `write_line!`.
macro_rules! esay {
    ($($arg:tt)*) => { write_line!(stderr, eprintln; $($arg)*) };
}

mod browser;
mod catalog;
mod db;
mod events;
mod harness;
mod prompt;
mod router;
mod run;
mod watch;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::builder::BoolishValueParser;
use clap::{Args, Parser, Subcommand};
use tokio::net::TcpListener;

use crate::browser::Window;
use crate::catalog::{
    DEFAULT_CASINO_URL, Fetcher, Http, NameMatch, fetch_novedades, find_game, find_game_by_id,
};
use crate::harness::{Agent, DEFAULT_TIMEOUT, Harness, handle_interrupts, parse_timeout};
use crate::prompt::{parse_betsson_url, slugify};
use crate::run::{MissingSession, Outcome};
use crate::watch::{Tried, WatchLock, parse_interval};

const ADDRESS: &str = "127.0.0.1:8000";

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Directory that holds the database, the assets, the runs, and session-block.txt.
    #[arg(
        long,
        global = true,
        env = "TESTSON_DATA_DIR",
        default_value = "data-dir"
    )]
    data_dir: PathBuf,
}

#[derive(Args)]
struct AgentArgs {
    /// Agent CLI that drives the browser.
    #[arg(long, env = "TESTSON_HARNESS", value_enum, default_value_t = Harness::Claude)]
    harness: Harness,

    /// Model passed to the harness. Defaults to sonnet for claude and to the CLI's own default
    /// for the others.
    #[arg(long, env = "TESTSON_MODEL")]
    model: Option<String>,

    /// Reasoning effort passed to the harness. Defaults to medium for claude.
    #[arg(long, env = "TESTSON_EFFORT")]
    effort: Option<String>,
}

impl AgentArgs {
    fn agent(self) -> Agent {
        Agent::new(self.harness, self.model, self.effort)
    }
}

#[derive(Args)]
struct BrowserArgs {
    /// Run the browser without a window. It always does without a display, such as on a Linux
    /// server with no X11 or Wayland session.
    #[arg(long, env = "TESTSON_HEADLESS", value_parser = BoolishValueParser::new())]
    headless: bool,
}

impl BrowserArgs {
    fn window(&self) -> Window {
        Window::choose(self.headless)
    }
}

#[derive(Args)]
struct RunArgs {
    /// Stop the agent when a run takes longer than this, for example 20m, and store the test as
    /// broken. Minimum 60s.
    #[arg(long, env = "TESTSON_TIMEOUT", value_name = "DURATION", default_value = DEFAULT_TIMEOUT, value_parser = parse_timeout)]
    timeout: Duration,
}

#[derive(Subcommand)]
enum Command {
    /// Start the dashboard
    Serve,
    /// Test one game in demo mode and store the result
    Test {
        /// Name of the game, as shown in the lobby.
        #[arg(value_parser = non_empty)]
        game: String,

        /// Identifier of the game in the catalogue, such as nyxAviator, for when several games
        /// share its name.
        #[arg(long, value_name = "ID", value_parser = non_empty_id)]
        game_id: Option<String>,

        /// Lobby or game URL. Must be https on www.betsson.es or betsson.es.
        #[arg(long, default_value = DEFAULT_CASINO_URL, value_parser = parse_betsson_url)]
        url: String,

        #[command(flatten)]
        agent: AgentArgs,

        #[command(flatten)]
        browser: BrowserArgs,

        #[command(flatten)]
        run: RunArgs,
    },
    /// Check that the harness answers with the chosen model, without running a test
    Ping {
        #[command(flatten)]
        agent: AgentArgs,
    },
    /// Demo-test the games that newly appear on Novedades
    Watch {
        /// Print the current games and do not save or test.
        #[arg(long, conflicts_with = "every")]
        list: bool,

        /// Repeat the check at this interval, for example 30m. Minimum 60s.
        #[arg(long, value_name = "INTERVAL", value_parser = parse_interval)]
        every: Option<Duration>,

        /// Tries a queued game gets without a verdict before it leaves the queue.
        #[arg(
            long,
            env = "TESTSON_MAX_ATTEMPTS",
            default_value_t = 3,
            value_parser = clap::value_parser!(u32).range(1..)
        )]
        max_attempts: u32,

        #[command(flatten)]
        agent: AgentArgs,

        #[command(flatten)]
        browser: BrowserArgs,

        #[command(flatten)]
        run: RunArgs,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            esay!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Run the command, and say whether it succeeded. A test that ran but did not pass did not.
fn run() -> Result<bool> {
    let cli = Cli::parse();
    let data_dir = cli.data_dir.as_path();
    match cli.command {
        Command::Serve => {
            let runtime = tokio::runtime::Runtime::new()?;
            runtime.block_on(serve(data_dir))?;
            Ok(true)
        }
        Command::Test {
            game,
            game_id,
            url,
            agent,
            browser,
            run,
        } => {
            handle_interrupts()?;
            test(
                data_dir,
                &game,
                game_id.as_deref(),
                &url,
                &agent.agent(),
                browser.window(),
                run.timeout,
            )
        }
        Command::Ping { agent } => {
            let agent = agent.agent();
            handle_interrupts()?;
            agent.ensure_installed()?;
            agent.ping()?;
            say!("{agent} answered.");
            Ok(true)
        }
        Command::Watch { list: true, .. } => {
            for game in fetch_novedades(&Http)? {
                say!("{}\t{}", game.id, game.name);
            }
            Ok(true)
        }
        Command::Watch {
            every,
            max_attempts,
            agent,
            browser,
            run,
            ..
        } => {
            let agent = agent.agent();
            let window = browser.window();
            handle_interrupts()?;
            db::setup(data_dir)?;
            let _lock = WatchLock::acquire(data_dir)?;
            let Some(every) = every else {
                check(data_dir, max_attempts, &agent, window, run.timeout)?;
                return Ok(true);
            };
            loop {
                if let Err(error) = check(data_dir, max_attempts, &agent, window, run.timeout) {
                    esay!("error: {error:#}");
                }
                say!("Next check in {}s", every.as_secs());
                thread::sleep(every);
            }
        }
    }
}

async fn serve(data_dir: &Path) -> Result<()> {
    db::setup(data_dir)?;
    std::fs::create_dir_all(data_dir.join(router::ASSETS_DIR))?;

    let app = router::router(data_dir);
    let listener = TcpListener::bind(ADDRESS)
        .await
        .with_context(|| format!("could not listen on {ADDRESS}"))?;

    say!("Serving on http://{ADDRESS}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Test a game by its name, or by its identifier when one is given.
fn test(
    data_dir: &Path,
    name: &str,
    game_id: Option<&str>,
    url: &str,
    agent: &Agent,
    window: Window,
    timeout: Duration,
) -> Result<bool> {
    db::setup(data_dir)?;
    let resolved = resolve_game(data_dir, &Http, name, game_id)?;
    for warning in &resolved.warnings {
        esay!("warning: {warning}");
    }
    let outcome = run::test_game(
        data_dir,
        &resolved.game,
        url,
        agent,
        window,
        timeout,
        MissingSession::Warn,
    )?;
    Ok(outcome.passed())
}

/// The game a test is for, stored, and what the user should know about how it was found.
struct Resolved {
    game: catalog::Game,
    warnings: Vec<String>,
}

/// Find the game a test is for, and store it.
///
/// A game the catalogue describes is stored, or brought up to date, as it describes it. When the
/// catalogue cannot be read or does not have the game, a stored game is used as it is, so that a
/// failure to read the catalogue never wipes what was stored. Only when no stored game matches
/// either is a new one stored, under an identifier made from the name, and when another game is
/// stored under that identifier the command stops instead. With `game_id`, the game
/// is looked up by that identifier instead, and it must be in the catalogue or stored.
fn resolve_game(
    data_dir: &Path,
    fetcher: &impl Fetcher,
    name: &str,
    game_id: Option<&str>,
) -> Result<Resolved> {
    match game_id {
        Some(game_id) => resolve_game_by_id(data_dir, fetcher, name, game_id.trim()),
        None => resolve_game_by_name(data_dir, fetcher, name.trim()),
    }
}

fn resolve_game_by_name(data_dir: &Path, fetcher: &impl Fetcher, name: &str) -> Result<Resolved> {
    let mut warnings = Vec::new();
    match find_game(fetcher, name) {
        Ok(NameMatch::One(game)) => {
            db::upsert_game(data_dir, &game)?;
            return Ok(Resolved { game, warnings });
        }
        Ok(NameMatch::Several(games)) => {
            bail!(several_games("games of the catalogue", name, &games))
        }
        Ok(NameMatch::None) => warnings.push(format!("the catalogue has no game named {name:?}.")),
        Err(error) => warnings.push(format!(
            "could not read the catalogue, so {name:?} is looked for among the stored games: \
             {error:#}"
        )),
    }
    match catalog::match_name(db::select_games_named(data_dir, name)?, name) {
        NameMatch::One(game) => {
            warnings.push(format!("the stored game {} is tested as it is.", game.id));
            Ok(Resolved { game, warnings })
        }
        NameMatch::Several(games) => bail!(
            "{} {}",
            warnings.join(" "),
            several_games("stored games", name, &games)
        ),
        NameMatch::None => {
            let game = catalog::Game {
                id: slugify(name),
                name: name.to_owned(),
                provider: String::new(),
                primary_type: String::new(),
                is_available_for_fun: false,
                image_url: String::new(),
            };
            if let Some(taken) = db::select_stored_game(data_dir, &game.id)?
                && taken.name.to_lowercase() != name.to_lowercase()
            {
                bail!(
                    "{} {name:?} would be stored as {}, but that is the identifier of the stored \
                     game {:?}. Choose the game with --game-id.",
                    warnings.join(" "),
                    game.id,
                    taken.name
                );
            }
            db::insert_game(data_dir, &game)?;
            warnings.push(format!(
                "no stored game is named {name:?}, so it is stored as {}, without a provider \
                 or an image.",
                game.id
            ));
            Ok(Resolved { game, warnings })
        }
    }
}

fn resolve_game_by_id(
    data_dir: &Path,
    fetcher: &impl Fetcher,
    name: &str,
    game_id: &str,
) -> Result<Resolved> {
    let mut warnings = Vec::new();
    let missing = match find_game_by_id(fetcher, game_id) {
        Ok(Some(game)) => {
            db::upsert_game(data_dir, &game)?;
            return Ok(Resolved {
                warnings: name_differs(&game, name).into_iter().collect(),
                game,
            });
        }
        Ok(None) => {
            warnings.push(format!("the catalogue has no game {game_id}."));
            "the catalogue has no such game"
        }
        Err(error) => {
            warnings.push(format!(
                "could not read the catalogue, so {game_id} is looked for among the stored \
                 games: {error:#}"
            ));
            "the catalogue could not be read"
        }
    };
    let Some(game) = db::select_stored_game(data_dir, game_id)? else {
        bail!("no game {game_id} is stored, and {missing}");
    };
    warnings.push(format!("the stored game {game_id} is tested as it is."));
    warnings.extend(name_differs(&game, name));
    Ok(Resolved { game, warnings })
}

/// A warning for a game picked by its identifier whose name is not the one asked for.
fn name_differs(game: &catalog::Game, name: &str) -> Option<String> {
    let name = name.trim();
    (game.name.to_lowercase() != name.to_lowercase()).then(|| {
        format!(
            "{} is named {:?}, not {name:?}. It is tested by its own name.",
            game.id, game.name
        )
    })
}

/// The error for a name that several games share, with their identifiers.
fn several_games(kind: &str, name: &str, games: &[catalog::Game]) -> String {
    let ids: Vec<&str> = games.iter().map(|game| game.id.as_str()).collect();
    format!(
        "{} {kind} are named {name:?}: {}. Choose one with --game-id.",
        games.len(),
        ids.join(", ")
    )
}

/// One watch check, testing each queued game the way `test` does, except that a missing
/// `session-block.txt` stops it: every queued game would fail the same way.
fn check(
    data_dir: &Path,
    max_attempts: u32,
    agent: &Agent,
    window: Window,
    timeout: Duration,
) -> Result<()> {
    watch::check(data_dir, &Http, max_attempts, |game| {
        tried(
            data_dir,
            run::test_game(
                data_dir,
                game,
                DEFAULT_CASINO_URL,
                agent,
                window,
                timeout,
                MissingSession::Refuse,
            ),
        )
    })
}

/// What a test of a queued game came to, for the queue of the watch.
fn tried(data_dir: &Path, result: Result<Outcome>) -> Tried {
    match result {
        Ok(outcome) if outcome.finished() => Tried::Verdict,
        Ok(outcome) if outcome.session_problem() => {
            esay!(
                "error: the login session is missing or expired. Refresh {} before the next check.",
                data_dir.join(run::SESSION_FILE).display()
            );
            Tried::Environment
        }
        Ok(_) => Tried::NoVerdict,
        Err(error) => {
            esay!("error: {error:#}");
            if run::did_not_start(&error) {
                Tried::Environment
            } else {
                Tried::NoVerdict
            }
        }
    }
}

fn non_empty(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("game name is empty".into())
    } else {
        Ok(value.to_owned())
    }
}

fn non_empty_id(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("game id is empty".into())
    } else {
        Ok(value.trim().to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;
    use serde_json::json;

    /// Set by `headless_follows_boolish_env_values` for the child run of `headless_from_env`.
    const EXPECTED: &str = "TESTSON_TEST_EXPECTED_HEADLESS";

    /// Runs `headless_from_env` in a child process with `TESTSON_HEADLESS` set, since setting it
    /// here would need `unsafe` and race with the other tests.
    #[test]
    fn headless_follows_boolish_env_values() {
        let cases = [
            ("1", true),
            ("yes", true),
            ("on", true),
            ("true", true),
            ("0", false),
            ("no", false),
            ("off", false),
            ("false", false),
        ];
        for (value, expected) in cases {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "tests::headless_from_env",
                    "--exact",
                    "--ignored",
                    "--quiet",
                ])
                .env("TESTSON_HEADLESS", value)
                .env(EXPECTED, expected.to_string())
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "TESTSON_HEADLESS={value}");
        }
    }

    #[test]
    #[ignore = "run by headless_follows_boolish_env_values with TESTSON_HEADLESS set"]
    fn headless_from_env() {
        let Ok(expected) = std::env::var(EXPECTED) else {
            return;
        };
        for args in [["testson", "test", "Alpha"], ["testson", "watch", "--list"]] {
            let cli = Cli::try_parse_from(args).unwrap();
            let (Command::Test { browser, .. } | Command::Watch { browser, .. }) = cli.command
            else {
                panic!("not a command with browser options");
            };
            assert_eq!(browser.headless.to_string(), expected);
        }
    }

    #[test]
    fn headless_is_a_flag() {
        let cli = Cli::try_parse_from(["testson", "test", "Alpha", "--headless"]).unwrap();
        let Command::Test { browser, .. } = cli.command else {
            panic!("not the test command");
        };
        assert!(browser.headless);
    }

    fn timeout(args: &[&str]) -> Duration {
        match Cli::try_parse_from(args).unwrap().command {
            Command::Test { run, .. } | Command::Watch { run, .. } => run.timeout,
            _ => panic!("not a run"),
        }
    }

    #[test]
    fn runs_have_a_timeout() {
        assert_eq!(
            timeout(&["testson", "test", "Alpha"]),
            Duration::from_secs(1_200)
        );
        assert_eq!(
            timeout(&["testson", "watch", "--timeout", "45m"]),
            Duration::from_secs(2_700)
        );
        assert!(Cli::try_parse_from(["testson", "test", "Alpha", "--timeout", "30s"]).is_err());
    }

    const DATA_DIR: &str = "data-dir";

    /// What a run that stored `document` and exited with `agent_exit_code` comes to.
    fn ran(document: serde_json::Value, agent_exit_code: i32) -> Tried {
        let outcome = Outcome {
            report_path: PathBuf::new(),
            document,
            agent_exit_code,
        };
        tried(Path::new(DATA_DIR), Ok(outcome))
    }

    #[test]
    fn only_the_game_counts_against_it() {
        assert_eq!(ran(json!({ "status": "working" }), 1), Tried::Verdict);
        assert_eq!(ran(json!({ "status": "failing" }), 0), Tried::Verdict);
        let broken = json!({ "status": "test_failed", "error": { "message": "no browser" } });
        assert_eq!(ran(broken, 0), Tried::NoVerdict);
        let logged_out = json!({
            "status": "test_failed",
            "error": { "message": "logged out", "reason": "session" },
        });
        assert_eq!(ran(logged_out, 0), Tried::Environment);
        let not_started = anyhow::anyhow!("ping failed").context(run::NotStarted);
        assert_eq!(
            tried(Path::new(DATA_DIR), Err(not_started)),
            Tried::Environment
        );
        let not_stored = anyhow::anyhow!("could not store the test");
        assert_eq!(
            tried(Path::new(DATA_DIR), Err(not_stored)),
            Tried::NoVerdict
        );
    }

    const PAGE: &str = r#"{"marketCode":"es","brandId":"11111111-2222-3333-4444-555555555555","countryCode":"ES","jurisdiction":"DGOJ"}"#;

    /// A catalogue that cannot be read.
    struct Offline;

    impl Fetcher for Offline {
        fn fetch(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Vec<u8>> {
            Err(anyhow!("GET {url} failed: offline"))
        }
    }

    /// A catalogue that lists its games as `(id, name)`.
    struct Listing(&'static [(&'static str, &'static str)]);

    impl Fetcher for Listing {
        fn fetch(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Vec<u8>> {
            if !url.contains("/api/") {
                return Ok(PAGE.as_bytes().to_vec());
            }
            let items: Vec<String> = self
                .0
                .iter()
                .map(|(id, name)| {
                    format!(
                        r#"{{"id":"{id}","name":"{name}","provider":"Catalogue","content":{{"thumbnail":{{"url":"https://example.test/{id}.png"}}}}}}"#
                    )
                })
                .collect();
            Ok(format!("[{}]", items.join(",")).into_bytes())
        }
    }

    fn data_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        db::setup(dir.path()).unwrap();
        dir
    }

    /// Store a game with a provider and an image, as the catalogue once described it.
    fn store(dir: &Path, id: &str, name: &str) {
        db::upsert_game(
            dir,
            &catalog::Game {
                id: id.into(),
                name: name.into(),
                provider: "Stored".into(),
                primary_type: "Slots".into(),
                is_available_for_fun: true,
                image_url: format!("https://example.test/stored-{id}.png"),
            },
        )
        .unwrap();
    }

    fn stored(dir: &Path, id: &str) -> catalog::Game {
        db::select_stored_game(dir, id).unwrap().unwrap()
    }

    fn mentions(resolved: &Resolved, text: &str) -> bool {
        resolved
            .warnings
            .iter()
            .any(|warning| warning.contains(text))
    }

    fn error(result: Result<Resolved>) -> String {
        match result {
            Ok(resolved) => panic!("expected an error, got {}", resolved.game.id),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn an_unreadable_catalogue_reuses_the_stored_game_as_it_is() {
        let dir = data_dir();
        store(dir.path(), "nyxAviator", "Aviator");
        let before = stored(dir.path(), "nyxAviator");
        let resolved = resolve_game(dir.path(), &Offline, " aviator ", None).unwrap();
        assert_eq!(resolved.game, before);
        assert_eq!(stored(dir.path(), "nyxAviator"), before);
        assert!(!db::game_exists(dir.path(), "aviator").unwrap());
        assert!(mentions(&resolved, "could not read the catalogue"));
        assert!(!mentions(&resolved, "has no game"));
    }

    #[test]
    fn an_unreadable_catalogue_and_no_stored_game_store_a_new_one() {
        let dir = data_dir();
        let resolved = resolve_game(dir.path(), &Offline, "Los Piratas Golfetes", None).unwrap();
        assert_eq!(resolved.game.id, "los-piratas-golfetes");
        assert_eq!(
            stored(dir.path(), "los-piratas-golfetes").name,
            "Los Piratas Golfetes"
        );
        assert!(mentions(&resolved, "could not read the catalogue"));
        assert!(!mentions(&resolved, "has no game"));
    }

    #[test]
    fn a_game_the_catalogue_lacks_is_the_stored_one_if_any() {
        let dir = data_dir();
        store(dir.path(), "oldPiratas", "Los Piratas Golfetes");
        let catalogue = Listing(&[("other", "Other")]);
        let resolved = resolve_game(dir.path(), &catalogue, "los piratas golfetes", None).unwrap();
        assert_eq!(resolved.game.id, "oldPiratas");
        assert_eq!(stored(dir.path(), "oldPiratas").provider, "Stored");
        assert!(!db::game_exists(dir.path(), "los-piratas-golfetes").unwrap());
        assert!(mentions(&resolved, "the catalogue has no game named"));
        assert!(!mentions(&resolved, "could not read"));

        let resolved = resolve_game(dir.path(), &catalogue, "Nuevo", None).unwrap();
        assert_eq!(resolved.game.id, "nuevo");
        assert!(db::game_exists(dir.path(), "nuevo").unwrap());
    }

    #[test]
    fn a_new_game_whose_identifier_is_taken_stops_and_writes_nothing() {
        let dir = data_dir();
        store(dir.path(), "piratas", "Piratas!");
        let before = stored(dir.path(), "piratas");
        let message = error(resolve_game(dir.path(), &Offline, "Piratas?", None));
        assert!(message.contains("\"Piratas?\""), "{message}");
        assert!(message.contains("\"Piratas!\""), "{message}");
        assert!(message.contains("--game-id"), "{message}");
        assert!(
            message.contains("could not read the catalogue"),
            "{message}"
        );
        assert_eq!(stored(dir.path(), "piratas"), before);

        // Names in other scripts get identifiers of their own, so only an identifier that is
        // really taken stops the command.
        assert_ne!(slugify("Ψυχή"), slugify("Ωμέγα"));
        let taken = slugify("Ψυχή");
        store(dir.path(), &taken, "Ωμέγα");
        let catalogue = Listing(&[("other", "Other")]);
        let message = error(resolve_game(dir.path(), &catalogue, "Ψυχή", None));
        assert!(message.contains("\"Ωμέγα\""), "{message}");
        assert!(
            message.contains("the catalogue has no game named"),
            "{message}"
        );
        assert_eq!(stored(dir.path(), &taken).name, "Ωμέγα");
    }

    #[test]
    fn a_game_of_the_catalogue_brings_the_stored_one_up_to_date() {
        let dir = data_dir();
        store(dir.path(), "nyxAviator", "Aviator");
        let catalogue = Listing(&[("nyxAviator", "Aviator")]);
        let resolved = resolve_game(dir.path(), &catalogue, "Aviator", None).unwrap();
        assert_eq!(resolved.warnings, Vec::<String>::new());
        assert_eq!(stored(dir.path(), "nyxAviator").provider, "Catalogue");
    }

    #[test]
    fn the_exact_name_wins_and_a_shared_name_must_be_chosen_by_id() {
        let dir = data_dir();
        let catalogue = Listing(&[
            ("nyxAviator", "Aviator"),
            ("spribeAviator", "Aviator"),
            ("loud", "AVIATOR"),
        ]);
        let resolved = resolve_game(dir.path(), &catalogue, "AVIATOR", None).unwrap();
        assert_eq!(resolved.game.id, "loud");

        let message = error(resolve_game(dir.path(), &catalogue, "Aviator", None));
        assert!(message.contains("nyxAviator, spribeAviator"), "{message}");
        assert!(message.contains("--game-id"), "{message}");
        assert!(!db::game_exists(dir.path(), "aviator").unwrap());

        let resolved =
            resolve_game(dir.path(), &catalogue, "Aviator", Some("spribeAviator")).unwrap();
        assert_eq!(resolved.game.id, "spribeAviator");
        assert_eq!(resolved.warnings, Vec::<String>::new());
        assert_eq!(stored(dir.path(), "spribeAviator").provider, "Catalogue");
    }

    #[test]
    fn several_stored_games_with_one_name_must_be_chosen_by_id() {
        let dir = data_dir();
        store(dir.path(), "nyxAviator", "Aviator");
        store(dir.path(), "aviator", "Aviator");
        let message = error(resolve_game(dir.path(), &Offline, "Aviator", None));
        assert!(
            message.contains("could not read the catalogue"),
            "{message}"
        );
        assert!(message.contains("aviator, nyxAviator"), "{message}");
        assert!(message.contains("--game-id"), "{message}");
    }

    #[test]
    fn a_game_id_falls_back_to_the_stored_game_and_is_never_made_up() {
        let dir = data_dir();
        store(dir.path(), "nyxAviator", "Aviator");
        let before = stored(dir.path(), "nyxAviator");
        let resolved = resolve_game(dir.path(), &Offline, "Aviator", Some("nyxAviator")).unwrap();
        assert_eq!(resolved.game, before);
        assert!(mentions(&resolved, "could not read the catalogue"));

        let catalogue = Listing(&[("other", "Other")]);
        let resolved = resolve_game(dir.path(), &catalogue, "Aviator", Some("nyxAviator")).unwrap();
        assert_eq!(resolved.game, before);
        assert!(mentions(&resolved, "the catalogue has no game nyxAviator"));

        let message = error(resolve_game(
            dir.path(),
            &catalogue,
            "Aviator",
            Some("typo"),
        ));
        assert!(message.contains("typo"), "{message}");
        assert!(!db::game_exists(dir.path(), "typo").unwrap());

        let resolved = resolve_game(dir.path(), &catalogue, "Aviator", Some("other")).unwrap();
        assert!(mentions(&resolved, "named \"Other\""));
    }
}
