use std::path::Path;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use crate::catalog;

const DATABASE_FILE: &str = "testson.db";
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_initial.sql"),
    include_str!("../migrations/002_watch.sql"),
];

/// Open the database with its foreign keys enforced, which SQLite leaves off unless asked.
fn connect(data_dir: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(data_dir.join(DATABASE_FILE))?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}

/// Create the data directory, switch the database to WAL mode and apply every pending migration.
///
/// # Errors
///
/// Fails when the directory or the database cannot be created, or a migration fails.
pub fn setup(data_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("could not create {}", data_dir.display()))?;
    let mut connection = connect(data_dir)?;
    let journal_mode: String =
        connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
    if journal_mode != "wal" {
        bail!("Database stayed in {journal_mode} journal mode instead of WAL");
    }

    migrate(&mut connection)?;
    Ok(())
}

/// Apply the migrations the database has not seen yet, tracked in `user_version`.
fn migrate(connection: &mut Connection) -> rusqlite::Result<()> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    let pending = (1u32..)
        .zip(MIGRATIONS)
        .filter(|(number, _)| *number > version);

    for (number, migration) in pending {
        let transaction = connection.transaction()?;
        transaction.execute_batch(migration)?;
        transaction.pragma_update(None, "user_version", number)?;
        transaction.commit()?;
    }
    Ok(())
}

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

/// A test as the page of its game lists it, without its JSON document.
#[derive(Serialize)]
pub struct TestSummary {
    id: i64,
    status: String,
    started_at: String,
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
    let connection = connect(data_dir)?;
    connection.query_row(
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
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((name, provider, primary_type, is_available_for_fun, image_url, added_on)) = game
    else {
        return Ok(None);
    };

    Ok(Some(Game {
        name,
        provider,
        primary_type,
        is_available_for_fun,
        image_url,
        added_on,
        tests: select_test_summaries(&connection, game_id)?,
    }))
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
    let connection = connect(data_dir)?;
    let document = connection.query_row("SELECT test FROM tests WHERE id = ?1", [test_id], |row| {
        row.get(0)
    });
    document.optional()
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

fn catalog_game(row: &rusqlite::Row<'_>) -> rusqlite::Result<catalog::Game> {
    Ok(catalog::Game {
        id: row.get(0)?,
        name: row.get(1)?,
        provider: row.get(2)?,
        primary_type: row.get(3)?,
        is_available_for_fun: row.get(4)?,
        image_url: row.get(5)?,
    })
}

/// Select the games that were in the New Games collection at the latest check, by name.
pub fn select_new_games(data_dir: &Path) -> rusqlite::Result<Vec<catalog::Game>> {
    let connection = connect(data_dir)?;
    let mut statement = connection.prepare(
        "SELECT id, name, provider, primary_type, is_available_for_fun, image_url
         FROM games
         WHERE in_new_games = 1
         ORDER BY name, id",
    )?;
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
    let mut statement = connection.prepare(
        "SELECT games.id, games.name, games.provider, games.primary_type,
                games.is_available_for_fun, games.image_url
         FROM pending_tests
         JOIN games ON games.id = pending_tests.game_id
         ORDER BY pending_tests.queued_at, games.id",
    )?;
    let games = statement.query_map([], catalog_game)?;
    games.collect()
}

/// Take a game off the queue of games waiting for a demo test.
pub fn dequeue(data_dir: &Path, game_id: &str) -> rusqlite::Result<()> {
    connect(data_dir)?.execute("DELETE FROM pending_tests WHERE game_id = ?1", [game_id])?;
    Ok(())
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
