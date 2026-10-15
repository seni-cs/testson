//! The watch over Novedades: what is new there, the queue of games waiting for a demo test, and
//! the lock that keeps to one watch at a time. What it finds is kept in the database.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::catalog::{Fetcher, Game, NOVEDADES_URL, fetch_novedades};
use crate::db::{self, PendingGame};
use crate::harness::parse_duration;

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
        // Windows locks only a handle opened for reading or writing, and append alone is
        // neither.
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
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
    parse_duration(value, "interval", 60)
}

/// Compares the live Novedades collection with the one stored at the previous check, and tests
/// what is new.
///
/// The first check stores the current games and does not test them. Later checks queue the games
/// that were not in the collection before and call `run_one` for the queued games, as
/// `test_pending` describes. `run_one` says what the test of that game came to.
///
/// # Errors
///
/// Fails when the catalogue or the database cannot be read or written, or when a demo test did
/// not reach a verdict.
pub fn check(
    data_dir: &Path,
    fetcher: &impl Fetcher,
    max_attempts: u32,
    run_one: impl FnMut(&Game) -> Tried,
) -> Result<()> {
    let current = fetch_novedades(fetcher)?;
    say!("Novedades: {} games ({NOVEDADES_URL})", current.len());
    let previous = db::select_new_games(data_dir)?;
    if previous.is_empty() {
        db::record_new_games(data_dir, &current, &[])?;
        say!("Baseline saved in the database.");
        say!(
            "No earlier list, so the {} games already on Novedades are not tested.",
            current.len()
        );
        say!("The next run demo-tests only games that appear after this snapshot.");
        return Ok(());
    }

    let added = missing_from(&previous, &current);
    let removed = missing_from(&current, &previous);
    if added.is_empty() {
        say!("No new games since the last check.");
    } else {
        say!("Added since last check: {}", added.len());
        for game in &added {
            say!("  + {}", game.name);
        }
    }
    if !removed.is_empty() {
        say!("Left Novedades: {}", removed.len());
        for game in &removed {
            say!("  - {}", game.name);
        }
    }
    let queued: Vec<&str> = added.iter().map(|game| game.id.as_str()).collect();
    db::record_new_games(data_dir, &current, &queued)?;
    test_pending(data_dir, max_attempts, run_one)
}

/// What one try at the demo test of a queued game came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tried {
    /// The test reached a verdict on the game: `working` or `failing`.
    Verdict,
    /// The agent ran the test, and it reached no verdict: it ended in `test_failed`, or no test
    /// was stored.
    NoVerdict,
    /// The environment stopped the test, so it says nothing about the game: it stopped before
    /// the agent ran, on a session file that cannot be used, a missing tool, or a failed ping, or
    /// the agent found the login session missing or expired.
    Environment,
}

/// Tests the queued games in order, until one does not reach a verdict.
///
/// The attempt is counted before the test runs, so a run that is killed half way still counts.
/// A game whose test reaches a verdict leaves the queue. One the agent tested without a verdict
/// keeps the attempt, and leaves the queue once it has had `max_attempts`; a game that already
/// has them leaves it without being tried. When the environment stopped the test, the attempt
/// is given back. Either way, the games after it stay queued untried, with their attempts as
/// they were: what broke one test, such as an expired session, most likely breaks the next, and
/// each try costs a full agent run.
///
/// # Errors
///
/// Fails when the database cannot be read or written, when a demo test did not reach a verdict,
/// or when a game was given up on.
fn test_pending(
    data_dir: &Path,
    max_attempts: u32,
    mut run_one: impl FnMut(&Game) -> Tried,
) -> Result<()> {
    let pending = db::select_pending_games(data_dir)?;
    say!("Pending tests: {}", pending.len());
    let mut problems = Vec::new();
    let mut queue = pending.into_iter();
    while let Some(PendingGame { game, attempts }) = queue.next() {
        if attempts >= max_attempts {
            db::dequeue(data_dir, &game.id)?;
            problems.push(give_up(&game, attempts));
            continue;
        }
        let attempts = db::count_attempt(data_dir, &game.id)?;
        say!(
            "Testing new game: {} (attempt {attempts} of {max_attempts})",
            game.name
        );
        let problem = match run_one(&game) {
            Tried::Verdict => {
                db::dequeue(data_dir, &game.id)?;
                say!("Test finished: {}", game.name);
                continue;
            }
            Tried::NoVerdict if attempts >= max_attempts => {
                db::dequeue(data_dir, &game.id)?;
                give_up(&game, attempts)
            }
            Tried::NoVerdict => {
                esay!(
                    "Test did not finish: {} (attempt {attempts} of {max_attempts}). It stays \
                     pending.",
                    game.name
                );
                format!("the demo test of {} reached no verdict", game.name)
            }
            Tried::Environment => {
                db::return_attempt(data_dir, &game.id)?;
                esay!(
                    "Test said nothing about {}. No attempt is counted, and it stays pending.",
                    game.name
                );
                format!(
                    "the environment stopped the demo test of {}; fix it (see the error above) \
                     before the next check",
                    game.name
                )
            }
        };
        problems.push(problem);
        let untried: Vec<String> = queue.map(|pending| pending.game.name).collect();
        if !untried.is_empty() {
            esay!(
                "Stopped before the other {} pending games, which would likely fail the same way. \
                 They stay queued for the next check: {}",
                untried.len(),
                untried.join(", ")
            );
        }
        break;
    }
    if !problems.is_empty() {
        bail!(problems.join("\n"));
    }
    Ok(())
}

/// What to report about a game taken off the queue after `attempts` without a verdict.
fn give_up(game: &Game, attempts: u32) -> String {
    format!(
        "gave up on {} after {attempts} attempts without a verdict; run `testson test {:?}` by hand",
        game.name, game.name
    )
}

/// Games in `current` that are not in `previous`.
fn missing_from<'a>(previous: &[Game], current: &'a [Game]) -> Vec<&'a Game> {
    let known: HashSet<&str> = previous.iter().map(|game| game.id.as_str()).collect();
    current
        .iter()
        .filter(|game| !known.contains(game.id.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Scripted(RefCell<String>);

    impl Fetcher for Scripted {
        fn fetch(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Vec<u8>> {
            let body = if url.contains("/api/") {
                self.0.borrow().clone()
            } else {
                r#"{"marketCode":"es","brandId":"11111111-2222-3333-4444-555555555555","countryCode":"ES","jurisdiction":"DGOJ"}"#.to_owned()
            };
            Ok(body.into_bytes())
        }
    }

    fn catalog(names: &[&str]) -> String {
        let items: Vec<String> = names
            .iter()
            .map(|name| {
                let id = name.to_lowercase();
                format!(r#"{{"id":"{id}","name":"{name}","collections":["NewGames"]}}"#)
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    fn data_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        db::setup(dir.path()).unwrap();
        dir
    }

    fn pending(data_dir: &Path) -> Vec<String> {
        attempts(data_dir)
            .into_iter()
            .map(|(name, _)| name)
            .collect()
    }

    /// The queued games by name, with the attempts counted for each.
    fn attempts(data_dir: &Path) -> Vec<(String, u32)> {
        db::select_pending_games(data_dir)
            .unwrap()
            .into_iter()
            .map(|pending| (pending.game.name, pending.attempts))
            .collect()
    }

    #[test]
    fn a_game_without_a_verdict_is_given_up_after_its_attempts() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, 2, |_| {
            panic!("baseline must not test")
        })
        .unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta"]);

        let error = check(dir.path(), &fetcher, 2, |_| Tried::NoVerdict).unwrap_err();
        assert!(error.to_string().contains("Beta"), "{error}");
        assert_eq!(attempts(dir.path()), [("Beta".to_owned(), 1)]);

        let error = check(dir.path(), &fetcher, 2, |_| Tried::NoVerdict)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("gave up on Beta after 2 attempts"),
            "{error}"
        );
        assert!(error.contains(r#"testson test "Beta""#), "{error}");
        assert_eq!(pending(dir.path()), Vec::<String>::new());

        check(dir.path(), &fetcher, 2, |_| panic!("nothing is queued")).unwrap();
    }

    #[test]
    fn the_first_test_without_a_verdict_stops_the_check() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, 3, |_| {
            panic!("baseline must not test")
        })
        .unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta", "Delta", "Gamma"]);

        let mut tested = Vec::new();
        let error = check(dir.path(), &fetcher, 3, |game| {
            tested.push(game.name.clone());
            if game.name == "Beta" {
                Tried::Verdict
            } else {
                Tried::NoVerdict
            }
        })
        .unwrap_err();
        assert!(error.to_string().contains("Delta"), "{error}");
        assert_eq!(tested, ["Beta", "Delta"]);
        assert_eq!(
            attempts(dir.path()),
            [("Delta".to_owned(), 1), ("Gamma".to_owned(), 0)]
        );

        tested.clear();
        check(dir.path(), &fetcher, 3, |game| {
            tested.push(game.name.clone());
            Tried::Verdict
        })
        .unwrap();
        assert_eq!(tested, ["Delta", "Gamma"]);
        assert_eq!(pending(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn the_attempt_is_counted_before_the_test_runs() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, 3, |_| {
            panic!("baseline must not test")
        })
        .unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta"]);
        // A run killed half way never returns; its attempt must already be stored by then.
        check(dir.path(), &fetcher, 3, |_| {
            assert_eq!(attempts(dir.path()), [("Beta".to_owned(), 1)]);
            Tried::Verdict
        })
        .unwrap();
        assert_eq!(pending(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_game_out_of_attempts_leaves_the_queue_untried() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, 2, |_| {
            panic!("baseline must not test")
        })
        .unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta", "Gamma"]);
        // Queue Beta and Gamma without counting an attempt for either.
        check(dir.path(), &fetcher, 2, |_| Tried::Environment).unwrap_err();
        assert_eq!(
            attempts(dir.path()),
            [("Beta".to_owned(), 0), ("Gamma".to_owned(), 0)]
        );
        // Two runs of Beta were killed before they returned.
        for _ in 0..2 {
            db::count_attempt(dir.path(), "beta").unwrap();
        }

        let mut tested = Vec::new();
        let error = check(dir.path(), &fetcher, 2, |game| {
            tested.push(game.name.clone());
            Tried::Verdict
        })
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("gave up on Beta after 2 attempts"),
            "{error}"
        );
        assert_eq!(tested, ["Gamma"]);
        assert_eq!(pending(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn a_test_that_did_not_run_counts_no_attempt_and_stops_the_check() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, 1, |_| {
            panic!("baseline must not test")
        })
        .unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta", "Gamma"]);

        for _ in 0..3 {
            let mut tested = Vec::new();
            let error = check(dir.path(), &fetcher, 1, |game| {
                tested.push(game.name.clone());
                Tried::Environment
            })
            .unwrap_err()
            .to_string();
            assert!(
                error.contains("the environment stopped the demo test of Beta"),
                "{error}"
            );
            assert_eq!(tested, ["Beta"]);
            assert_eq!(
                attempts(dir.path()),
                [("Beta".to_owned(), 0), ("Gamma".to_owned(), 0)]
            );
        }
    }

    #[test]
    fn first_check_is_a_baseline_and_the_next_tests_only_new_games() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, 3, |_| {
            panic!("baseline must not test")
        })
        .unwrap();

        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta"]);
        let mut tested = Vec::new();
        let error = check(dir.path(), &fetcher, 3, |game| {
            tested.push(game.name.clone());
            Tried::NoVerdict
        })
        .unwrap_err();
        assert_eq!(tested, ["Beta"]);
        assert!(error.to_string().contains("Beta"));
        assert_eq!(pending(dir.path()), ["Beta"]);

        check(dir.path(), &fetcher, 3, |_| Tried::Verdict).unwrap();
        assert_eq!(pending(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn every_game_seen_shows_on_the_dashboard() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, 3, |_| Tried::Verdict).unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Beta"]);
        check(dir.path(), &fetcher, 3, |_| Tried::Verdict).unwrap();
        let names: Vec<String> = db::select_new_games(dir.path())
            .unwrap()
            .into_iter()
            .map(|game| game.name)
            .collect();
        assert_eq!(names, ["Beta"]);
        assert!(db::game_exists(dir.path(), "alpha").unwrap());
        assert!(db::game_exists(dir.path(), "beta").unwrap());
    }

    #[test]
    fn a_game_that_comes_back_is_new_again() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha", "Beta"])));
        check(dir.path(), &fetcher, 3, |_| {
            panic!("baseline must not test")
        })
        .unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha"]);
        check(dir.path(), &fetcher, 3, |_| panic!("nothing is new")).unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta"]);
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, 3, |game| {
            tested.push(game.id.clone());
            Tried::Verdict
        })
        .unwrap();
        assert_eq!(tested, ["beta"]);
    }

    #[test]
    fn an_empty_collection_leaves_the_snapshot_alone() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, 3, |_| Tried::NoVerdict).unwrap();
        *fetcher.0.borrow_mut() = "[]".to_owned();
        assert!(check(dir.path(), &fetcher, 3, |_| Tried::NoVerdict).is_err());
        assert_eq!(db::select_new_games(dir.path()).unwrap().len(), 1);
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
