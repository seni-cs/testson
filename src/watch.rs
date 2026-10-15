//! The watch over Novedades: what is new there, the queue of games waiting for a demo test, and
//! the lock that keeps to one watch at a time. What it finds is kept in the database.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::catalog::{Fetcher, Game, NOVEDADES_URL, fetch_novedades};
use crate::db;

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
        let file = OpenOptions::new()
            .create(true)
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

/// Compares the live Novedades collection with the one stored at the previous check, and tests
/// what is new.
///
/// The first check stores the current games and does not test them. Later checks queue the games
/// that were not in the collection before and call `run_one` for each queued game. `run_one`
/// returns whether that test reached a verdict on the game; a game whose test did not stays
/// queued for the next check.
///
/// # Errors
///
/// Fails when the catalogue or the database cannot be read or written, or when a demo test did
/// not finish.
pub fn check(
    data_dir: &Path,
    fetcher: &impl Fetcher,
    mut run_one: impl FnMut(&Game) -> bool,
) -> Result<()> {
    let current = fetch_novedades(fetcher)?;
    println!("Novedades: {} games ({NOVEDADES_URL})", current.len());
    let previous = db::select_new_games(data_dir)?;
    if previous.is_empty() {
        db::record_new_games(data_dir, &current, &[])?;
        println!("Baseline saved in the database.");
        println!(
            "No earlier list, so the {} games already on Novedades are not tested.",
            current.len()
        );
        println!("The next run demo-tests only games that appear after this snapshot.");
        return Ok(());
    }

    let added = missing_from(&previous, &current);
    let removed = missing_from(&current, &previous);
    if added.is_empty() {
        println!("No new games since the last check.");
    } else {
        println!("Added since last check: {}", added.len());
        for game in &added {
            println!("  + {}", game.name);
        }
    }
    if !removed.is_empty() {
        println!("Left Novedades: {}", removed.len());
        for game in &removed {
            println!("  - {}", game.name);
        }
    }
    let queued: Vec<&str> = added.iter().map(|game| game.id.as_str()).collect();
    db::record_new_games(data_dir, &current, &queued)?;
    let pending = db::select_pending_games(data_dir)?;
    println!("Pending tests: {}", pending.len());

    let mut failed = Vec::new();
    for game in pending {
        println!("Testing new game: {}", game.name);
        if run_one(&game) {
            db::dequeue(data_dir, &game.id)?;
            println!("Test finished: {}", game.name);
        } else {
            eprintln!("Test did not finish: {}. It stays pending.", game.name);
            failed.push(game.name);
        }
    }
    if !failed.is_empty() {
        bail!("demo test failed for: {}", failed.join(", "));
    }
    Ok(())
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
        db::select_pending_games(data_dir)
            .unwrap()
            .into_iter()
            .map(|game| game.name)
            .collect()
    }

    #[test]
    fn first_check_is_a_baseline_and_the_next_tests_only_new_games() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, |_| panic!("baseline must not test")).unwrap();

        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta"]);
        let mut tested = Vec::new();
        let error = check(dir.path(), &fetcher, |game| {
            tested.push(game.name.clone());
            false
        })
        .unwrap_err();
        assert_eq!(tested, ["Beta"]);
        assert!(error.to_string().contains("Beta"));
        assert_eq!(pending(dir.path()), ["Beta"]);

        check(dir.path(), &fetcher, |_| true).unwrap();
        assert_eq!(pending(dir.path()), Vec::<String>::new());
    }

    #[test]
    fn every_game_seen_shows_on_the_dashboard() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, |_| true).unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Beta"]);
        check(dir.path(), &fetcher, |_| true).unwrap();
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
        check(dir.path(), &fetcher, |_| panic!("baseline must not test")).unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha"]);
        check(dir.path(), &fetcher, |_| panic!("nothing is new")).unwrap();
        *fetcher.0.borrow_mut() = catalog(&["Alpha", "Beta"]);
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, |game| {
            tested.push(game.id.clone());
            true
        })
        .unwrap();
        assert_eq!(tested, ["beta"]);
    }

    #[test]
    fn an_empty_collection_leaves_the_snapshot_alone() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha"])));
        check(dir.path(), &fetcher, |_| false).unwrap();
        *fetcher.0.borrow_mut() = "[]".to_owned();
        assert!(check(dir.path(), &fetcher, |_| false).is_err());
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
