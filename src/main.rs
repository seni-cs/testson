mod catalog;
mod db;
mod events;
mod harness;
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

use crate::catalog::{DEFAULT_CASINO_URL, Http, fetch_novedades, find_game};
use crate::harness::{Agent, Harness};
use crate::prompt::{parse_betsson_url, slugify};
use crate::watch::{WatchLock, parse_interval};

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

#[derive(Subcommand)]
enum Command {
    /// Start the dashboard
    Serve,
    /// Test one game in demo mode and store the result
    Test {
        /// Name of the game, as shown in the lobby.
        #[arg(value_parser = non_empty)]
        game: String,

        /// Lobby or game URL. Must be https on a betsson host.
        #[arg(long, default_value = DEFAULT_CASINO_URL, value_parser = parse_betsson_url)]
        url: String,

        #[command(flatten)]
        agent: AgentArgs,
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
        Command::Test { game, url, agent } => test(data_dir, &game, &url, &agent.agent()),
        Command::Ping { agent } => {
            let agent = agent.agent();
            agent.ensure_installed()?;
            agent.ping()?;
            println!("{agent} answered.");
            Ok(true)
        }
        Command::Watch { list: true, .. } => {
            for game in fetch_novedades(&Http)? {
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

/// Test a game by its name, as the catalogue describes it. When the catalogue cannot be read or
/// has no game by that name, the stored game with that name is tested as it is, and only when
/// there is none either is a new one stored, under an identifier made from the name.
fn test(data_dir: &Path, name: &str, url: &str, agent: &Agent) -> Result<bool> {
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
    Ok(run::test_game(data_dir, &game, url, agent)?.passed())
}

fn check(data_dir: &Path, agent: &Agent) -> Result<()> {
    watch::check(data_dir, &Http, |game| {
        Ok(run::test_game(data_dir, game, DEFAULT_CASINO_URL, agent)?.finished())
    })
}

fn non_empty(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("game name is empty".into())
    } else {
        Ok(value.to_owned())
    }
}
