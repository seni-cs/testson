//! The database: its migrations and every query, grouped by table.
//!
//! Each function opens its own connection, so the server, `test` and `watch` can use the database
//! at the same time. WAL mode lets them read while another process writes, and the busy timeout
//! makes a writer wait for another one instead of failing.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;

use crate::catalog;

const DATABASE_FILE: &str = "testson.db";

/// The migrations, in order. The database records how many it has applied in `user_version`.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_initial.sql"),
    include_str!("../migrations/002_watch.sql"),
    include_str!("../migrations/003_pending_attempts.sql"),
];

/// How long a connection waits for another process to finish writing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(10);

// Setup

/// Open the database with its foreign keys enforced, which SQLite leaves off unless asked.
fn connect(data_dir: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(data_dir.join(DATABASE_FILE))?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}

/// Create the data directory, switch the database to WAL mode and apply every pending migration.
///
/// # Errors
///
/// Fails when the directory or the database cannot be created, a migration fails, or the
/// database was made by a newer version of testson.
pub fn setup(data_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("could not create {}", data_dir.display()))?;
    let mut connection = connect(data_dir)?;
    let journal_mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if journal_mode != "wal" {
        bail!("the database stayed in {journal_mode} journal mode instead of WAL");
    }
    migrate(&mut connection)
}

/// Apply the migrations the database has not seen yet, in one transaction.
///
/// The transaction takes the write lock before it reads `user_version`, so two processes that
/// start together never apply the same migration twice.
fn migrate(connection: &mut Connection) -> anyhow::Result<()> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version: u32 = transaction.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let latest = u32::try_from(MIGRATIONS.len())?;
    if version > latest {
        bail!(
            "the database is at version {version}, newer than this testson knows ({latest}). \
             Update testson."
        );
    }
    for migration in MIGRATIONS.iter().skip(usize::try_from(version)?) {
        transaction.execute_batch(migration)?;
    }
    transaction.pragma_update(None, "user_version", latest)?;
    transaction.commit()?;
    Ok(())
}

// Games

/// A game as the list of games shows it. Its status and the time it was tested at are those
/// of its latest test, if any.
#[derive(Serialize)]
pub struct GameSummary {
    id: String,
    name: String,
    image_url: String,
    added_on: String,
    status: Option<String>,
    tested_at: Option<String>,
}

/// A game as its own page shows it, with its tests newest first.
#[derive(Serialize)]
pub struct Game {
    name: String,
    provider: String,
    primary_type: String,
    is_available_for_fun: bool,
    image_url: String,
    added_on: String,
    tests: Vec<TestSummary>,
}

/// The columns of `games` that [`catalog_game`] reads, in its order.
const CATALOG_COLUMNS: &str = "games.id, games.name, games.provider, games.primary_type, games.is_available_for_fun, \
     games.image_url";

fn catalog_game(row: &Row<'_>) -> rusqlite::Result<catalog::Game> {
    Ok(catalog::Game {
        id: row.get(0)?,
        name: row.get(1)?,
        provider: row.get(2)?,
        primary_type: row.get(3)?,
        is_available_for_fun: row.get(4)?,
        image_url: row.get(5)?,
    })
}

/// Select every game, newest first.
pub fn select_games(data_dir: &Path) -> rusqlite::Result<Vec<GameSummary>> {
    let connection = connect(data_dir)?;
    let mut statement = connection.prepare(
        "SELECT games.id, games.name, games.image_url, games.added_on,
                latest.status, latest.started_at
         FROM games
         LEFT JOIN tests AS latest ON latest.id = (
             SELECT id FROM tests WHERE game_id = games.id
             ORDER BY started_at DESC, id DESC LIMIT 1
         )
         ORDER BY games.added_on DESC, games.name",
    )?;
    let games = statement.query_map([], |row| {
        Ok(GameSummary {
            id: row.get(0)?,
            name: row.get(1)?,
            image_url: row.get(2)?,
            added_on: row.get(3)?,
            status: row.get(4)?,
            tested_at: row.get(5)?,
        })
    })?;
    games.collect()
}

pub fn game_exists(data_dir: &Path, game_id: &str) -> rusqlite::Result<bool> {
    connect(data_dir)?.query_row(
        "SELECT EXISTS (SELECT 1 FROM games WHERE id = ?1)",
        [game_id],
        |row| row.get(0),
    )
}

/// Select a game and its tests, or nothing if no game has that id.
pub fn select_game(data_dir: &Path, game_id: &str) -> rusqlite::Result<Option<Game>> {
    let connection = connect(data_dir)?;
    let game = connection
        .query_row(
            "SELECT name, provider, primary_type, is_available_for_fun, image_url, added_on
             FROM games
             WHERE id = ?1",
            [game_id],
            |row| {
                Ok(Game {
                    name: row.get(0)?,
                    provider: row.get(1)?,
                    primary_type: row.get(2)?,
                    is_available_for_fun: row.get(3)?,
                    image_url: row.get(4)?,
                    added_on: row.get(5)?,
                    tests: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(mut game) = game else {
        return Ok(None);
    };
    game.tests = select_test_summaries(&connection, game_id)?;
    Ok(Some(game))
}

/// Select the stored game with this name, ignoring case, preferring one spelt exactly so.
pub fn find_game_by_name(data_dir: &Path, name: &str) -> rusqlite::Result<Option<catalog::Game>> {
    connect(data_dir)?
        .query_row(
            &format!(
                "SELECT {CATALOG_COLUMNS} FROM games
                 WHERE name = ?1 COLLATE NOCASE
                 ORDER BY name = ?1 DESC, added_on DESC
                 LIMIT 1"
            ),
            [name],
            catalog_game,
        )
        .optional()
}

/// Store a game as the catalogue describes it, or bring a stored one up to date with it. The day
/// a game was added on stays the day it was first stored.
pub fn upsert_game(data_dir: &Path, game: &catalog::Game) -> rusqlite::Result<()> {
    upsert(&connect(data_dir)?, game)
}

fn upsert(connection: &Connection, game: &catalog::Game) -> rusqlite::Result<()> {
    connection.execute(
        "INSERT INTO games (id, name, provider, primary_type, is_available_for_fun, image_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (id) DO UPDATE SET
             name = excluded.name,
             provider = excluded.provider,
             primary_type = excluded.primary_type,
             is_available_for_fun = excluded.is_available_for_fun,
             image_url = excluded.image_url",
        params![
            game.id,
            game.name,
            game.provider,
            game.primary_type,
            game.is_available_for_fun,
            game.image_url,
        ],
    )?;
    Ok(())
}

// Tests

/// A test as the page of its game lists it, without its JSON document.
#[derive(Serialize)]
pub struct TestSummary {
    id: i64,
    status: String,
    started_at: String,
}

/// Select the tests of a game, newest first.
fn select_test_summaries(
    connection: &Connection,
    game_id: &str,
) -> rusqlite::Result<Vec<TestSummary>> {
    let mut statement = connection.prepare(
        "SELECT id, status, started_at
         FROM tests
         WHERE game_id = ?1
         ORDER BY started_at DESC, id DESC",
    )?;
    let tests = statement.query_map([game_id], |row| {
        Ok(TestSummary {
            id: row.get(0)?,
            status: row.get(1)?,
            started_at: row.get(2)?,
        })
    })?;
    tests.collect()
}

/// Select the JSON document of a test, as stored.
pub fn select_test_document(data_dir: &Path, test_id: i64) -> rusqlite::Result<Option<String>> {
    connect(data_dir)?
        .query_row("SELECT test FROM tests WHERE id = ?1", [test_id], |row| {
            row.get(0)
        })
        .optional()
}

/// Store the JSON document of a test of a game, and return the number the test goes by.
pub fn insert_test(data_dir: &Path, game_id: &str, document: &str) -> rusqlite::Result<i64> {
    let connection = connect(data_dir)?;
    connection.execute(
        "INSERT INTO tests (game_id, test) VALUES (?1, ?2)",
        [game_id, document],
    )?;
    Ok(connection.last_insert_rowid())
}

// The watch over Novedades and its queue

/// Select the games that were in the New Games collection at the latest check, by name.
pub fn select_new_games(data_dir: &Path) -> rusqlite::Result<Vec<catalog::Game>> {
    let connection = connect(data_dir)?;
    let mut statement = connection.prepare(&format!(
        "SELECT {CATALOG_COLUMNS} FROM games WHERE in_new_games = 1 ORDER BY name, id"
    ))?;
    let games = statement.query_map([], catalog_game)?;
    games.collect()
}

/// Store `games` as the whole New Games collection, and queue the ones in `queued` for a demo
/// test, all at once so that an interrupted check leaves the previous collection in place.
pub fn record_new_games(
    data_dir: &Path,
    games: &[catalog::Game],
    queued: &[&str],
) -> rusqlite::Result<()> {
    let mut connection = connect(data_dir)?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "UPDATE games SET in_new_games = 0 WHERE in_new_games = 1",
        [],
    )?;
    for game in games {
        upsert(&transaction, game)?;
        transaction.execute(
            "UPDATE games SET in_new_games = 1 WHERE id = ?1",
            [&game.id],
        )?;
    }
    for game_id in queued {
        transaction.execute(
            "INSERT INTO pending_tests (game_id) VALUES (?1) ON CONFLICT DO NOTHING",
            [game_id],
        )?;
    }
    transaction.commit()
}

/// Select the games waiting for a demo test, in the order they were queued in.
pub fn select_pending_games(data_dir: &Path) -> rusqlite::Result<Vec<catalog::Game>> {
    let connection = connect(data_dir)?;
    let mut statement = connection.prepare(&format!(
        "SELECT {CATALOG_COLUMNS}
         FROM pending_tests
         JOIN games ON games.id = pending_tests.game_id
         ORDER BY pending_tests.queued_at, games.id"
    ))?;
    let games = statement.query_map([], catalog_game)?;
    games.collect()
}

/// Count one more attempt at the demo test of a queued game, and return how many it has had.
pub fn add_attempt(data_dir: &Path, game_id: &str) -> rusqlite::Result<u32> {
    connect(data_dir)?.query_row(
        "UPDATE pending_tests SET attempts = attempts + 1 WHERE game_id = ?1 RETURNING attempts",
        [game_id],
        |row| row.get(0),
    )
}

/// Take a game off the queue of games waiting for a demo test.
pub fn dequeue(data_dir: &Path, game_id: &str) -> rusqlite::Result<()> {
    connect(data_dir)?.execute("DELETE FROM pending_tests WHERE game_id = ?1", [game_id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn game(id: &str, name: &str) -> catalog::Game {
        catalog::Game {
            id: id.into(),
            name: name.into(),
            provider: String::new(),
            primary_type: String::new(),
            is_available_for_fun: false,
            image_url: String::new(),
        }
    }

    #[test]
    fn setup_is_idempotent_and_refuses_a_newer_database() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        setup(dir.path()).unwrap();
        connect(dir.path())
            .unwrap()
            .pragma_update(None, "user_version", 99)
            .unwrap();
        assert!(setup(dir.path()).unwrap_err().to_string().contains("newer"));
    }

    #[test]
    fn finds_a_stored_game_by_name_preferring_the_exact_spelling() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        upsert_game(dir.path(), &game("upper", "ALPHA")).unwrap();
        upsert_game(dir.path(), &game("exact", "Alpha")).unwrap();
        let found = |name| find_game_by_name(dir.path(), name).unwrap().map(|g| g.id);
        assert_eq!(found("Alpha").as_deref(), Some("exact"));
        assert_eq!(found("ALPHA").as_deref(), Some("upper"));
        assert_eq!(found("Beta"), None);
    }

    #[test]
    fn counts_the_attempts_of_a_queued_game() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        record_new_games(dir.path(), &[game("alpha", "Alpha")], &["alpha"]).unwrap();
        assert_eq!(add_attempt(dir.path(), "alpha").unwrap(), 1);
        assert_eq!(add_attempt(dir.path(), "alpha").unwrap(), 2);
    }
}
