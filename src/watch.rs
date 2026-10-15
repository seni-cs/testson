//! The watch over the games of the provider: which of them still need a test in each mode, and
//! the lock that keeps to one watch at a time. What it finds is kept in the database.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::catalog::{Fetcher, Game, PROVIDER, fetch_provider_games};
use crate::db;
use crate::run::Mode;

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

/// Stores the games the catalogue has of the provider, and tests each in every mode it has no
/// verdict in yet.
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
    mut run_one: impl FnMut(&Game, Mode) -> Result<bool>,
) -> Result<()> {
    let games = fetch_provider_games(fetcher)?;
    println!("{PROVIDER}: {} games", games.len());
    db::upsert_games(data_dir, &games)?;
    let mut pending = Vec::new();
    let mut given_up = Vec::new();
    for game in &games {
        for mode in Mode::ALL {
            if db::has_verdict(data_dir, &game.id, mode.as_str())? {
                continue;
            }
            if db::failed_attempts(data_dir, &game.id, mode.as_str())? >= MAX_ATTEMPTS {
                given_up.push(format!("{} ({mode})", game.name));
            } else {
                pending.push((game, mode));
            }
        }
    }
    println!("Pending tests: {}", pending.len());
    if !given_up.is_empty() {
        eprintln!(
            "Not retried after {MAX_ATTEMPTS} attempts without a verdict, test them by hand with \
             testson test: {}",
            given_up.join(", ")
        );
    }

    let mut failed = Vec::new();
    for (game, mode) in pending {
        println!("Testing {} ({mode})", game.name);
        if run_one(game, mode)? {
            println!("Test finished: {} ({mode})", game.name);
        } else {
            let attempts = db::failed_attempts(data_dir, &game.id, mode.as_str())?;
            eprintln!(
                "Test did not finish: {} ({mode}), attempt {attempts} of {MAX_ATTEMPTS}.",
                game.name
            );
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
                format!(r#"{{"id":"{id}","name":"{name}","provider":"PragmaticPlay"}}"#)
            })
            .chain([r#"{"id":"other","name":"Other","provider":"Other"}"#.to_owned()])
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
    fn tests_every_game_of_the_provider_in_both_modes() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha", "Beta"])));
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, |game, mode| {
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

    #[test]
    fn only_a_verdict_takes_a_game_and_mode_off_the_list() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new(catalog(&["Alpha", "Beta"])));
        check(dir.path(), &fetcher, |_, _| Ok(true)).unwrap();
        store(dir.path(), "alpha", Mode::RealMoney, "working");
        store(dir.path(), "alpha", Mode::NoBalance, "failing");
        store(dir.path(), "beta", Mode::RealMoney, "test_failed");

        let mut tested = Vec::new();
        let error = check(dir.path(), &fetcher, |game, mode| {
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
        check(dir.path(), &fetcher, |_, _| Ok(true)).unwrap();
        for _ in 0..MAX_ATTEMPTS {
            store(dir.path(), "alpha", Mode::RealMoney, "test_failed");
        }
        let mut tested = Vec::new();
        check(dir.path(), &fetcher, |_, mode| {
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
        let error = check(dir.path(), &fetcher, |_, _| {
            runs += 1;
            Err(anyhow::anyhow!("playwright-cli is not on PATH"))
        })
        .unwrap_err();
        assert_eq!(runs, 1);
        assert!(error.to_string().contains("playwright-cli"));
    }

    #[test]
    fn a_catalogue_without_the_provider_is_an_error() {
        let dir = data_dir();
        let fetcher = Scripted(RefCell::new("[]".to_owned()));
        assert!(check(dir.path(), &fetcher, |_, _| panic!("nothing to test")).is_err());
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
