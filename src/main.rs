mod catalog;
mod db;
mod events;
mod harness;
mod login;
mod prompt;
mod report;
mod router;
mod run;
mod watch;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use tokio::net::TcpListener;

use crate::catalog::{DEFAULT_CASINO_URL, Http, fetch_provider_games, find_game};
use crate::harness::{Agent, Harness};
use crate::login::Browser;
use crate::prompt::{parse_betsson_url, slugify};
use crate::run::Mode;
use crate::watch::{WatchLock, parse_interval};

const ADDRESS: &str = "127.0.0.1:8000";

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Directory that holds the database, the assets, the runs, and the session files.
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

#[derive(Subcommand)]
enum Command {
    /// Start the dashboard
    Serve,
    /// Test one game with real money and without balance, and store the results
    Test {
        /// Name of the game, as shown in the lobby.
        #[arg(value_parser = non_empty)]
        game: String,

        /// Lobby or game URL. Must be https on a betsson host.
        #[arg(long, default_value = DEFAULT_CASINO_URL, value_parser = parse_betsson_url)]
        url: String,

        /// Run only the test of this mode. Both run by default.
        #[arg(long, value_enum)]
        mode: Option<Mode>,

        #[command(flatten)]
        agent: AgentArgs,
    },
    /// Log in to the account of each mode in Google Chrome, and save its session for the tests
    Login {
        /// Log in to the account of this mode only. Both by default.
        #[arg(long, value_enum)]
        mode: Option<Mode>,

        /// Page the agent logs in on. Must be https on a betsson host.
        #[arg(long, default_value = DEFAULT_CASINO_URL, value_parser = parse_betsson_url)]
        url: String,

        /// Attach to a browser already running with remote debugging, such as
        /// `http://127.0.0.1:9222`, instead of starting Chrome. It must not be logged in to the
        /// account of the other mode.
        #[arg(long, value_name = "URL", requires = "mode", conflicts_with = "chrome")]
        cdp: Option<String>,

        /// Path of the Chrome program to start. Found in the usual places by default.
        #[arg(long, env = "TESTSON_CHROME", value_name = "PATH")]
        chrome: Option<PathBuf>,

        #[command(flatten)]
        agent: AgentArgs,
    },
    /// Store every Pragmatic Play game of the catalogue on the dashboard, without testing it
    Sync,
    /// Check that the harness answers with the chosen model, without running a test
    Ping {
        #[command(flatten)]
        agent: AgentArgs,
    },
    /// Test every Pragmatic Play game in both modes, until each has a verdict
    Watch {
        /// Print the games of the provider and do not save or test.
        #[arg(long, conflicts_with = "every")]
        list: bool,

        /// Repeat the check at this interval, for example 30m. Minimum 60s.
        #[arg(long, value_name = "INTERVAL", value_parser = parse_interval)]
        every: Option<Duration>,

        #[command(flatten)]
        agent: AgentArgs,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("error: {error:#}");
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
            url,
            mode,
            agent,
        } => {
            let modes = mode.map_or(Mode::ALL.to_vec(), |mode| vec![mode]);
            test(data_dir, &game, &url, &modes, &agent.agent())
        }
        Command::Login {
            mode,
            url,
            cdp,
            chrome,
            agent,
        } => {
            let agent = agent.agent();
            let browser = cdp.map_or(Browser::Launch(chrome), Browser::Attach);
            let mut passed = true;
            for mode in mode.map_or(Mode::ALL.to_vec(), |mode| vec![mode]) {
                passed &= login::login(data_dir, mode, &url, &browser, &agent)?;
            }
            Ok(passed)
        }
        Command::Sync => {
            db::setup(data_dir)?;
            let games = fetch_provider_games(&Http)?;
            db::upsert_games(data_dir, &games)?;
            println!("Stored {} games of {}.", games.len(), catalog::PROVIDER);
            Ok(true)
        }
        Command::Ping { agent } => {
            let agent = agent.agent();
            agent.ensure_installed()?;
            agent.ping()?;
            println!("{agent} answered.");
            Ok(true)
        }
        Command::Watch { list: true, .. } => {
            for game in fetch_provider_games(&Http)? {
                println!("{}\t{}", game.id, game.name);
            }
            Ok(true)
        }
        Command::Watch { every, agent, .. } => {
            let agent = agent.agent();
            db::setup(data_dir)?;
            let _lock = WatchLock::acquire(data_dir)?;
            let Some(every) = every else {
                check(data_dir, &agent)?;
                return Ok(true);
            };
            loop {
                if let Err(error) = check(data_dir, &agent) {
                    eprintln!("error: {error:#}");
                }
                println!("Next check in {}s", every.as_secs());
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

    println!("Serving on http://{ADDRESS}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Test a game by its name, as the catalogue describes it, in each of `modes`. When the
/// catalogue cannot be read or has no game by that name, the stored game with that name is tested
/// as it is, and only when there is none either is a new one stored, under an identifier made from
/// the name.
fn test(data_dir: &Path, name: &str, url: &str, modes: &[Mode], agent: &Agent) -> Result<bool> {
    db::setup(data_dir)?;
    let game = match find_game(&Http, name) {
        Ok(Some(game)) => {
            db::upsert_game(data_dir, &game)?;
            game
        }
        found => {
            match found {
                Err(error) => eprintln!("warning: could not read the catalogue: {error:#}"),
                Ok(_) => eprintln!("warning: the catalogue has no game named {name:?}."),
            }
            if let Some(game) = db::find_game_by_name(data_dir, name.trim())? {
                eprintln!("warning: testing the stored game {} as it is.", game.id);
                game
            } else {
                let game = catalog::Game {
                    id: slugify(name),
                    name: name.trim().to_owned(),
                    provider: String::new(),
                    primary_type: String::new(),
                    is_available_for_fun: false,
                    image_url: String::new(),
                };
                eprintln!("warning: storing it as a new game, {}.", game.id);
                db::upsert_game(data_dir, &game)?;
                game
            }
        }
    };
    let mut passed = true;
    for &mode in modes {
        passed &= run::test_game(data_dir, &game, url, mode, agent)?.passed();
    }
    Ok(passed)
}

fn check(data_dir: &Path, agent: &Agent) -> Result<()> {
    watch::check(data_dir, &Http, |game, mode| {
        Ok(run::test_game(data_dir, game, DEFAULT_CASINO_URL, mode, agent)?.finished())
    })
}

fn non_empty(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("game name is empty".into())
    } else {
        Ok(value.to_owned())
    }
}
