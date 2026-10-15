//! The browser `playwright-cli` opens for the agent, and the `playwright-cli` release the prompt
//! was written for.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::harness::{find_on_path, on_path};

/// The program that drives the browser.
const PLAYWRIGHT_CLI: &str = "playwright-cli";

/// The `playwright-cli` release whose commands `prompts/test-game.txt` uses.
pub const PLAYWRIGHT_CLI_VERSION: &str = "0.1.22";

/// How long `playwright-cli --version` may take before the check gives up and only warns.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// The browser the agent opens. `playwright-cli open` would otherwise pick the system Google
/// Chrome, which the install steps do not provide.
pub const BROWSER: &str = "chromium";

/// The commands that install `playwright-cli` and the browser the agent opens.
#[must_use]
pub fn install_hint() -> String {
    format!(
        "npm install -g @playwright/cli@{PLAYWRIGHT_CLI_VERSION} && playwright-cli install-browser {BROWSER}"
    )
}

/// Whether `playwright-cli` is on `PATH`.
#[must_use]
pub fn playwright_cli_installed() -> bool {
    on_path(PLAYWRIGHT_CLI)
}

/// A command that starts `playwright-cli`, found on `PATH` the way the agent CLIs are, so that
/// on Windows it is the `.cmd` shim npm installs.
#[must_use]
pub fn playwright_cli() -> Command {
    Command::new(find_on_path(PLAYWRIGHT_CLI).unwrap_or_else(|| PLAYWRIGHT_CLI.into()))
}

/// Whether the browser shows a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Headed,
    Headless,
}

impl Window {
    /// Headless when asked for or when there is no display to show a window on, which it
    /// notes. Headed otherwise, so the run can be watched.
    #[must_use]
    pub fn choose(headless: bool) -> Self {
        let window = Self::resolve(headless, std::env::consts::OS, |name| {
            std::env::var_os(name).is_some_and(|value| !value.is_empty())
        });
        if window == Self::Headless && !headless {
            esay!(
                "note: there is no display (DISPLAY and WAYLAND_DISPLAY are unset), so the \
                 browser runs headless."
            );
        }
        window
    }

    /// Only Linux can lack a display: there, a window needs `DISPLAY` or `WAYLAND_DISPLAY`.
    fn resolve(headless: bool, os: &str, is_set: impl Fn(&str) -> bool) -> Self {
        let display = os != "linux" || is_set("DISPLAY") || is_set("WAYLAND_DISPLAY");
        if headless || !display {
            Self::Headless
        } else {
            Self::Headed
        }
    }

    /// The options of `playwright-cli open`: the browser, and `--headed` for a window, since
    /// it opens headless without it.
    #[must_use]
    pub fn open_options(self) -> String {
        match self {
            Self::Headed => format!("--browser={BROWSER} --headed"),
            Self::Headless => format!("--browser={BROWSER}"),
        }
    }
}

/// Warns when `playwright-cli` is not the release the prompt was written for, or its version
/// cannot be read. The run goes on either way.
pub fn warn_on_other_version() {
    let found = read_version();
    if let Some(warning) = version_warning(found.as_deref().map_err(String::as_str)) {
        esay!("warning: {warning}");
    }
}

/// The output of `playwright-cli --version`, or why it could not be read within
/// [`VERSION_TIMEOUT`].
///
/// The output is read on a thread while waiting, so a full pipe cannot stall the command.
fn read_version() -> Result<String, String> {
    let mut child = playwright_cli()
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| error.to_string())?;
    let (sender, output) = mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        thread::spawn(move || {
            let mut text = String::new();
            let _ = stdout.read_to_string(&mut text);
            let _ = sender.send(text);
        });
    }
    let deadline = Instant::now() + VERSION_TIMEOUT;
    let timed_out = || format!("it did not answer within {}s", VERSION_TIMEOUT.as_secs());
    loop {
        match child.try_wait().map_err(|error| error.to_string())? {
            Some(status) if status.success() => break,
            Some(status) => return Err(format!("it exited with {status}")),
            None if Instant::now() >= deadline => {
                kill_tree(&mut child);
                return Err(timed_out());
            }
            None => thread::sleep(Duration::from_millis(100)),
        }
    }
    output
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| timed_out())
}

/// Kills `child` and what it started. On Windows `playwright-cli` is a `.cmd` shim, and killing
/// the shim alone would leave its node process running.
fn kill_tree(child: &mut Child) {
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// What to say about the output of `playwright-cli --version`, or why it could not run.
fn version_warning(found: Result<&str, &str>) -> Option<String> {
    let found =
        found.and_then(|output| output.split_whitespace().last().ok_or("it printed nothing"));
    match found {
        Ok(PLAYWRIGHT_CLI_VERSION) => None,
        Ok(version) => Some(format!(
            "playwright-cli is {version}, but the prompt was written for \
             {PLAYWRIGHT_CLI_VERSION} and its commands may differ. Install that release with: \
             npm install -g @playwright/cli@{PLAYWRIGHT_CLI_VERSION}"
        )),
        Err(why) => Some(format!(
            "could not read the playwright-cli version ({why}). The prompt was written for \
             {PLAYWRIGHT_CLI_VERSION}."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(headless: bool, os: &str, set: &[&str]) -> Window {
        Window::resolve(headless, os, |name| set.contains(&name))
    }

    #[test]
    fn headed_unless_asked_or_there_is_no_display() {
        assert_eq!(resolve(false, "windows", &[]), Window::Headed);
        assert_eq!(resolve(false, "macos", &[]), Window::Headed);
        assert_eq!(resolve(false, "linux", &["DISPLAY"]), Window::Headed);
        assert_eq!(
            resolve(false, "linux", &["WAYLAND_DISPLAY"]),
            Window::Headed
        );
        assert_eq!(resolve(false, "linux", &[]), Window::Headless);
        assert_eq!(resolve(true, "windows", &[]), Window::Headless);
        assert_eq!(resolve(true, "linux", &["DISPLAY"]), Window::Headless);
    }

    #[test]
    fn opens_the_installed_chromium_and_not_the_system_chrome() {
        assert_eq!(Window::Headed.open_options(), "--browser=chromium --headed");
        assert_eq!(Window::Headless.open_options(), "--browser=chromium");
        assert!(install_hint().contains("install-browser chromium"));
        assert!(install_hint().contains(&format!("@playwright/cli@{PLAYWRIGHT_CLI_VERSION}")));
    }

    #[test]
    fn warns_on_another_version_or_none() {
        assert_eq!(version_warning(Ok("0.1.22\n")), None);
        let other = version_warning(Ok("0.2.0\n")).unwrap();
        assert!(other.contains("0.2.0"));
        assert!(other.contains("@playwright/cli@0.1.22"));
        assert!(version_warning(Ok("")).unwrap().contains("printed nothing"));
        assert!(
            version_warning(Err("program not found"))
                .unwrap()
                .contains("program not found")
        );
    }
}
