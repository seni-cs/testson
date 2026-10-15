use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use rusqlite::{Connection, ErrorCode, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

use crate::catalog;

const DATABASE_FILE: &str = "testson.db";
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_initial.sql"),
    include_str!("../migrations/002_watch.sql"),
    include_str!("../migrations/003_pending_attempts.sql"),
];
/// How long a connection waits for another one to finish writing before it gives up. Only one
/// connection writes at a time, and the server, a test and the watch can all have one open.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);
/// How long to wait before trying again to switch a database that was busy to WAL mode.
const WAL_RETRY: Duration = Duration::from_millis(10);

/// Open the database with its foreign keys enforced, which `SQLite` leaves off unless asked, and
/// waiting for a write of another connection instead of failing at once.
fn connect(data_dir: &Path) -> rusqlite::Result<Connection> {
    let connection = Connection::open(data_dir.join(DATABASE_FILE))?;
    connection.busy_timeout(BUSY_TIMEOUT)?;
    connection.pragma_update(None, "foreign_keys", true)?;
    Ok(connection)
}

/// Create the data directory, switch the database to WAL mode and apply every pending migration.
/// Several processes can set up the same data directory at once: the first one migrates it and
/// the others wait for it.
///
/// # Errors
///
/// Fails when the directory or the database cannot be created, a migration fails, or the
/// database was written by a newer build that knows migrations this one does not.
pub fn setup(data_dir: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(data_dir)
        .with_context(|| format!("could not create {}", data_dir.display()))?;
    let mut connection = connect(data_dir)?;
    use_wal(&connection)?;
    migrate(&mut connection)
}

/// Switch the database to WAL mode, so that the server can read while a test or the watch
/// writes. The mode is kept in the database file, so only the first setup of a data directory
/// changes it. The change needs the database to itself, and SQLite reports the database busy at
/// once, without waiting, when another process is changing it too, so it is tried again until
/// the busy timeout runs out.
fn use_wal(connection: &Connection) -> anyhow::Result<()> {
    let deadline = Instant::now() + BUSY_TIMEOUT;
    let journal_mode: String = loop {
        match connection.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0)) {
            Err(error)
                if error.sqlite_error_code() == Some(ErrorCode::DatabaseBusy)
                    && Instant::now() < deadline =>
            {
                thread::sleep(WAL_RETRY);
            }
            result => break result.context("could not switch the database to WAL mode")?,
        }
    };
    if journal_mode != "wal" {
        bail!("Database stayed in {journal_mode} journal mode instead of WAL");
    }
    Ok(())
}

/// Apply the migrations the database has not seen yet, tracked in `user_version`.
///
/// A database that is up to date is left alone without taking the write lock, so the server
/// starts even while a test or the watch is writing. Otherwise the version is read again inside
/// an immediate transaction, which holds the write lock from its start, so a process that sets
/// up the database at the same time waits for this one and then finds the migrations applied
/// instead of applying them a second time.
fn migrate(connection: &mut Connection) -> anyhow::Result<()> {
    let known = u32::try_from(MIGRATIONS.len())?;
    if schema_version(connection, known)? == known {
        return Ok(());
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let version = schema_version(&transaction, known)?;
    if version == known {
        return Ok(());
    }
    for (number, migration) in (1..=known).zip(MIGRATIONS).filter(|(n, _)| *n > version) {
        transaction
            .execute_batch(migration)
            .with_context(|| format!("migration {number} failed"))?;
    }
    transaction.pragma_update(None, "user_version", known)?;
    transaction.commit()?;
    Ok(())
}

/// Read the version of the schema of the database, which is the number of migrations applied to
/// it, and refuse one written by a newer build that knows more than the `known` ones.
fn schema_version(connection: &Connection, known: u32) -> anyhow::Result<u32> {
    let version: u32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version > known {
        bail!(
            "The data directory was written by a newer testson (schema {version}, this build \
             knows {known})"
        );
    }
    Ok(version)
}

/// A game as the list of games shows it. Its status and the time it was tested at are those
/// of its latest test, if any. The latest test is the one stored last: `started_at` is the time
/// the agent reported, which may be wrong, so it is shown but never used to order the tests.
#[derive(Serialize)]
pub struct GameSummary {
    id: String,
    name: String,
    image_url: String,
    added_on: String,
    status: Option<String>,
    tested_at: Option<String>,
}

/// A game as its own page shows it, with its tests newest first, in the order they were stored.
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
             ORDER BY id DESC LIMIT 1
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

/// Select the tests of a game, the last stored first.
fn select_test_summaries(
    connection: &Connection,
    game_id: &str,
) -> rusqlite::Result<Vec<TestSummary>> {
    let mut statement = connection.prepare(
        "SELECT id, status, started_at
         FROM tests
         WHERE game_id = ?1
         ORDER BY id DESC",
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

/// Store a game unless one with its id is stored already, which is then left as it is. Used for
/// a game the catalogue did not describe, so that its empty fields never replace stored ones.
pub fn insert_game(data_dir: &Path, game: &catalog::Game) -> rusqlite::Result<()> {
    connect(data_dir)?.execute(
        "INSERT INTO games (id, name, provider, primary_type, is_available_for_fun, image_url)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT (id) DO NOTHING",
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

/// Select a stored game by its id, as the catalogue described it when it was stored.
pub fn select_stored_game(
    data_dir: &Path,
    game_id: &str,
) -> rusqlite::Result<Option<catalog::Game>> {
    connect(data_dir)?
        .query_row(
            "SELECT id, name, provider, primary_type, is_available_for_fun, image_url
             FROM games
             WHERE id = ?1",
            [game_id],
            catalog_game,
        )
        .optional()
}

/// Select the stored games named `name`, ignoring case and the spaces around it. SQLite folds
/// the case of ASCII letters only, so the names are compared here, as the catalogue compares
/// them.
pub fn select_games_named(data_dir: &Path, name: &str) -> rusqlite::Result<Vec<catalog::Game>> {
    let name = name.trim().to_lowercase();
    let connection = connect(data_dir)?;
    let mut statement = connection.prepare(
        "SELECT id, name, provider, primary_type, is_available_for_fun, image_url
         FROM games
         ORDER BY id",
    )?;
    let games = statement.query_map([], catalog_game)?;
    let mut named = Vec::new();
    for game in games {
        let game = game?;
        if game.name.trim().to_lowercase() == name {
            named.push(game);
        }
    }
    Ok(named)
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

/// A game waiting for a demo test, and how many checks have tried it without a verdict.
pub struct PendingGame {
    pub game: catalog::Game,
    pub attempts: u32,
}

/// Select the games waiting for a demo test, in the order they were queued in.
pub fn select_pending_games(data_dir: &Path) -> rusqlite::Result<Vec<PendingGame>> {
    let connection = connect(data_dir)?;
    let mut statement = connection.prepare(
        "SELECT games.id, games.name, games.provider, games.primary_type,
                games.is_available_for_fun, games.image_url, pending_tests.attempts
         FROM pending_tests
         JOIN games ON games.id = pending_tests.game_id
         ORDER BY pending_tests.queued_at, games.id",
    )?;
    let games = statement.query_map([], |row| {
        Ok(PendingGame {
            game: catalog_game(row)?,
            attempts: row.get(6)?,
        })
    })?;
    games.collect()
}

/// Count one more attempt at the demo test of a queued game, and return how many there have
/// been so far.
pub fn count_attempt(data_dir: &Path, game_id: &str) -> rusqlite::Result<u32> {
    connect(data_dir)?.query_row(
        "UPDATE pending_tests SET attempts = attempts + 1 WHERE game_id = ?1 RETURNING attempts",
        [game_id],
        |row| row.get(0),
    )
}

/// Take back the attempt last counted for a queued game, when the try said nothing about it.
pub fn return_attempt(data_dir: &Path, game_id: &str) -> rusqlite::Result<()> {
    connect(data_dir)?.execute(
        "UPDATE pending_tests SET attempts = attempts - 1 WHERE game_id = ?1 AND attempts > 0",
        [game_id],
    )?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};

    use super::*;

    fn known_version() -> u32 {
        u32::try_from(MIGRATIONS.len()).unwrap()
    }

    fn user_version(data_dir: &Path) -> u32 {
        connect(data_dir)
            .unwrap()
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn concurrent_setups_of_a_fresh_data_dir_all_succeed() {
        const SETUPS: usize = 8;
        let dir = tempfile::tempdir().unwrap();
        let data_dir = dir.path().join("data-dir");
        let barrier = Arc::new(Barrier::new(SETUPS));

        let setups: Vec<_> = (0..SETUPS)
            .map(|_| {
                let data_dir = data_dir.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    setup(&data_dir)
                })
            })
            .collect();
        for handle in setups {
            handle.join().unwrap().unwrap();
        }

        assert_eq!(user_version(&data_dir), known_version());
    }

    #[test]
    fn a_database_of_the_watch_before_retries_keeps_its_queue() {
        let dir = tempfile::tempdir().unwrap();
        let connection = connect(dir.path()).unwrap();
        for migration in &MIGRATIONS[..2] {
            connection.execute_batch(migration).unwrap();
        }
        connection.pragma_update(None, "user_version", 2).unwrap();
        connection
            .execute_batch(
                "INSERT INTO games (id, name, provider, is_available_for_fun, image_url)
                     VALUES ('alpha', 'Alpha', '', 1, '');
                 INSERT INTO pending_tests (game_id) VALUES ('alpha');",
            )
            .unwrap();
        drop(connection);

        setup(dir.path()).unwrap();

        assert_eq!(user_version(dir.path()), known_version());
        let pending = select_pending_games(dir.path()).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].game.id, "alpha");
        assert_eq!(pending[0].attempts, 0);
        assert_eq!(count_attempt(dir.path(), "alpha").unwrap(), 1);
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        let newer = known_version() + 1;
        connect(dir.path())
            .unwrap()
            .pragma_update(None, "user_version", newer)
            .unwrap();

        let error = setup(dir.path()).unwrap_err().to_string();

        assert!(error.contains("newer testson"), "{error}");
        assert!(error.contains(&format!("schema {newer}")), "{error}");
        assert_eq!(user_version(dir.path()), newer);
    }

    #[test]
    fn an_up_to_date_database_is_set_up_while_another_connection_writes() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        let mut writer = connect(dir.path()).unwrap();
        let _transaction = writer
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let started = Instant::now();

        setup(dir.path()).unwrap();

        assert!(started.elapsed() < BUSY_TIMEOUT, "{:?}", started.elapsed());
        assert_eq!(user_version(dir.path()), known_version());
    }

    #[test]
    fn a_write_waits_for_a_concurrent_write() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        let locked = Arc::new(Barrier::new(2));

        let holder = {
            let data_dir = dir.path().to_path_buf();
            let locked = Arc::clone(&locked);
            thread::spawn(move || {
                let mut connection = connect(&data_dir).unwrap();
                let transaction = connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .unwrap();
                locked.wait();
                thread::sleep(Duration::from_millis(200));
                transaction.commit().unwrap();
            })
        };
        locked.wait();
        let game = catalog::Game {
            id: "game".to_owned(),
            name: "Game".to_owned(),
            provider: "Provider".to_owned(),
            primary_type: "Slots".to_owned(),
            is_available_for_fun: true,
            image_url: "https://example.com/game.png".to_owned(),
        };

        upsert_game(dir.path(), &game).unwrap();

        holder.join().unwrap();
        assert!(game_exists(dir.path(), "game").unwrap());
    }

    fn data_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        dir
    }

    fn game(id: &str, name: &str, image_url: &str) -> catalog::Game {
        catalog::Game {
            id: id.into(),
            name: name.into(),
            provider: "Provider".into(),
            primary_type: "Slots".into(),
            is_available_for_fun: true,
            image_url: image_url.into(),
        }
    }

    fn insert(dir: &Path, game_id: &str, status: &str, started_at: &str) -> i64 {
        let document = format!(r#"{{"status":"{status}","started_at":"{started_at}"}}"#);
        insert_test(dir, game_id, &document).unwrap()
    }

    #[test]
    fn the_latest_test_is_the_last_stored_whatever_time_the_agent_reported() {
        let dir = data_dir();
        upsert_game(dir.path(), &game("a", "Alpha", "")).unwrap();
        let first = insert(dir.path(), "a", "working", "2026-10-14T12:00:00Z");
        let second = insert(dir.path(), "a", "failing", "2026-10-14T09:00:00Z");

        let games = select_games(dir.path()).unwrap();
        assert_eq!(games[0].status.as_deref(), Some("failing"));
        assert_eq!(
            games[0].tested_at.as_deref(),
            Some("2026-10-14T09:00:00.000Z")
        );

        let tests = select_game(dir.path(), "a").unwrap().unwrap().tests;
        let ids: Vec<i64> = tests.iter().map(|test| test.id).collect();
        assert_eq!(ids, [second, first]);
        assert_eq!(tests[0].started_at, "2026-10-14T09:00:00.000Z");
    }

    #[test]
    fn inserting_a_game_leaves_a_stored_one_alone() {
        let dir = data_dir();
        upsert_game(
            dir.path(),
            &game("a", "Alpha", "https://example.test/a.png"),
        )
        .unwrap();
        let mut bare = game("a", "Alpha", "");
        bare.provider.clear();
        insert_game(dir.path(), &bare).unwrap();
        let stored = select_stored_game(dir.path(), "a").unwrap().unwrap();
        assert_eq!(stored.image_url, "https://example.test/a.png");
        assert_eq!(stored.provider, "Provider");

        insert_game(dir.path(), &game("b", "Beta", "")).unwrap();
        assert!(game_exists(dir.path(), "b").unwrap());
        assert!(select_stored_game(dir.path(), "c").unwrap().is_none());
    }

    #[test]
    fn selects_stored_games_by_name_ignoring_case() {
        let dir = data_dir();
        upsert_game(dir.path(), &game("nyxAviator", "Aviator", "")).unwrap();
        upsert_game(dir.path(), &game("other", "Other", "")).unwrap();
        upsert_game(dir.path(), &game("enie", "ÑANDÚ", "")).unwrap();
        let ids = |name: &str| -> Vec<String> {
            select_games_named(dir.path(), name)
                .unwrap()
                .into_iter()
                .map(|game| game.id)
                .collect()
        };
        assert_eq!(ids(" aviator "), ["nyxAviator"]);
        assert_eq!(ids("ñandú"), ["enie"]);
        assert_eq!(ids("Missing"), Vec::<String>::new());
    }
}
