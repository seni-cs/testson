//! The watch over the games of the casino section: which of them still need a test in each mode,
//! and the lock that keeps to one watch at a time. What it finds is kept in the database.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::catalog::{Fetcher, Game, fetch_casino};
use crate::db;
use crate::report;
use crate::run::{self, Mode};

/// How many tests of a game in one mode may end without a verdict before the watch stops trying.
pub const MAX_ATTEMPTS: u32 = 3;

/// Exclusive lock held for the life of one watch process.
#[must_use = "dropping the lock lets another watch process start"]
pub struct WatchLock(File);

impl WatchLock {
    /// # Errors
    ///
    /// Fails when another process holds the lock or the file cannot be opened.
    pub fn acquire(data_dir: &Path) -> Result<Self> {
        let path = data_dir.join("watch.lock");
        fs::create_dir_all(data_dir)
            .with_context(|| format!("could not create {}", data_dir.display()))?;
        // Windows refuses to lock a handle opened for appending alone, so it is also opened for
        // reading.
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("could not open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(TryLockError::WouldBlock) => bail!("another watch process is already running"),
            Err(TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("could not lock {}", path.display()))
            }
        }
    }
}

impl Drop for WatchLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Parses `30m`, `1h`, or a number of seconds, at least 60s. Used as a clap value parser.
///
/// # Errors
///
/// Fails when the text is not an interval or is shorter than 60 seconds.
pub fn parse_interval(value: &str) -> Result<Duration, String> {
    let value = value.trim();
    let (digits, factor) = match value.char_indices().last() {
        Some((index, 's')) => (&value[..index], 1),
        Some((index, 'm')) => (&value[..index], 60),
        Some((index, 'h')) => (&value[..index], 3_600),
        _ => (value, 1),
    };
    let amount: u64 = digits
        .parse()
        .ok()
        .filter(|_| digits.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or("interval must look like 30m, 1h, or 300")?;
    let seconds = amount.checked_mul(factor).ok_or("interval is too large")?;
    if seconds < 60 {
        return Err("interval must be at least 60s".into());
    }
    Ok(Duration::from_secs(seconds))
}

/// Stores the games of the casino section, and tests each in every mode it has no verdict in yet
/// for the version its provider serves now. Games the database did not have before this check go
/// first, then those Betsson lists as new, in its order, then the others by name. With
/// `new_only`, only those Betsson lists as new are tested. `limit`, when given, is how many games
/// are tested; the rest wait for the next check.
///
/// The version is the ETag of the build.js the latest test of the game loaded, asked for again
/// without a browser, so a game is tested again only when it changed. When it is unknown, as
/// for a game that has never loaded, any verdict counts.
///
/// `run_one` returns whether that test reached a verdict on the game. A test that did not, such
/// as one that ended in `test_failed`, is tried again at the next check, until the game has had
/// [`MAX_ATTEMPTS`] in that mode; then only `testson test` tries it. An error from `run_one` means
/// the test could not start, such as a missing tool or session: it stops the check and stores no
/// test, so it counts no attempt. A game that leaves the catalogue stays on the dashboard.
///
/// # Errors
///
/// Fails when the catalogue or the database cannot be read or written, when a test could not
/// start, or when a test did not reach a verdict.
pub fn check(
    data_dir: &Path,
    fetcher: &impl Fetcher,
    limit: Option<usize>,
    new_only: bool,
    mut run_one: impl FnMut(&Game, Mode) -> Result<bool>,
) -> Result<()> {
    let casino = fetch_casino(fetcher)?;
    let games = casino.games;
    println!(
        "Casino: {} games, {} of them listed as new",
        games.len(),
        casino.new_games.len()
    );
    let new_rank = |game: &Game| {
        casino
            .new_games
            .iter()
            .position(|id| *id == game.id)
            .unwrap_or(usize::MAX)
    };
    let stored = db::select_game_ids(data_dir)?;
    let new: Vec<&str> = games
        .iter()
        .filter(|game| !stored.contains(&game.id))
        .map(|game| game.name.as_str())
        .collect();
    // The first check finds every game, too many to name.
    match new.len() {
        0 => {}
        1..=20 => println!("New to the dashboard: {}", new.join(", ")),
        count => println!("New to the dashboard: {count} games"),
    }
    db::upsert_games(data_dir, &games)?;
    let mut pending = Vec::new();
    let mut given_up = Vec::new();
    let mut changed = Vec::new();
    for game in games
        .iter()
        .filter(|game| !new_only || new_rank(game) != usize::MAX)
    {
        let current = run::current_version(data_dir, fetcher, &game.id)?;
        let known = db::latest_version(data_dir, &game.id)?;
        if let (Some(current), Some(known)) = (&current, &known)
            && !report::same_version(&current.etag, &known.etag)
        {
            changed.push(game.name.clone());
        }
        let etag = current.map(|current| current.etag);
        for mode in Mode::ALL {
            if db::has_verdict(data_dir, &game.id, mode.as_str(), etag.as_deref())? {
                continue;
            }
            let attempts = db::failed_attempts(data_dir, &game.id, mode.as_str(), etag.as_deref())?;
            if attempts >= MAX_ATTEMPTS {
                given_up.push(format!("{} ({mode})", game.name));
            } else {
                pending.push((game, mode, etag.clone()));
            }
        }
    }
    if !changed.is_empty() {
        println!("New versions: {}", changed.join(", "));
    }
    // Stable, so the order of the modes, and the catalogue order among the others, hold.
    pending.sort_by_key(|(game, _, _)| (stored.contains(&game.id), new_rank(game)));
    println!("Pending tests: {}", pending.len());
    if let Some(limit) = limit {
        let mut chosen: Vec<&str> = Vec::new();
        pending.retain(|(game, _, _)| {
            if !chosen.contains(&game.id.as_str()) && chosen.len() < limit {
                chosen.push(&game.id);
            }
            chosen.contains(&game.id.as_str())
        });
        println!(
            "Testing {} of them, of {limit} games at most",
            pending.len()
        );
    }
    if !given_up.is_empty() {
        eprintln!(
            "Not retried after {MAX_ATTEMPTS} attempts without a verdict, test them by hand with \
             testson test: {}",
            given_up.join(", ")
        );
    }

    let mut failed = Vec::new();
    for (game, mode, etag) in pending {
        println!("Testing {} ({mode})", game.name);
        if run_one(game, mode)? {
            println!("Test finished: {} ({mode})", game.name);
        } else {
            // An agent that exited with an error may still have reported a verdict, which counts.
            let note = if db::has_verdict(data_dir, &game.id, mode.as_str(), etag.as_deref())? {
                "its verdict is kept and not retried".to_owned()
            } else {
                let attempts =
                    db::failed_attempts(data_dir, &game.id, mode.as_str(), etag.as_deref())?;
                format!("attempt {attempts} of {MAX_ATTEMPTS}")
            };
            eprintln!("Test did not finish: {} ({mode}), {note}.", game.name);
            failed.push(format!("{} ({mode})", game.name));
        }
    }
    if !failed.is_empty() {
        bail!("tests did not finish for: {}", failed.join(", "));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Scripted(RefCell<String>);

    impl Fetcher for Scripted {
        fn etag(&self, _url: &str) -> Result<Option<String>> {
            Ok(Some(ETAG.with_borrow(Clone::clone)))
        }

        fn fetch(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Vec<u8>> {
            let body = if url.contains("/gamecollections") {
                collections(&self.0.borrow())
            } else if url.contains("/api/") {
                self.0.borrow().clone()
            } else {
                r#"{"marketCode":"es","brandId":"11111111-2222-3333-4444-555555555555","countryCode":"ES","jurisdiction":"DGOJ"}"#.to_owned()
            };
            Ok(body.into_bytes())
        }
    }

    thread_local! {
        /// The games Betsson lists as new.
        static NEW_GAMES: RefCell<Vec<&'static str>> = const { RefCell::new(Vec::new()) };
    }

    /// The collections of a catalogue: every game is in the casino section but `other`, which is
    /// in the live casino.
    fn collections(catalog: &str) -> String {
        let games: Vec<serde_json::Value> = serde_json::from_str(catalog).unwrap();
        let casino: Vec<&str> = games
            .iter()
            .filter_map(|game| game["id"].as_str())
            .filter(|id| *id != "other")
            .collect();
        serde_json::json!([
            { "id": "LiveCasino", "games": ["other"], "subCollections": [] },
            { "id": "Casino", "games": [], "subCollections": [
                { "id": "NewGames", "games": NEW_GAMES.with_borrow(Clone::clone) },
                { "id": "Slots", "games": casino }
            ] }
        ])
        .to_string()
    }

    fn catalog(names: &[&str]) -> String {
        let items: Vec<String> = names
            .iter()
            .map(|name| {
                let id = name.to_lowercase();
                format!(r#"{{"id":"{id}","name":"{name}","provider":"Relax"}}"#)
            })
            .chain([r#"{"id":"other","name":"Other","provider":"Evolution"}"#.to_owned()])
            .collect();
        format!("[{}]", items.join(","))
    }

    fn data_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        db::setup(dir.path()).unwrap();
        dir
    }

    /// Store a test of `game_id` in `mode` that ended in `status`.
    fn store(data_dir: &Path, game_id: &str, mode: Mode, status: &str) {
        let document = serde_json::json!({
            "schema_version": 2,
            "mode": mode.as_str(),
            "status": status,
            "summary": "Stored by a test.",
            "started_at": "2026-10-15T10:00:00Z",
            "duration_ms": 1,
            "steps": [],
        });
        db::insert_test(data_dir, game_id, &document.to_string()).unwrap();
    }

    #[test]
    fn tests_every_game_of_the_casino_section_in_both_modes() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha", "Beta"])));
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, None, false, |game, mode| {
            tested.push((game.id.clone(), mode));
            Ok(true)
        })
        .unwrap();
        assert_eq!(
            tested,
            [
                ("alpha".to_owned(), Mode::RealMoney),
                ("alpha".to_owned(), Mode::NoBalance),
                ("beta".to_owned(), Mode::RealMoney),
                ("beta".to_owned(), Mode::NoBalance),
            ]
        );
        assert!(db::game_exists(dir.path(), "alpha").unwrap());
        assert!(!db::game_exists(dir.path(), "other").unwrap());
    }

    thread_local! {
        /// The ETag the server of the provider sends for the client of every game.
        static ETAG: RefCell<String> = RefCell::new("\"v1\"".to_owned());
    }

    #[test]
    fn a_new_version_is_tested_again() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, None, false, |_, _| Ok(true)).unwrap();
        for mode in Mode::ALL {
            let document = serde_json::json!({
                "mode": mode.as_str(),
                "status": "working",
                "started_at": "2026-10-15T10:00:00Z",
                "game_version": { "build_url": "https://x/build.js", "etag": "\"v1\"" },
            });
            db::insert_test(dir.path(), "alpha", &document.to_string()).unwrap();
        }
        let tested = |fetcher: &Scripted| {
            let mut tested = Vec::new();
            check(dir.path(), fetcher, None, false, |_, mode| {
                tested.push(mode);
                Ok(true)
            })
            .unwrap();
            tested
        };
        assert_eq!(tested(&fetcher), []);
        ETAG.set("\"v2\"".to_owned());
        assert_eq!(tested(&fetcher), Mode::ALL);
    }

    #[test]
    fn new_games_go_first_and_the_limit_counts_games() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha", "Beta"])));
        check(dir.path(), &fetcher, Some(0), false, |_, _| {
            panic!("over the limit")
        })
        .unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta", "Gamma"]);
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, Some(1), false, |game, mode| {
            tested.push((game.id.clone(), mode));
            Ok(true)
        })
        .unwrap();
        assert_eq!(
            tested,
            [
                ("gamma".to_owned(), Mode::RealMoney),
                ("gamma".to_owned(), Mode::NoBalance),
            ]
        );
    }

    #[test]
    fn the_games_betsson_lists_as_new_go_next_or_alone() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha", "Beta", "Gamma"])));
        check(dir.path(), &fetcher, Some(0), false, |_, _| {
            panic!("over the limit")
        })
        .unwrap();
        NEW_GAMES.set(vec!["gamma", "beta"]);
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, None, false, |game, mode| {
            tested.push((game.id.clone(), mode));
            Ok(true)
        })
        .unwrap();
        let order: Vec<&str> = tested.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(order, ["gamma", "gamma", "beta", "beta", "alpha", "alpha"]);

        let dir = data_dir();
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, None, true, |game, _| {
            tested.push(game.id.clone());
            Ok(true)
        })
        .unwrap();
        assert_eq!(tested, ["gamma", "gamma", "beta", "beta"]);
        // Every game of the casino section is stored, whichever are tested.
        assert!(db::game_exists(dir.path(), "alpha").unwrap());
    }

    #[test]
    fn only_a_verdict_takes_a_game_and_mode_off_the_list() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha", "Beta"])));
        check(dir.path(), &fetcher, None, false, |_, _| Ok(true)).unwrap();
        store(dir.path(), "alpha", Mode::RealMoney, "working");
        store(dir.path(), "alpha", Mode::NoBalance, "failing");
        store(dir.path(), "beta", Mode::RealMoney, "test_failed");

        let mut tested = Vec::new();
        let error = check(dir.path(), &fetcher, None, false, |game, mode| {
            tested.push((game.id.clone(), mode));
            Ok(false)
        })
        .unwrap_err();
        assert_eq!(
            tested,
            [
                ("beta".to_owned(), Mode::RealMoney),
                ("beta".to_owned(), Mode::NoBalance),
            ]
        );
        assert!(error.to_string().contains("Beta (real-money)"));
    }

    #[test]
    fn stops_retrying_after_the_maximum_attempts() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, None, false, |_, _| Ok(true)).unwrap();
        for _ in 0..MAX_ATTEMPTS {
            store(dir.path(), "alpha", Mode::RealMoney, "test_failed");
        }
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, None, false, |_, mode| {
            tested.push(mode);
            Ok(true)
        })
        .unwrap();
        assert_eq!(tested, [Mode::NoBalance]);
    }

    #[test]
    fn a_test_that_cannot_start_stops_the_check() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha", "Beta"])));
        let mut runs = 0;
        let error = check(dir.path(), &fetcher, None, false, |_, _| {
            runs += 1;
            Err(anyhow::anyhow!("playwright-cli is not on PATH"))
        })
        .unwrap_err();
        assert_eq!(runs, 1);
        assert!(error.to_string().contains("playwright-cli"));
    }

    #[test]
    fn a_casino_section_without_games_is_an_error() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new("[]".to_owned()));
        assert!(
            check(dir.path(), &fetcher, None, false, |_, _| panic!(
                "nothing to test"
            ))
            .is_err()
        );
    }

    #[test]
    fn parses_intervals() {
        assert_eq!(parse_interval("30m"), Ok(Duration::from_secs(1_800)));
        assert_eq!(parse_interval(" 1h "), Ok(Duration::from_secs(3_600)));
        assert_eq!(parse_interval("300"), Ok(Duration::from_secs(300)));
        assert_eq!(parse_interval("60s"), Ok(Duration::from_secs(60)));
        assert!(parse_interval("59s").is_err());
        assert!(parse_interval("30min").is_err());
        assert!(parse_interval("1H").is_err());
        assert!(parse_interval("+5m").is_err());
        assert!(parse_interval("m").is_err());
    }

    #[test]
    fn a_second_lock_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let _lock = WatchLock::acquire(dir.path()).unwrap();
        assert!(WatchLock::acquire(dir.path()).is_err());
    }
}
