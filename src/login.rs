//! `testson login`: testson logs in to one account in a real Chrome, reached over the `DevTools`
//! port, and saves its cookies and localStorage as the session of that mode.
//!
//! Betsson blocks the login in the browser Playwright launches, so the login runs in Google Chrome
//! instead, started with a profile of its own for each account. The steps are the scripts in
//! `scripts/`, run with playwright-cli. The credentials reach the browser as playwright-cli
//! secrets, typed by name, so no output or log holds them, and the file that holds them is gone
//! before the first step.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::Local;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::harness::find_on_path;
use crate::prompt::session_has_auth;
use crate::router::ASSETS_DIR;
use crate::run::{
    Mode, RUNS_DIR, absolute, create_run_dir, ensure_playwright_cli, playwright_cli, set_mode,
    write_private,
};

/// Opens the login form, or finds the page already logged in. Its `{{BETSSON_URL}}` is replaced.
const OPEN_SCRIPT: &str = include_str!("../scripts/login-open.js");
/// Presses the submit button of the filled form.
const SUBMIT_SCRIPT: &str = include_str!("../scripts/login-submit.js");
/// Says whether the page is logged in yet, and the error the form shows.
const STATE_SCRIPT: &str = include_str!("../scripts/login-state.js");

/// How long the login may take once submitted, a person solving a captcha included.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How often the page is checked for the login.
const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// How long the site gets to store the whole login once its first key appears.
const SETTLE: Duration = Duration::from_secs(3);

/// How long Chrome may take to open its `DevTools` port, and to close once asked.
const CHROME_TIMEOUT: Duration = Duration::from_secs(30);

/// The playwright-cli secrets the credentials are typed with.
const USERNAME: &str = "BETSSON_USERNAME";
const PASSWORD: &str = "BETSSON_PASSWORD";

/// The fields of the login form. Playwright's CSS reaches into their shadow roots.
const USERNAME_FIELD: &str = "#email-input";
const PASSWORD_FIELD: &str = "#password-input";

/// Where `testson login` finds the browser it logs in with.
pub enum Browser {
    /// Start Google Chrome with a profile of the account's own, from this program or the one
    /// installed in the usual place.
    Launch(Option<PathBuf>),
    /// Attach to a browser already running with remote debugging, at this `DevTools` endpoint.
    Attach(String),
}

/// What `login-state.js` returns.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LoginState {
    logged_in: bool,
    message: String,
}

/// Logs in to the account of `mode` and saves its session. Says whether it did.
///
/// # Errors
///
/// Fails when a tool or the credentials are missing, the browser cannot be started or attached
/// to, a step of the login breaks, or the session cannot be saved.
pub fn login(data_dir: &Path, mode: Mode, url: &str, browser: &Browser) -> Result<bool> {
    let credentials_path = data_dir.join(mode.credentials_file());
    let (username, password) = read_credentials(&credentials_path)?;
    ensure_playwright_cli()?;

    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let dir = create_run_dir(
        &data_dir.join(RUNS_DIR),
        &data_dir.join(ASSETS_DIR),
        &format!("login-{mode}-{stamp}"),
    )?;
    let folder = dir
        .file_name()
        .and_then(|name| name.to_str())
        .context("run directory name is not valid UTF-8")?;
    let session_name = format!("betsson-{folder}");

    let (chrome, endpoint) = match browser {
        Browser::Launch(program) => {
            let program = match program {
                Some(program) => program.clone(),
                None => find_chrome().context(
                    "Google Chrome was not found. Install it, or pass its path with --chrome.",
                )?,
            };
            let profile = absolute(&data_dir.join(mode.browser_profile()))?;
            let (chrome, endpoint) = Chrome::launch(&program, &profile, url)?;
            (Some(chrome), endpoint)
        }
        Browser::Attach(endpoint) => (None, endpoint.clone()),
    };
    println!("Logging in to the {mode} account in {endpoint}");

    // The secrets live in the playwright-cli session from here on, so the file goes at once.
    let config = dir.join("cli.config.json");
    write_private(
        &config,
        &json!({ "secrets": { USERNAME: username, PASSWORD: password } }).to_string(),
    )?;
    let attached = playwright_cli(
        &dir,
        &session_name,
        "attach",
        &[
            &format!("--cdp={endpoint}"),
            &format!("--config={}", absolute(&config)?.display()),
        ],
    );
    let removed = fs::remove_file(&config);
    attached.context("could not attach playwright-cli to the browser")?;
    removed.with_context(|| format!("could not remove {}", config.display()))?;

    let saved = log_in_and_save(data_dir, mode, url, &dir, &session_name);
    let _ = playwright_cli(&dir, &session_name, "detach", &[]);
    drop(chrome);
    saved
}

/// Logs in on the attached browser, unless it already is, and saves the session.
fn log_in_and_save(
    data_dir: &Path,
    mode: Mode,
    url: &str,
    dir: &Path,
    session_name: &str,
) -> Result<bool> {
    let open = OPEN_SCRIPT.replace("{{BETSSON_URL}}", &serde_json::to_string(url)?);
    let opened = run_script(dir, session_name, "login-open.js", &open)?;
    if opened == "logged_in" {
        println!("Already logged in.");
    } else {
        playwright_cli(dir, session_name, "fill", &[USERNAME_FIELD, USERNAME])?;
        playwright_cli(dir, session_name, "fill", &[PASSWORD_FIELD, PASSWORD])?;
        run_script(dir, session_name, "login-submit.js", SUBMIT_SCRIPT)?;
        println!(
            "Submitted. If Betsson asks for a captcha or a code, complete it in the Chrome window \
             within {} minutes.",
            LOGIN_TIMEOUT.as_secs() / 60
        );
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        loop {
            let state: LoginState = serde_json::from_value(run_script(
                dir,
                session_name,
                "login-state.js",
                STATE_SCRIPT,
            )?)
            .context("login-state.js returned an unexpected value")?;
            if state.logged_in {
                thread::sleep(SETTLE);
                break;
            }
            if !state.message.is_empty() {
                eprintln!("error: Betsson refused the login: {}", state.message);
                return Ok(false);
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "error: the page was not logged in within {} minutes.",
                    LOGIN_TIMEOUT.as_secs() / 60
                );
                return Ok(false);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    let state_path = dir.join("state.json");
    let state = playwright_cli(
        dir,
        session_name,
        "state-save",
        &[&absolute(&state_path)?.display().to_string()],
    )
    .and_then(|_| {
        set_mode(&state_path, 0o600);
        fs::read_to_string(&state_path).context("could not read the saved state")
    });
    let _ = fs::remove_file(&state_path);
    let state = state?;
    let parsed: Value = serde_json::from_str(&state).context("the saved state is not JSON")?;
    if !session_has_auth(&parsed) {
        bail!("the page looked logged in, but the browser holds no Betsson login");
    }
    let session_path = data_dir.join(mode.session_file());
    let partial = data_dir.join(format!("{}.partial", mode.session_file()));
    write_private(&partial, &state)?;
    fs::rename(&partial, &session_path)
        .with_context(|| format!("could not write {}", session_path.display()))?;
    println!("Saved:  {}", session_path.display());
    Ok(true)
}

/// Writes `script` to `name` in the run directory, runs it with `run-code`, and returns what it
/// returned.
fn run_script(dir: &Path, session: &str, name: &str, script: &str) -> Result<Value> {
    let path = dir.join(name);
    write_private(&path, script)?;
    let output = playwright_cli(
        dir,
        session,
        "run-code",
        &[
            "--filename",
            &absolute(&path)?.display().to_string(),
            "--raw",
        ],
    )
    .with_context(|| format!("{name} failed"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim())
        .with_context(|| format!("{name} returned something other than JSON: {stdout}"))
}

/// Reads `BETSSON_USERNAME` and `BETSSON_PASSWORD` from a file of `KEY=value` lines.
fn read_credentials(path: &Path) -> Result<(String, String)> {
    if !path.is_file() {
        bail!(
            "{} was not found. Create it with the login of this mode's account:\n\
             {USERNAME}=<username or email>\n{PASSWORD}=<password>",
            path.display()
        );
    }
    set_mode(path, 0o600);
    let text =
        fs::read_to_string(path).with_context(|| format!("could not read {}", path.display()))?;
    parse_credentials(&text).with_context(|| format!("{} is not valid", path.display()))
}

fn parse_credentials(text: &str) -> Result<(String, String)> {
    let (mut username, mut password) = (None, None);
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            bail!("a line has no `=`");
        };
        let value = value.trim();
        let value = ['"', '\'']
            .iter()
            .find_map(|&quote| value.strip_prefix(quote)?.strip_suffix(quote))
            .unwrap_or(value)
            .to_owned();
        match key.trim() {
            USERNAME => username = Some(value),
            PASSWORD => password = Some(value),
            other => bail!("unknown key {other}; only {USERNAME} and {PASSWORD} are read"),
        }
    }
    match (username, password) {
        (Some(username), Some(password)) if !username.is_empty() && !password.is_empty() => {
            Ok((username, password))
        }
        _ => bail!("{USERNAME} and {PASSWORD} must both be set"),
    }
}

/// Google Chrome where it is usually installed, or on `PATH`.
fn find_chrome() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(target_os = "macos") {
        let app = "Applications/Google Chrome.app/Contents/MacOS/Google Chrome";
        candidates.push(Path::new("/").join(app));
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push(Path::new(&home).join(app));
        }
    }
    if cfg!(windows) {
        for variable in ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"] {
            if let Some(root) = std::env::var_os(variable) {
                candidates.push(Path::new(&root).join(r"Google\Chrome\Application\chrome.exe"));
            }
        }
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .or_else(|| {
            ["google-chrome", "google-chrome-stable", "chrome"]
                .into_iter()
                .find_map(find_on_path)
        })
}

/// A Chrome that `testson login` started. It is closed when dropped.
struct Chrome {
    child: Child,
}

impl Chrome {
    /// Starts Chrome with `profile` on a free `DevTools` port, and returns it with the endpoint.
    fn launch(program: &Path, profile: &Path, url: &str) -> Result<(Self, String)> {
        fs::create_dir_all(profile)
            .with_context(|| format!("could not create {}", profile.display()))?;
        set_mode(profile, 0o700);
        // Chrome writes the port it chose here, and leaves the file behind when it exits.
        let port_file = profile.join("DevToolsActivePort");
        let _ = fs::remove_file(&port_file);
        let child = Command::new(program)
            .arg("--remote-debugging-port=0")
            // The port alone makes Chrome set `navigator.webdriver`, and Betsson's fraud check
            // then refuses the login.
            .arg("--disable-blink-features=AutomationControlled")
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(["--no-first-run", "--no-default-browser-check", url])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("could not start {}", program.display()))?;
        let mut chrome = Self { child };
        let deadline = Instant::now() + CHROME_TIMEOUT;
        loop {
            let port = fs::read_to_string(&port_file)
                .ok()
                .and_then(|text| text.lines().next()?.trim().parse::<u16>().ok());
            if let Some(port) = port {
                return Ok((chrome, format!("http://127.0.0.1:{port}")));
            }
            if chrome.child.try_wait()?.is_some() {
                bail!(
                    "Chrome exited before opening its DevTools port. Close any Chrome window that \
                     still uses {}, and try again.",
                    profile.display()
                );
            }
            if Instant::now() >= deadline {
                bail!(
                    "Chrome did not open its DevTools port within {}s",
                    CHROME_TIMEOUT.as_secs()
                );
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Chrome {
    /// Asks Chrome to quit, so it writes its profile out, and kills it if it does not.
    fn drop(&mut self) {
        let pid = self.child.id().to_string();
        // SIGTERM on Unix. On Windows, `taskkill` without `/F` closes Chrome's windows, which
        // ends it as gracefully as its own close button does.
        let (program, flag) = if cfg!(windows) {
            ("taskkill", "/PID")
        } else {
            ("kill", "-TERM")
        };
        let quit = Command::new(program)
            .args([flag, &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + CHROME_TIMEOUT;
        while quit.is_ok() && Instant::now() < deadline {
            if !matches!(self.child.try_wait(), Ok(None)) {
                return;
            }
            thread::sleep(Duration::from_millis(200));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_both_credentials() {
        let text =
            "# real money\nBETSSON_USERNAME = ana@example.com\nBETSSON_PASSWORD=\"p=ss word\"\n";
        let (username, password) = parse_credentials(text).unwrap();
        assert_eq!(username, "ana@example.com");
        assert_eq!(password, "p=ss word");
        assert_eq!(
            parse_credentials("BETSSON_USERNAME=a\nBETSSON_PASSWORD='b'\n")
                .unwrap()
                .1,
            "b"
        );
    }

    #[test]
    fn the_scripts_are_single_functions_run_code_can_wrap() {
        for script in [OPEN_SCRIPT, SUBMIT_SCRIPT, STATE_SCRIPT] {
            // run-code wraps the file in parentheses and calls it, so it may not end in `;`.
            assert!(script.trim_end().ends_with('}'));
            assert!(script.contains("async (page) =>"));
            assert!(!script.contains(USERNAME) && !script.contains(PASSWORD));
        }
        assert_eq!(OPEN_SCRIPT.matches("{{BETSSON_URL}}").count(), 1);
    }

    #[test]
    fn rejects_incomplete_or_unknown_credentials() {
        assert!(parse_credentials("BETSSON_USERNAME=a\n").is_err());
        assert!(parse_credentials("BETSSON_USERNAME=a\nBETSSON_PASSWORD=\n").is_err());
        assert!(parse_credentials("BETSSON_USERNAME=a\nBETSSON_PASSWORD=b\nTOKEN=c\n").is_err());
        assert!(parse_credentials("BETSSON_USERNAME a\n").is_err());
    }
}
