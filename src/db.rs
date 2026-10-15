//! The database: its migrations and every query, grouped by table.
//!
//! Each function opens its own connection, so the server, `test` and `watch` can use the database
//! at the same time. WAL mode lets them read while another process writes, and the busy timeout
//! makes a writer wait for another one instead of failing.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use serde::Serialize;

use crate::catalog;
use crate::report::GameVersion;

const DATABASE_FILE: &str = "testson.db";

/// The migrations, in order. The database records how many it has applied in `user_version`.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_initial.sql"),
    include_str!("../migrations/002_watch.sql"),
    include_str!("../migrations/003_pending_attempts.sql"),
    include_str!("../migrations/004_modes.sql"),
    include_str!("../migrations/005_scripts.sql"),
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

/// A game as the list of games shows it, with the latest test of each mode, if any.
#[derive(Serialize)]
pub struct GameSummary {
    id: String,
    name: String,
    image_url: String,
    added_on: String,
    real_money: Option<Latest>,
    no_balance: Option<Latest>,
}

/// The status of the latest test of a game in one mode, when it started, and who ran it.
#[derive(Serialize)]
pub struct Latest {
    status: String,
    started_at: String,
    /// `agent` or `script`.
    runner: String,
}

/// A game as its own page shows it, with its tests and its scripts newest first.
#[derive(Serialize)]
pub struct Game {
    name: String,
    provider: String,
    primary_type: String,
    is_available_for_fun: bool,
    image_url: String,
    added_on: String,
    tests: Vec<TestSummary>,
    scripts: Vec<ScriptSummary>,
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

/// Select every game, by name.
pub fn select_games(data_dir: &Path) -> rusqlite::Result<Vec<GameSummary>> {
    let connection = connect(data_dir)?;
    let mut statement = connection.prepare(
        "SELECT games.id, games.name, games.image_url, games.added_on,
                real_money.status, real_money.started_at, real_money.runner,
                no_balance.status, no_balance.started_at, no_balance.runner
         FROM games
         LEFT JOIN tests AS real_money ON real_money.id = (
             SELECT id FROM tests WHERE game_id = games.id AND mode = 'real_money'
             ORDER BY started_at DESC, id DESC LIMIT 1
         )
         LEFT JOIN tests AS no_balance ON no_balance.id = (
             SELECT id FROM tests WHERE game_id = games.id AND mode = 'no_balance'
             ORDER BY started_at DESC, id DESC LIMIT 1
         )
         ORDER BY games.name COLLATE NOCASE, games.id",
    )?;
    let games = statement.query_map([], |row| {
        Ok(GameSummary {
            id: row.get(0)?,
            name: row.get(1)?,
            image_url: row.get(2)?,
            added_on: row.get(3)?,
            real_money: latest(row, 4)?,
            no_balance: latest(row, 7)?,
        })
    })?;
    games.collect()
}

/// The latest test whose status, start and runner are at `index` and the two columns after it,
/// if any.
fn latest(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<Latest>> {
    let status: Option<String> = row.get(index)?;
    let started_at: Option<String> = row.get(index + 1)?;
    let runner: Option<String> = row.get(index + 2)?;
    Ok(status
        .zip(started_at)
        .zip(runner)
        .map(|((status, started_at), runner)| Latest {
            status,
            started_at,
            runner,
        }))
}

/// The ids of every stored game.
pub fn select_game_ids(data_dir: &Path) -> rusqlite::Result<HashSet<String>> {
    let connection = connect(data_dir)?;
    let mut statement = connection.prepare("SELECT id FROM games")?;
    statement.query_map([], |row| row.get(0))?.collect()
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
                    scripts: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(mut game) = game else {
        return Ok(None);
    };
    game.tests = select_test_summaries(&connection, game_id)?;
    game.scripts = select_script_summaries(&connection, game_id)?;
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

/// Store `games` as the catalogue describes them, all at once.
pub fn upsert_games(data_dir: &Path, games: &[catalog::Game]) -> rusqlite::Result<()> {
    let mut connection = connect(data_dir)?;
    let transaction = connection.transaction()?;
    for game in games {
        upsert(&transaction, game)?;
    }
    transaction.commit()
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
    /// `real_money`, `no_balance`, or `demo` for a test stored before the modes.
    mode: String,
    status: String,
    started_at: String,
    /// `agent` or `script`.
    runner: String,
    /// The ETag of the build.js of the game the test was run on, if known.
    etag: Option<String>,
    /// The script a replay replayed.
    script_id: Option<i64>,
    /// How far a replay got.
    script_outcome: Option<String>,
}

/// Select the tests of a game, newest first.
fn select_test_summaries(
    connection: &Connection,
    game_id: &str,
) -> rusqlite::Result<Vec<TestSummary>> {
    let mut statement = connection.prepare(
        "SELECT id, mode, status, started_at, runner, etag, script_id, script_outcome
         FROM tests
         WHERE game_id = ?1
         ORDER BY started_at DESC, id DESC",
    )?;
    let tests = statement.query_map([game_id], |row| {
        Ok(TestSummary {
            id: row.get(0)?,
            mode: row.get(1)?,
            status: row.get(2)?,
            started_at: row.get(3)?,
            runner: row.get(4)?,
            etag: row.get(5)?,
            script_id: row.get(6)?,
            script_outcome: row.get(7)?,
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

/// The tests that stand for `etag`, the version the game has now: those that saw it. With no
/// `etag`, as before any test saw a version of the game, every test stands for it.
const OF_VERSION: &str = "(?3 IS NULL OR etag = ?3)";

/// Whether a test of a game in `mode` reached a verdict on version `etag` of it, `working` or
/// `failing`. A test that ended in `test_failed` says nothing about the game.
pub fn has_verdict(
    data_dir: &Path,
    game_id: &str,
    mode: &str,
    etag: Option<&str>,
) -> rusqlite::Result<bool> {
    connect(data_dir)?.query_row(
        &format!(
            "SELECT EXISTS (
                 SELECT 1 FROM tests
                 WHERE game_id = ?1 AND mode = ?2 AND status IN ('working', 'failing')
                     AND {OF_VERSION}
             )"
        ),
        params![game_id, mode, etag],
        |row| row.get(0),
    )
}

/// How many tests of a game in `mode` ended in `test_failed` on version `etag` of it, the
/// attempts that reached no verdict on it. A replay that stopped because its script no longer
/// fits the game is not one: the agent test that follows it is.
pub fn failed_attempts(
    data_dir: &Path,
    game_id: &str,
    mode: &str,
    etag: Option<&str>,
) -> rusqlite::Result<u32> {
    connect(data_dir)?.query_row(
        &format!(
            "SELECT count(*) FROM tests
             WHERE game_id = ?1 AND mode = ?2 AND status = 'test_failed'
                 AND coalesce(script_outcome, '') NOT IN ('step_failed', 'version_changed')
                 AND {OF_VERSION}"
        ),
        params![game_id, mode, etag],
        |row| row.get(0),
    )
}

/// The version of the game the newest test that saw one saw, in either mode.
pub fn latest_version(data_dir: &Path, game_id: &str) -> rusqlite::Result<Option<GameVersion>> {
    connect(data_dir)?
        .query_row(
            "SELECT json_extract(test, '$.game_version.build_url'), etag FROM tests
             WHERE game_id = ?1 AND etag IS NOT NULL
             ORDER BY started_at DESC, id DESC
             LIMIT 1",
            [game_id],
            |row| {
                Ok(GameVersion {
                    build_url: row.get(0)?,
                    etag: row.get(1)?,
                })
            },
        )
        .optional()
}

// Scripts

/// A script as the page of its game lists it, without the script itself.
#[derive(Serialize)]
pub struct ScriptSummary {
    id: i64,
    mode: String,
    etag: String,
    created_by_test: i64,
    created_at: String,
    retired_at: Option<String>,
    /// `version_changed`, `step_failed` or `replaced`.
    retired_reason: Option<String>,
}

/// A script that is replayed.
#[derive(Debug)]
pub struct StoredScript {
    pub id: i64,
    /// The script, as `schema/test-script.schema.json` describes it.
    pub script: String,
}

fn select_script_summaries(
    connection: &Connection,
    game_id: &str,
) -> rusqlite::Result<Vec<ScriptSummary>> {
    let mut statement = connection.prepare(
        "SELECT id, mode, etag, created_by_test, created_at, retired_at, retired_reason
         FROM scripts
         WHERE game_id = ?1
         ORDER BY created_at DESC, id DESC",
    )?;
    let scripts = statement.query_map([game_id], |row| {
        Ok(ScriptSummary {
            id: row.get(0)?,
            mode: row.get(1)?,
            etag: row.get(2)?,
            created_by_test: row.get(3)?,
            created_at: row.get(4)?,
            retired_at: row.get(5)?,
            retired_reason: row.get(6)?,
        })
    })?;
    scripts.collect()
}

/// Store a script the test `created_by_test` of a game left, and return its number. It retires
/// the scripts of the game in the same mode that were still replayed, so only one is.
pub fn insert_script(
    data_dir: &Path,
    game_id: &str,
    script: &str,
    created_by_test: i64,
) -> rusqlite::Result<i64> {
    let mut connection = connect(data_dir)?;
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO scripts (game_id, script, created_by_test) VALUES (?1, ?2, ?3)",
        params![game_id, script, created_by_test],
    )?;
    let id = transaction.last_insert_rowid();
    transaction.execute(
        "UPDATE scripts
         SET retired_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), retired_reason = 'replaced'
         WHERE game_id = ?1 AND mode = json_extract(?2, '$.mode') AND id != ?3
             AND retired_at IS NULL",
        params![game_id, script, id],
    )?;
    transaction.commit()?;
    Ok(id)
}

/// Select the script of a game in `mode` that is still replayed, if any.
pub fn active_script(
    data_dir: &Path,
    game_id: &str,
    mode: &str,
) -> rusqlite::Result<Option<StoredScript>> {
    connect(data_dir)?
        .query_row(
            "SELECT id, script FROM scripts
             WHERE game_id = ?1 AND mode = ?2 AND retired_at IS NULL
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
            [game_id, mode],
            |row| {
                Ok(StoredScript {
                    id: row.get(0)?,
                    script: row.get(1)?,
                })
            },
        )
        .optional()
}

/// Stop replaying a script, because the game changed version or a step of it no longer ran.
pub fn retire_script(data_dir: &Path, script_id: i64, reason: &str) -> rusqlite::Result<()> {
    connect(data_dir)?.execute(
        "UPDATE scripts
         SET retired_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), retired_reason = ?2
         WHERE id = ?1 AND retired_at IS NULL",
        params![script_id, reason],
    )?;
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

    /// Store a test of `alpha` that saw `etag`, if any.
    fn store(data_dir: &Path, mode: &str, status: &str, at: &str, etag: Option<&str>) -> i64 {
        let mut document = serde_json::json!({ "mode": mode, "status": status, "started_at": at });
        if let Some(etag) = etag {
            document["game_version"] =
                serde_json::json!({ "build_url": "https://x/build.js", "etag": etag });
        }
        insert_test(data_dir, "alpha", &document.to_string()).unwrap()
    }

    #[test]
    fn a_verdict_counts_for_the_version_it_saw() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        upsert_games(dir.path(), &[game("alpha", "Alpha")]).unwrap();
        let path = dir.path();
        store(
            path,
            "real_money",
            "working",
            "2026-10-15T10:00:00Z",
            Some("v1"),
        );
        store(path, "no_balance", "working", "2026-10-15T10:05:00Z", None);
        assert!(has_verdict(path, "alpha", "real_money", Some("v1")).unwrap());
        assert!(!has_verdict(path, "alpha", "real_money", Some("v2")).unwrap());
        // A verdict that saw no version stands only while no version is known.
        assert!(!has_verdict(path, "alpha", "no_balance", Some("v1")).unwrap());
        assert!(has_verdict(path, "alpha", "no_balance", None).unwrap());
        assert_eq!(latest_version(path, "alpha").unwrap().unwrap().etag, "v1");

        store(
            path,
            "real_money",
            "test_failed",
            "2026-10-15T11:00:00Z",
            Some("v1"),
        );
        store(
            path,
            "real_money",
            "test_failed",
            "2026-10-15T12:00:00Z",
            Some("v2"),
        );
        assert_eq!(
            failed_attempts(path, "alpha", "real_money", Some("v2")).unwrap(),
            1
        );
        assert_eq!(
            failed_attempts(path, "alpha", "real_money", None).unwrap(),
            2
        );
    }

    #[test]
    fn keeps_one_script_replayed_per_game_and_mode() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        upsert_games(dir.path(), &[game("alpha", "Alpha")]).unwrap();
        let path = dir.path();
        let test = store(
            path,
            "real_money",
            "working",
            "2026-10-15T10:00:00Z",
            Some("v1"),
        );
        let script = |etag: &str| {
            serde_json::json!({
                "mode": "real_money",
                "game_version": { "build_url": "https://x/build.js", "etag": etag },
            })
            .to_string()
        };
        let first = insert_script(path, "alpha", &script("v1"), test).unwrap();
        assert_eq!(
            active_script(path, "alpha", "real_money")
                .unwrap()
                .unwrap()
                .id,
            first
        );
        assert!(
            active_script(path, "alpha", "no_balance")
                .unwrap()
                .is_none()
        );
        let second = insert_script(path, "alpha", &script("v2"), test).unwrap();
        let active = active_script(path, "alpha", "real_money").unwrap().unwrap();
        assert_eq!(active.id, second);
        retire_script(path, second, "step_failed").unwrap();
        assert!(
            active_script(path, "alpha", "real_money")
                .unwrap()
                .is_none()
        );
        let game = select_game(path, "alpha").unwrap().unwrap();
        let reasons: Vec<_> = game
            .scripts
            .iter()
            .map(|script| script.retired_reason.as_deref())
            .collect();
        assert_eq!(reasons, [Some("step_failed"), Some("replaced")]);
        assert!(
            retire_script(path, first, "lost").is_ok(),
            "an already retired script stays"
        );
        assert!(insert_script(path, "alpha", "{\"mode\": \"demo\"}", test).is_err());
    }

    #[test]
    fn reads_verdicts_and_attempts_by_mode() {
        let dir = tempfile::tempdir().unwrap();
        setup(dir.path()).unwrap();
        upsert_games(dir.path(), &[game("alpha", "Alpha")]).unwrap();
        let store = |mode: &str, status: &str| {
            let document = serde_json::json!({ "mode": mode, "status": status,
                                               "started_at": "2026-10-15T10:00:00Z" });
            insert_test(dir.path(), "alpha", &document.to_string()).unwrap();
        };
        store("real_money", "test_failed");
        store("real_money", "test_failed");
        store("no_balance", "working");
        assert!(!has_verdict(dir.path(), "alpha", "real_money", None).unwrap());
        assert!(has_verdict(dir.path(), "alpha", "no_balance", None).unwrap());
        assert_eq!(
            failed_attempts(dir.path(), "alpha", "real_money", None).unwrap(),
            2
        );
        assert_eq!(
            failed_attempts(dir.path(), "alpha", "no_balance", None).unwrap(),
            0
        );
    }
}
