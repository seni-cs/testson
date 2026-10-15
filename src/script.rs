//! Replay scripts: the playwright-cli commands an agent run left for a game in one mode, the rules
//! that keep them safe to run unattended, and the run that replays one without an agent.
//!
//! A script holds only what is particular to the game. The replay opens the browser, records the
//! video, captures the postMessages, loads the saved login and collects the evidence itself, so the
//! login never enters a script, and checks the version of the game before it bets.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{LazyLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use regex::Regex;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::prompt::parse_betsson_url;
use crate::report::{self, GameVersion};
use crate::run::{Mode, SESSION_COPY};

/// The highest stake a test may bet, in cents.
const MAX_STAKE_CENTS: i64 = 100;
/// The highest balance the account without balance may have, in cents: the lowest stake of a slot.
const MAX_NO_BALANCE_CENTS: i64 = 10;
/// The highest number a script may type, which keeps the loss limit of the gaming session low.
const MAX_TYPED_NUMBER: f64 = 10.0;
/// The longest a step may wait or probe.
const MAX_TIMEOUT_MS: u64 = 120_000;
/// How long a probe keeps trying when its step sets no `timeout_ms`.
const PROBE_TIMEOUT_MS: u64 = 30_000;
/// How often a probe tries again.
const POLL_INTERVAL: Duration = Duration::from_secs(1);
/// How long one playwright-cli command may take before it is stopped.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(90);
/// How long the output of a command that exited is waited for.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);
/// How much of the output of a command a failed step quotes.
const QUOTED_OUTPUT: usize = 600;

/// Commands that change the page. Only actions and the spin may run them.
const ACTION_COMMANDS: &[&str] = &[
    "goto",
    "reload",
    "click",
    "dblclick",
    "fill",
    "type",
    "press",
    "select",
    "check",
    "uncheck",
    "hover",
    "mousemove",
    "mousedown",
    "mouseup",
    "mousewheel",
    "dialog-accept",
    "dialog-dismiss",
];
/// Commands that only read the page. Any step may run them.
const READ_COMMANDS: &[&str] = &["find", "eval"];
/// The option a command of a script may start with, which prints only what it returns.
const RAW: &str = "--raw";

/// Controls a script must never reach: the cashier, demo play, bonus purchases and anything that
/// raises the stake, as the hard rules of the prompt say, and the storage that holds the login.
static FORBIDDEN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(deposit\w*|depositar|ingresar|retirar|withdraw\w*|cashier|cajero|caja|demo|modo prueba|jugar gratis|comprar|buy|bono|bonus|autoplay|auto play|auto-play|ante bet|double chance|gamble|localstorage|sessionstorage|cookie\w*)\b",
    )
    .expect("forbidden pattern")
});

/// What an `eval` that only reads must not hold: anything that acts, waits, loops or assigns.
static ACTING_CODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)click|dispatch|submit|postmessage|settimeout|setinterval|requestanimationframe|\bwhile\b|\bfor\s*\(|fetch|xmlhttprequest|websocket|\blocation\b|\bopen\s*\(|\bimport\b|\beval\s*\(|\bfunction\b|(^|[^=!<>])=($|[^=>])",
    )
    .expect("acting code pattern")
});

/// A replay script, as `schema/test-script.schema.json` describes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Script {
    #[serde(rename = "script_version")]
    pub version: u64,
    pub mode: Mode,
    pub game_version: GameVersion,
    pub steps: Vec<Step>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub role: Role,
    pub description: String,
    #[serde(default)]
    pub commands: Vec<Vec<String>>,
    #[serde(default)]
    pub source: Source,
    pub url_matches: Option<String>,
    pub matches: Option<String>,
    pub name: Option<Reading>,
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub screenshot: bool,
    /// An action on what some visits show and others do not, skipped when it finds nothing.
    #[serde(default)]
    pub optional: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Action,
    Spin,
    Pause,
    Expect,
    Read,
}

impl Role {
    fn acts(self) -> bool {
        matches!(self, Self::Action | Self::Spin)
    }

    fn probes(self) -> bool {
        matches!(self, Self::Expect | Self::Read)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    #[default]
    Page,
    Messages,
    Response,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Reading {
    BalanceBefore,
    Stake,
    Win,
    BalanceAfter,
    GameBalanceBefore,
    GameBalanceAfter,
    Currency,
    Language,
}

impl Reading {
    fn as_str(self) -> &'static str {
        match self {
            Self::BalanceBefore => "balance_before",
            Self::Stake => "stake",
            Self::Win => "win",
            Self::BalanceAfter => "balance_after",
            Self::GameBalanceBefore => "game_balance_before",
            Self::GameBalanceAfter => "game_balance_after",
            Self::Currency => "currency",
            Self::Language => "language",
        }
    }

    /// The currency and the language are text; every other reading is an amount of money.
    fn is_money(self) -> bool {
        !matches!(self, Self::Currency | Self::Language)
    }
}

/// Reads a script and checks it follows the schema and the safety rules for `mode`.
///
/// # Errors
///
/// Fails, saying why, at the first rule the script breaks.
pub fn parse(text: &str, mode: Mode) -> Result<Script> {
    let script: Script = serde_json::from_str(text).context("the script is not valid")?;
    validate(&script, mode)?;
    Ok(script)
}

/// Checks the rules the schema cannot express, and those that keep an unattended replay to one
/// spin at the minimum stake, away from the cashier, demo play and the login.
///
/// # Errors
///
/// Fails, saying why, at the first rule the script breaks.
pub fn validate(script: &Script, mode: Mode) -> Result<()> {
    ensure!(script.version == 1, "script_version must be 1");
    ensure!(
        script.mode == mode,
        "the script is for {}, not {}",
        script.mode,
        mode
    );
    report::check_version(&script.game_version)?;
    ensure!(!script.steps.is_empty(), "the script has no steps");
    let spins: Vec<usize> = positions(script, |step| step.role == Role::Spin);
    match mode {
        Mode::RealMoney => ensure!(spins.len() == 1, "a real_money script spins exactly once"),
        Mode::NoBalance => ensure!(spins.len() <= 1, "a no_balance script spins at most once"),
    }
    let spin = spins.first().copied();
    for (index, step) in script.steps.iter().enumerate() {
        check_step(step, spin.is_some_and(|spin| index > spin))
            .with_context(|| format!("step {} ({})", index + 1, step.description))?;
    }
    let reading = |name| positions(script, |step| step.name == Some(name));
    let balance_before = reading(Reading::BalanceBefore);
    ensure!(
        balance_before.len() == 1,
        "the script reads balance_before exactly once"
    );
    let first_action = positions(script, |step| step.role.acts());
    ensure!(
        first_action
            .first()
            .is_none_or(|&action| balance_before[0] < action),
        "the script reads balance_before before it does anything"
    );
    for name in [
        Reading::Stake,
        Reading::Win,
        Reading::BalanceAfter,
        Reading::GameBalanceBefore,
        Reading::GameBalanceAfter,
        Reading::Currency,
        Reading::Language,
    ] {
        ensure!(
            reading(name).len() <= 1,
            "the script reads {} more than once",
            name.as_str()
        );
    }
    ensure!(
        !reading(Reading::Currency).is_empty(),
        "the script reads the currency"
    );
    if let Some(spin) = spin {
        let stake = reading(Reading::Stake).first().copied();
        ensure!(
            stake.is_some_and(|stake| stake < spin),
            "the script reads the stake before it spins"
        );
        // The stake checked before the spin must be the one it bets.
        ensure!(
            stake.is_some_and(|stake| !script.steps[stake + 1..spin]
                .iter()
                .any(|step| step.role.acts())),
            "nothing acts between the read of the stake and the spin"
        );
    }
    check_balance_readings(script, spin)?;
    if mode == Mode::NoBalance {
        ensure!(
            script.steps.iter().any(|step| step.role == Role::Expect),
            "a no_balance script expects the message that the balance is not enough"
        );
        ensure!(
            !reading(Reading::Language).is_empty(),
            "a no_balance script reads the language of that message"
        );
    }
    Ok(())
}

/// The Betsson balance and the balance inside the game are both read around the spin, so the
/// replay can check each follows the round, or does not change when the bet is refused.
fn check_balance_readings(script: &Script, spin: Option<usize>) -> Result<()> {
    let reading = |name| positions(script, |step| step.name == Some(name));
    if let Some(spin) = spin {
        for name in [
            Reading::Win,
            Reading::BalanceAfter,
            Reading::GameBalanceAfter,
        ] {
            ensure!(
                reading(name).iter().all(|&at| at > spin),
                "the script reads {} after it spins",
                name.as_str()
            );
        }
        ensure!(
            reading(Reading::GameBalanceBefore)
                .iter()
                .all(|&at| at < spin),
            "the script reads game_balance_before before it spins"
        );
        for name in [
            Reading::BalanceAfter,
            Reading::GameBalanceBefore,
            Reading::GameBalanceAfter,
        ] {
            ensure!(
                !reading(name).is_empty(),
                "a script that spins reads {}",
                name.as_str()
            );
        }
    }
    // The balances after the round are awaited until they are the ones the win makes, so the win
    // is read first.
    for after in [Reading::BalanceAfter, Reading::GameBalanceAfter] {
        ensure!(
            reading(Reading::Win)
                .iter()
                .all(|&win| reading(after).iter().all(|&at| win < at)),
            "the script reads the win before {}",
            after.as_str()
        );
    }
    Ok(())
}

fn positions(script: &Script, keep: impl Fn(&Step) -> bool) -> Vec<usize> {
    script
        .steps
        .iter()
        .enumerate()
        .filter(|(_, step)| keep(step))
        .map(|(index, _)| index)
        .collect()
}

fn check_step(step: &Step, after_spin: bool) -> Result<()> {
    ensure!(
        !step.description.trim().is_empty(),
        "the description is empty"
    );
    let commands_needed = match step.role {
        Role::Action | Role::Spin => true,
        Role::Pause => false,
        Role::Expect | Role::Read => step.source == Source::Page,
    };
    ensure!(
        commands_needed != step.commands.is_empty(),
        "{}",
        if commands_needed {
            "the step needs commands"
        } else {
            "the step must not have commands"
        }
    );
    ensure!(
        step.role.probes() || (step.source == Source::Page && step.url_matches.is_none()),
        "only a probe reads a source"
    );
    ensure!(
        (step.source == Source::Response) == step.url_matches.is_some(),
        "url_matches goes with the response source, and only with it"
    );
    match step.role {
        Role::Expect => ensure!(step.matches.is_some(), "an expect needs matches"),
        Role::Pause => ensure!(step.timeout_ms.is_some(), "a pause needs timeout_ms"),
        _ => {}
    }
    ensure!(
        (step.role == Role::Read) == step.name.is_some(),
        "a read, and only a read, names its reading"
    );
    ensure!(
        !after_spin || !step.role.acts(),
        "nothing is done after the spin but waiting, reading and checking"
    );
    if step.role == Role::Spin {
        check_one_press(&step.commands)?;
    }
    ensure!(
        !step.optional || step.role == Role::Action,
        "only an action may be optional"
    );
    ensure!(
        step.timeout_ms
            .is_none_or(|timeout| timeout <= MAX_TIMEOUT_MS),
        "timeout_ms is above {MAX_TIMEOUT_MS}"
    );
    for pattern in [&step.matches, &step.url_matches].into_iter().flatten() {
        Regex::new(pattern).with_context(|| format!("{pattern:?} is not a regular expression"))?;
    }
    for command in &step.commands {
        check_command(command, step.role).with_context(|| format!("command {command:?}"))?;
    }
    Ok(())
}

fn check_command(command: &[String], role: Role) -> Result<()> {
    let command = command.strip_prefix(&[RAW.to_owned()]).unwrap_or(command);
    let Some((name, arguments)) = command.split_first() else {
        bail!("the command is empty");
    };
    let allowed = READ_COMMANDS.contains(&name.as_str())
        || (role.acts() && ACTION_COMMANDS.contains(&name.as_str()));
    ensure!(allowed, "{name} is not a command this step may run");
    for argument in arguments {
        ensure!(
            !argument.starts_with('-') || argument.parse::<f64>().is_ok(),
            "the option {argument} is not allowed"
        );
        if let Some(found) = FORBIDDEN.find(argument) {
            bail!(
                "it targets {:?}, which a test must never touch",
                found.as_str()
            );
        }
    }
    match name.as_str() {
        "goto" => {
            let url = arguments.first().context("goto needs a URL")?;
            parse_betsson_url(url).map_err(anyhow::Error::msg)?;
        }
        "fill" | "type" => {
            let text = arguments.last().context("the command needs a text")?;
            if let Ok(number) = text.trim().replace(',', ".").parse::<f64>() {
                ensure!(
                    number <= MAX_TYPED_NUMBER,
                    "it types {number}, above the {MAX_TYPED_NUMBER} a loss limit may be"
                );
            }
        }
        // Space spins most games, so it is pressed only by the spin.
        "press" => ensure!(
            role == Role::Spin
                || !arguments
                    .iter()
                    .any(|key| key.trim().is_empty() || key.eq_ignore_ascii_case("space")),
            "only the spin presses Space"
        ),
        "eval" => {
            let code = arguments.first().context("eval needs a function")?;
            if let Some(found) = ACTING_CODE.find(code) {
                bail!(
                    "the eval does more than read the page: {:?}",
                    found.as_str()
                );
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether a currency, as read, is the euro: `€`, `EUR`, or an amount written with either.
fn is_euro(text: &str) -> bool {
    text.contains('€') || text.to_uppercase().contains("EUR")
}

/// Whether the language of a message, read as a code such as `ES` or `es-ES` or as the message
/// itself, is Spanish.
fn is_spanish(text: &str) -> bool {
    const WORDS: [&str; 5] = ["saldo", "fondos", "insuficiente", "suficiente", "depósito"];
    let text = text.trim().trim_matches('"').to_lowercase();
    text == "es"
        || text.starts_with("es-")
        || text.starts_with("es_")
        || WORDS.iter().any(|word| text.contains(word))
}

/// Reads an amount of money as a page writes it, such as `12,40 €`, `€1,234.50` or `0.20`, in
/// cents.
#[must_use]
pub fn parse_cents(text: &str) -> Option<i64> {
    static AMOUNT: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"-?\d[\d.,]*").expect("amount pattern"));
    let found = AMOUNT.find(text)?.as_str();
    let negative = found.starts_with('-');
    let digits = found.trim_start_matches('-').trim_end_matches(['.', ',']);
    // With both kinds of separator, the last one is the decimal one. With one kind, it is the
    // decimal one only when it appears once and is not followed by three digits, as in `1.000`.
    let decimal = digits.rfind(['.', ',']).filter(|&at| {
        let both = digits.contains('.') && digits.contains(',');
        let kind = &digits[at..=at];
        both || (digits.matches(kind).count() == 1 && digits.len() - at - 1 != 3)
    });
    let (whole, fraction) = match decimal {
        Some(at) => (&digits[..at], &digits[at + 1..]),
        None => (digits, ""),
    };
    let whole: String = whole.chars().filter(char::is_ascii_digit).collect();
    let mut fraction: String = fraction.chars().take(2).collect();
    while fraction.len() < 2 {
        fraction.push('0');
    }
    let cents = whole
        .parse::<i64>()
        .unwrap_or(0)
        .checked_mul(100)?
        .checked_add(fraction.parse::<i64>().ok()?)?;
    Some(if negative { -cents } else { cents })
}

/// What a playwright-cli command printed, and whether it succeeded.
#[derive(Debug, Clone)]
pub struct Reply {
    pub ok: bool,
    pub output: String,
}

/// The browser a replay drives. Tests replace it, so they never open one.
pub trait Browser {
    /// Runs one playwright-cli command, given without the session option.
    fn run(&mut self, arguments: &[String]) -> Reply;

    /// Waits, for a pause or between two probes.
    fn sleep(&mut self, duration: Duration) {
        thread::sleep(duration);
    }
}

/// The browser of a playwright-cli session, run from the run directory.
pub struct Cli {
    pub program: PathBuf,
    pub session: String,
    pub dir: PathBuf,
}

impl Browser for Cli {
    /// Runs the command, and stops it after [`COMMAND_TIMEOUT`].
    fn run(&mut self, arguments: &[String]) -> Reply {
        let failed = |output: String| Reply { ok: false, output };
        let spawned = Command::new(&self.program)
            .arg("--session")
            .arg(&self.session)
            .args(arguments)
            .current_dir(&self.dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => return failed(format!("playwright-cli failed to start: {error}")),
        };
        let (sender, receiver) = mpsc::channel();
        let pipes: [Option<Box<dyn Read + Send>>; 2] = [
            child.stdout.take().map(|pipe| Box::new(pipe) as _),
            child.stderr.take().map(|pipe| Box::new(pipe) as _),
        ];
        for mut pipe in pipes.into_iter().flatten() {
            let sender = sender.clone();
            thread::spawn(move || {
                let mut text = String::new();
                let _ = pipe.read_to_string(&mut text);
                let _ = sender.send(text);
            });
        }
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        // The browser `open` starts may keep the pipes open, so their ends are not waited for long.
        let output: String = (0..2)
            .map_while(|_| receiver.recv_timeout(OUTPUT_GRACE).ok())
            .collect();
        match status {
            Some(status) => Reply {
                ok: status.success(),
                output,
            },
            None => failed(format!(
                "the command was stopped after {} seconds\n{output}",
                COMMAND_TIMEOUT.as_secs()
            )),
        }
    }
}

/// Where a replay runs and what it replays.
pub struct Replay<'a> {
    pub mode: Mode,
    pub url: &'a str,
    pub script_id: i64,
    /// The private run directory, with the saved session and the postMessage scripts.
    pub run_dir: &'a Path,
    /// The served directory the video and the screenshots go to.
    pub assets_dir: &'a Path,
    /// URL path `assets_dir` is served at.
    pub data_prefix: &'a str,
}

/// Why a replay stopped before its end.
#[derive(Debug)]
enum Stop {
    /// A check of the account or the stake, or the browser itself: the agent would stop too.
    Blocked(String),
    /// A step did not run or did not hold. That may be the script as much as the game, so the
    /// agent, not the replay, says which.
    Step(String),
    /// The game loaded another build than the one the script was written for.
    Version(String),
    /// Every step ran, but the balance did not follow the round: a verdict on the game, whose
    /// reason is on the step that checked the balance.
    Game,
}

/// Replays `script` and returns the JSON document of the test, which follows the schema
/// whatever happened.
pub fn replay(browser: &mut impl Browser, context: &Replay<'_>, script: &Script) -> Value {
    let started_at = Utc::now();
    let clock = Instant::now();
    let mut run = Run {
        browser,
        context,
        steps: Vec::new(),
        screenshots: 0,
        ran: 0,
        readings: Vec::new(),
        version: None,
        spun: false,
    };
    let stop = run.all(script).err();
    run.finish(script, stop.as_ref(), started_at, clock)
}

struct Run<'a, B: Browser> {
    browser: &'a mut B,
    context: &'a Replay<'a>,
    /// The steps of the report, as JSON.
    steps: Vec<Value>,
    screenshots: u32,
    /// How many steps of the script ran, the last of them maybe failing.
    ran: usize,
    /// What the read steps read, as the page wrote it and in cents.
    readings: Vec<(Reading, String, i64)>,
    /// The version of the game the replay saw, once it checked it.
    version: Option<GameVersion>,
    /// The spin was pressed, or tried: a round may have been played.
    spun: bool,
}

impl<B: Browser> Run<'_, B> {
    fn all(&mut self, script: &Script) -> Result<(), Stop> {
        let video = path_text(&self.context.assets_dir.join("session.webm"));
        self.builtin(
            "Opened the lobby",
            &strings(&["open", self.context.url, "--browser=chromium", "--headed"]),
        )?;
        self.builtin(
            "Resized the window to 1440×900",
            &strings(&["resize", "1440", "900"]),
        )?;
        self.builtin("Started the recording", &strings(&["video-start", &video]))?;
        let capture = run_file(self.context.run_dir, CAPTURE_FILE);
        self.builtin("Started capturing the postMessages", &capture)?;
        let session = path_text(&self.context.run_dir.join(SESSION_COPY));
        self.builtin(
            "Loaded the saved login of the account",
            &strings(&["state-load", &session]),
        )?;
        self.builtin("Reloaded the lobby logged in", &strings(&["reload"]))?;
        for step in &script.steps {
            self.step(script, step)?;
        }
        self.check_balance(Reading::BalanceBefore, Reading::BalanceAfter, "Betsson")?;
        self.check_balance(
            Reading::GameBalanceBefore,
            Reading::GameBalanceAfter,
            "game",
        )?;
        self.check_market()?;
        Ok(())
    }

    /// Runs a step of the script and records it.
    fn step(&mut self, script: &Script, step: &Step) -> Result<(), Stop> {
        if step.role == Role::Spin {
            self.check_version(&script.game_version)?;
            self.check_stake()?;
            self.spun = true;
        }
        let result = match step.role {
            Role::Action | Role::Spin => self.act(step),
            Role::Pause => {
                self.browser
                    .sleep(Duration::from_millis(step.timeout_ms.unwrap_or_default()));
                Ok(String::new())
            }
            Role::Expect | Role::Read => self.probe_until(step),
        };
        let (action, target) = match (step.commands.last(), step.role) {
            (Some(command), _) => name_and_target(command),
            (None, Role::Pause) => ("wait".to_owned(), None),
            (None, _) => ("expect".to_owned(), None),
        };
        if step.optional && result.is_err() {
            // What it handles did not show this time, which is no failure.
            let description = format!("{} (optional, not shown this time)", step.description);
            let mut entry = report_step(&action, &description, None);
            entry["status"] = json!("skipped");
            self.steps.push(entry);
            self.ran += 1;
            return Ok(());
        }
        let error = result.as_ref().err();
        let mut entry = report_step(&action, &step.description, error.map(String::as_str));
        if let Some(target) = target {
            entry["selector"] = json!(target);
        }
        if (step.screenshot || error.is_some())
            && let Some(shot) = self.screenshot(&step.description)
        {
            entry["screenshots"] = json!([shot]);
        }
        self.steps.push(entry);
        self.ran += 1;
        let value = result.map_err(Stop::Step)?;
        if let Some(name) = step.name {
            if !name.is_money() {
                self.readings.push((name, value, 0));
                return Ok(());
            }
            let Some(cents) = parse_cents(&value) else {
                let message = format!("{} {value:?} is not an amount of money", name.as_str());
                self.fail_last(&message);
                return Err(Stop::Step(message));
            };
            self.readings.push((name, value, cents));
            if name == Reading::BalanceBefore {
                self.check_account(cents)?;
            }
        }
        Ok(())
    }

    /// Runs the commands of a step, and returns what the last one printed.
    fn act(&mut self, step: &Step) -> Result<String, String> {
        let mut output = String::new();
        for command in &step.commands {
            output = self.command(command)?;
        }
        Ok(output)
    }

    /// Runs one command, and fails with what it printed when it does not succeed.
    fn command(&mut self, command: &[String]) -> Result<String, String> {
        let reply = self.browser.run(command);
        if reply.ok {
            Ok(reply.output)
        } else {
            Err(format!(
                "{} failed: {}",
                command.join(" "),
                quote(&reply.output)
            ))
        }
    }

    /// Probes until the text matches, within the timeout of the step, and returns the value it
    /// matched. The balance after the round is probed until it is the one the round should leave,
    /// since the page may show the old one for a while; when it never is, the last one read is
    /// returned, for the check of the balance to fail on.
    fn probe_until(&mut self, step: &Step) -> Result<String, String> {
        let pattern = step
            .matches
            .as_deref()
            .map(Regex::new)
            .transpose()
            .map_err(|error| error.to_string())?;
        let awaited = match step.name {
            Some(Reading::BalanceAfter) => self.expected_balance(Reading::BalanceBefore),
            Some(Reading::GameBalanceAfter) => self.expected_balance(Reading::GameBalanceBefore),
            _ => None,
        };
        let timeout = Duration::from_millis(step.timeout_ms.unwrap_or(PROBE_TIMEOUT_MS));
        let attempts = (timeout.as_millis() / POLL_INTERVAL.as_millis()).max(1);
        let started = Instant::now();
        let mut last = Err(String::new());
        for attempt in 0..attempts {
            if attempt > 0 {
                if started.elapsed() >= timeout {
                    break;
                }
                self.browser.sleep(POLL_INTERVAL);
            }
            last = self
                .probe(step)
                .and_then(|text| matched(pattern.as_ref(), &text));
            if last.as_ref().is_ok_and(|value| {
                awaited.is_none_or(|awaited| parse_cents(value) == Some(awaited))
            }) {
                break;
            }
        }
        last
    }

    /// The text a probe reads from its source.
    fn probe(&mut self, step: &Step) -> Result<String, String> {
        match step.source {
            Source::Page => self.act(step),
            Source::Messages => Ok(Value::Array(self.messages()).to_string()),
            Source::Response => {
                let filter = step.url_matches.clone().unwrap_or_default();
                let listed = self.command(&strings(&["requests", "--filter", &filter]))?;
                let (index, _) = last_request(&listed)
                    .ok_or_else(|| format!("no response matched {filter:?} yet"))?;
                self.command(&strings(&["response-body", &index.to_string()]))
            }
        }
    }

    /// Stops before the spin when the build.js the game loaded has another ETag than the
    /// script's. Its URL may change, such as from one host of the provider to another.
    fn check_version(&mut self, expected: &GameVersion) -> Result<(), Stop> {
        self.steps.push(report_step(
            "expect",
            "Checked that the game is the version the script was written for",
            None,
        ));
        let version = self.read_version().map_err(|message| {
            self.fail_last(&message);
            Stop::Step(message)
        })?;
        let changed = !report::same_version(&version.etag, &expected.etag);
        let message = format!(
            "The game loaded build.js with ETag {}, but the script was written for {}.",
            version.etag, expected.etag
        );
        self.version = Some(version);
        if changed {
            self.fail_last(&message);
            return Err(Stop::Version(message));
        }
        Ok(())
    }

    fn read_version(&mut self) -> Result<GameVersion, String> {
        let listed = self.command(&strings(&[
            "requests",
            "--static",
            "--filter",
            BUILD_FILTER,
        ]))?;
        let (index, url) = last_request(&listed)
            .ok_or("The game did not load a build.js, so its version is unknown.")?;
        let headers = self.command(&strings(&["response-headers", &index.to_string()]))?;
        let etag = etag(&headers).ok_or("build.js came without an ETag.")?;
        Ok(GameVersion {
            build_url: without_query(&url),
            etag,
        })
    }

    /// Stops when the account is not the one the mode needs.
    fn check_account(&mut self, cents: i64) -> Result<(), Stop> {
        let message = match self.context.mode {
            Mode::RealMoney if cents <= 0 => "The real-money account has no balance: add some.",
            Mode::NoBalance if cents > MAX_NO_BALANCE_CENTS => {
                "The no-balance account has balance, so this is the wrong account: no bet was tried."
            }
            _ => return Ok(()),
        };
        self.fail_last(message);
        Err(Stop::Blocked(message.to_owned()))
    }

    /// Stops before the spin when the stake is not above 0 and at most 1 € or, with real money,
    /// the balance.
    fn check_stake(&mut self) -> Result<(), Stop> {
        let stake = self.cents(Reading::Stake).unwrap_or_default();
        let balance = self.cents(Reading::BalanceBefore).unwrap_or_default();
        let message = if !(1..=MAX_STAKE_CENTS).contains(&stake) {
            format!(
                "The stake is {}, not above 0 and at most 1 €, so the spin was not pressed.",
                euros(stake)
            )
        } else if self.context.mode == Mode::RealMoney && stake > balance {
            format!(
                "The stake, {}, is above the balance, {}, so the spin was not pressed.",
                euros(stake),
                euros(balance)
            )
        } else {
            return Ok(());
        };
        self.steps.push(report_step(
            "expect",
            "Checked the stake before pressing spin",
            Some(&message),
        ));
        Err(Stop::Blocked(message))
    }

    /// The balance the round should leave from the reading `before`: with real money, less the
    /// stake, plus the win; without balance, the bet is refused, so the same.
    fn expected_balance(&self, before: Reading) -> Option<i64> {
        let before = self.cents(before)?;
        if self.context.mode == Mode::NoBalance {
            return Some(before);
        }
        let win = self.cents(Reading::Win).unwrap_or(0);
        Some(before - self.cents(Reading::Stake)? + win)
    }

    /// The balance after the round, of Betsson or of the game, must be the one it should leave.
    fn check_balance(&mut self, before: Reading, after: Reading, whose: &str) -> Result<(), Stop> {
        let (Some(expected), Some(read)) = (self.expected_balance(before), self.cents(after))
        else {
            return Ok(());
        };
        let start = euros(self.cents(before).unwrap_or_default());
        let description = match self.context.mode {
            Mode::RealMoney => format!(
                "Checked the {whose} balance: {start} − {} stake + {} win = {}",
                euros(self.cents(Reading::Stake).unwrap_or_default()),
                euros(self.cents(Reading::Win).unwrap_or_default()),
                euros(expected)
            ),
            Mode::NoBalance => {
                format!("Checked that the {whose} balance did not change from {start}")
            }
        };
        let message = (expected != read).then(|| {
            format!(
                "The {whose} balance after the round is {}, not the {} it should be.",
                euros(read),
                euros(expected)
            )
        });
        self.steps
            .push(report_step("expect", &description, message.as_deref()));
        if message.is_some() {
            return Err(Stop::Game);
        }
        Ok(())
    }

    /// The market is betsson.es: the currency shown must be the euro, and the message that the
    /// balance is not enough must be in Spanish.
    fn check_market(&mut self) -> Result<(), Stop> {
        let currency = self
            .reading(Reading::Currency)
            .map(|(text, _)| text.to_owned());
        let language = self
            .reading(Reading::Language)
            .map(|(text, _)| text.to_owned());
        let mut failed = false;
        if let Some(currency) = currency {
            let message =
                (!is_euro(&currency)).then(|| format!("The currency is {currency:?}, not EUR."));
            self.steps.push(report_step(
                "expect",
                "Checked that the currency is EUR",
                message.as_deref(),
            ));
            failed |= message.is_some();
        }
        if let Some(language) = language {
            let message = (!is_spanish(&language))
                .then(|| format!("The message is in {language:?}, not in Spanish."));
            self.steps.push(report_step(
                "expect",
                "Checked that the message is in Spanish",
                message.as_deref(),
            ));
            failed |= message.is_some();
        }
        if failed {
            return Err(Stop::Game);
        }
        Ok(())
    }

    /// A reading, as the page wrote it and in cents.
    fn reading(&self, name: Reading) -> Option<(&str, i64)> {
        self.readings
            .iter()
            .find(|(reading, _, _)| *reading == name)
            .map(|(_, text, cents)| (text.as_str(), *cents))
    }

    fn cents(&self, name: Reading) -> Option<i64> {
        self.reading(name).map(|(_, cents)| cents)
    }

    /// Runs a step of testson's own and records it, returning what it printed. When it fails,
    /// the browser itself is broken: the agent could not test either.
    fn builtin(&mut self, description: &str, command: &[String]) -> Result<String, Stop> {
        let result = self.command(command);
        let (name, _) = name_and_target(command);
        let error = result.as_ref().err().map(String::as_str);
        self.steps.push(report_step(&name, description, error));
        result.map_err(Stop::Blocked)
    }

    /// Marks the last recorded step as the one the replay stopped at.
    fn fail_last(&mut self, message: &str) {
        if let Some(step) = self.steps.last_mut() {
            step["status"] = json!("failed");
            step["error"] = json!({ "message": message });
        }
    }

    fn screenshot(&mut self, caption: &str) -> Option<Value> {
        self.screenshots += 1;
        let name = format!("{:02}.png", self.screenshots);
        let path = self.context.assets_dir.join("screenshots").join(&name);
        let filename = format!("--filename={}", path_text(&path));
        let reply = self.browser.run(&strings(&["screenshot", &filename]));
        reply.ok.then(|| {
            json!({
                "path": format!("{}/screenshots/{name}", self.context.data_prefix),
                "caption": caption,
                "taken_at": Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            })
        })
    }

    /// The postMessages received so far, as collect-messages.js returns them.
    fn messages(&mut self) -> Vec<Value> {
        let reply = self
            .browser
            .run(&run_file(self.context.run_dir, COLLECT_FILE));
        if reply.ok {
            parse_messages(&reply.output)
        } else {
            Vec::new()
        }
    }

    /// Collects the evidence, stops the recording, and builds the document of the test.
    fn finish(
        mut self,
        script: &Script,
        stop: Option<&Stop>,
        started_at: DateTime<Utc>,
        clock: Instant,
    ) -> Value {
        let screenshot = self.screenshot("What the page showed at the end");
        let console = self.browser.run(&strings(&["console", "warning"])).output;
        let requests = self.browser.run(&strings(&["requests"])).output;
        let user_agent = self
            .browser
            .run(&strings(&[RAW, "eval", "() => navigator.userAgent"]));
        let messages = self.messages();
        self.browser.run(&strings(&["video-stop"]));

        let mode = self.context.mode;
        let (status, outcome, summary) = verdict(mode, stop, self.spun);
        let mut steps = std::mem::take(&mut self.steps);
        steps.extend(
            script
                .steps
                .iter()
                .skip(self.ran)
                .map(|step| json!({ "description": step.description, "status": "skipped" })),
        );
        let mut evidence = report_step(
            "console",
            "Collected the console messages and the failed requests",
            None,
        );
        evidence["screenshots"] = json!(screenshot.into_iter().collect::<Vec<_>>());
        evidence["console_messages"] = json!(parse_console(&console));
        evidence["failed_requests"] = json!(parse_failed_requests(&requests));
        steps.push(evidence);
        let duration = u64::try_from(clock.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut document = json!({
            "schema_version": 3,
            "mode": mode.as_str(),
            "runner": "script",
            "script_id": self.context.script_id,
            "script_outcome": outcome,
            "status": status,
            "summary": summary,
            "game_url": self.context.url,
            "started_at": started_at.to_rfc3339_opts(SecondsFormat::Millis, true),
            "duration_ms": duration,
            "post_messages": messages,
            "steps": steps,
            "environment": { "browser": "chromium", "viewport": { "width": 1440, "height": 900 } },
        });
        let user_agent_text = user_agent.output.trim().trim_matches('"');
        if user_agent.ok && !user_agent_text.is_empty() {
            document["environment"]["user_agent"] = json!(user_agent_text);
        }
        match stop {
            // A failing game is a verdict, which has no error.
            Some(Stop::Blocked(message) | Stop::Step(message) | Stop::Version(message)) => {
                document["error"] = json!({ "message": message });
            }
            Some(Stop::Game) | None => {}
        }
        if let Some(version) = &self.version {
            document["game_version"] = json!(version);
        }
        for (key, name) in [
            ("balance_before", Reading::BalanceBefore),
            ("balance_after", Reading::BalanceAfter),
            ("game_balance_before", Reading::GameBalanceBefore),
            ("game_balance_after", Reading::GameBalanceAfter),
            ("currency", Reading::Currency),
            ("language", Reading::Language),
        ] {
            if let Some((text, _)) = self.reading(name).filter(|(text, _)| !text.is_empty()) {
                document[key] = json!(text.trim());
            }
        }
        if self.context.assets_dir.join("session.webm").is_file() {
            document["video_path"] = json!(format!("{}/session.webm", self.context.data_prefix));
        }
        document
    }
}

/// The status, the `script_outcome` and the summary of a replay that stopped at `stop`, or ran
/// to its end, having pressed the spin or not.
fn verdict(
    mode: Mode,
    stop: Option<&Stop>,
    spun: bool,
) -> (&'static str, &'static str, &'static str) {
    match stop {
        // The agent would play another round, so it waits for a later test.
        Some(Stop::Step(_)) if spun => (
            "test_failed",
            "failed_after_spin",
            "A step after the spin did not run or did not hold, so the round was not checked. \
             The agent tests the game at a later test, not now, so as not to play another round.",
        ),
        None if mode == Mode::RealMoney => (
            "working",
            "completed",
            "The replay played one real-money round at the minimum stake, and the balance \
             followed it.",
        ),
        None => (
            "working",
            "completed",
            "The replay tried to bet without balance, and the game said the balance is not \
             enough.",
        ),
        Some(Stop::Game) => (
            "failing",
            "completed",
            "The replay ran every step, but a check of the game did not hold: a balance did not \
             follow the round, or the currency or the language is not the expected one.",
        ),
        Some(Stop::Step(_)) => (
            "test_failed",
            "step_failed",
            "A step of the replay did not run or did not hold, so the agent tests the game \
             again.",
        ),
        Some(Stop::Version(_)) => (
            "test_failed",
            "version_changed",
            "The game has a new version, so the replay stopped before betting and the agent \
             tests it again.",
        ),
        Some(Stop::Blocked(_)) => (
            "test_failed",
            "blocked",
            "A check of the account, the stake or the browser stopped the replay before \
             betting, so it says nothing about the game.",
        ),
    }
}

/// The value `pattern` matched in `text`, at its last match, since sources such as the
/// postMessages list what they hold oldest first: its first group, or the whole match. Without a
/// pattern, the whole text, without the quotes `--raw` prints around a string.
fn matched(pattern: Option<&Regex>, text: &str) -> Result<String, String> {
    let Some(pattern) = pattern else {
        return Ok(text.trim().trim_matches('"').to_owned());
    };
    let found = pattern.captures_iter(text).last().ok_or_else(|| {
        format!(
            "the text did not match {:?}: {}",
            pattern.as_str(),
            quote(text)
        )
    })?;
    let value = found.get(1).or_else(|| found.get(0));
    Ok(value.map_or("", |value| value.as_str()).to_owned())
}

/// The file names of the postMessage scripts in a run directory.
pub const CAPTURE_FILE: &str = "capture-messages.js";
pub const COLLECT_FILE: &str = "collect-messages.js";

/// The `requests --filter` that finds the build.js of a game client.
const BUILD_FILTER: &str = r"/build\.js(\?|$)";

/// A step of the report, which failed when it has an error.
fn report_step(action: &str, description: &str, error: Option<&str>) -> Value {
    let mut step = json!({
        "action": action,
        "description": description,
        "status": if error.is_some() { "failed" } else { "passed" },
    });
    if let Some(message) = error {
        step["error"] = json!({ "message": message });
    }
    step
}

/// The arguments that run a script of the run directory with `run-code`, printing what it returns.
fn run_file(run_dir: &Path, name: &str) -> Vec<String> {
    vec![
        "--raw".to_owned(),
        "run-code".to_owned(),
        "--filename".to_owned(),
        path_text(&run_dir.join(name)),
    ]
}

fn strings(arguments: &[&str]) -> Vec<String> {
    arguments
        .iter()
        .map(|&argument| argument.to_owned())
        .collect()
}

fn path_text(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

fn euros(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    format!("{sign}{},{:02} €", cents.abs() / 100, cents.abs() % 100)
}

/// The command a list of arguments runs, past its options, and the target it acts on, if any.
/// Every command of a step runs, so the commands of a spin press exactly once: one `click`,
/// `press` or `mousedown`, which only moves of the pointer may come before, and only the
/// `mouseup` of that `mousedown` after. Anything else could bet twice or change the stake.
fn check_one_press(commands: &[Vec<String>]) -> Result<()> {
    let names: Vec<String> = commands
        .iter()
        .map(|command| name_and_target(command).0)
        .collect();
    let press = names
        .iter()
        .position(|name| matches!(name.as_str(), "click" | "press" | "mousedown"));
    let Some(press) = press else {
        bail!("the spin presses nothing");
    };
    ensure!(
        names[..press]
            .iter()
            .all(|name| matches!(name.as_str(), "mousemove" | "hover")),
        "the spin only moves the pointer before it presses"
    );
    let after: Vec<&str> = names[press + 1..].iter().map(String::as_str).collect();
    ensure!(
        after.is_empty() || (names[press] == "mousedown" && after == ["mouseup"]),
        "the spin presses exactly once, and only releases the mouse after it"
    );
    Ok(())
}

fn name_and_target(command: &[String]) -> (String, Option<String>) {
    let mut rest = command
        .iter()
        .skip_while(|argument| argument.starts_with("--"));
    let name = rest.next().cloned().unwrap_or_default();
    let target = match name.as_str() {
        "click" | "dblclick" | "fill" | "select" | "check" | "uncheck" | "hover" | "find"
        | "goto" => rest.next().cloned(),
        _ => None,
    };
    (name, target)
}

/// The end of what a command printed, where its error usually is.
fn quote(output: &str) -> String {
    let output = output.trim();
    match output.char_indices().rev().nth(QUOTED_OUTPUT - 1) {
        Some((start, _)) if start > 0 => format!("…{}", &output[start..]),
        _ => output.to_owned(),
    }
}

fn without_query(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).to_owned()
}

/// The number and the URL of the last request a `requests` listing shows.
fn last_request(listing: &str) -> Option<(u32, String)> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*(\d+)\.\s+\[[A-Z]+\]\s+(\S+)").expect("request line pattern")
    });
    LINE.captures_iter(listing)
        .filter_map(|found| Some((found[1].parse().ok()?, found[2].to_owned())))
        .last()
}

/// The ETag in a `response-headers` listing.
fn etag(headers: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (name, value) = line.trim().split_once(':')?;
        let value = value.trim();
        (name.trim().eq_ignore_ascii_case("etag") && !value.is_empty()).then(|| value.to_owned())
    })
}

/// The warnings and errors of a `console warning` listing, as the schema has them.
fn parse_console(listing: &str) -> Vec<Value> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\[(ERROR|WARNING)\]\s(.*?)(?:\s@\s(\S+?)(?::\d+)?)?\s*$")
            .expect("console line pattern")
    });
    LINE.captures_iter(listing)
        .take(200)
        .map(|found| {
            let mut message = json!({
                "level": if &found[1] == "ERROR" { "error" } else { "warning" },
                "text": &found[2],
            });
            if let Some(source) = found
                .get(3)
                .filter(|source| url::Url::parse(source.as_str()).is_ok())
            {
                message["source_url"] = json!(without_query(source.as_str()));
            }
            message
        })
        .collect()
}

/// The failed requests of a `requests` listing: HTTP 400 to 599 or a network failure.
fn parse_failed_requests(listing: &str) -> Vec<Value> {
    static LINE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?m)^\s*\d+\.\s+\[([A-Z]+)\]\s+(\S+)\s+=>\s+\[(\d{3}|FAILED)\]\s*(.*)$")
            .expect("failed request pattern")
    });
    LINE.captures_iter(listing)
        .filter_map(|found| {
            let url = without_query(&found[2]);
            url::Url::parse(&url).ok()?;
            let mut request = json!({ "url": url, "method": &found[1] });
            if &found[3] == "FAILED" {
                let failure = found[4].trim();
                request["failure"] = json!(if failure.is_empty() {
                    "failed"
                } else {
                    failure
                });
            } else {
                let code: u16 = found[3].parse().ok()?;
                if !(400..=599).contains(&code) {
                    return None;
                }
                request["status_code"] = json!(code);
            }
            Some(request)
        })
        .take(200)
        .collect()
}

/// The messages collect-messages.js returned, which `--raw` prints as JSON, maybe as a string.
fn parse_messages(output: &str) -> Vec<Value> {
    let mut value: Value = serde_json::from_str(output.trim()).unwrap_or(Value::Null);
    if let Value::String(text) = &value {
        value = serde_json::from_str(text).unwrap_or(Value::Null);
    }
    let Value::Array(messages) = value else {
        return Vec::new();
    };
    messages
        .into_iter()
        .filter(|message| report::check_post_message(message).is_ok())
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script() -> Value {
        json!({
            "script_version": 1,
            "mode": "real_money",
            "game_version": { "build_url": "https://x.pragmaticplay.net/gs2c/build.js", "etag": "\"abc\"" },
            "steps": [
                { "role": "read", "name": "balance_before", "description": "Read the balance",
                  "commands": [["--raw", "eval", "() => document.querySelector('.balance').textContent"]] },
                { "role": "action", "description": "Open the game",
                  "commands": [["click", "getByRole('button', { name: 'Jugar' })"]] },
                { "role": "expect", "description": "The game is ready",
                  "commands": [["find", "Girar"]], "matches": "Girar" },
                { "role": "read", "name": "game_balance_before", "description": "Read the game balance",
                  "commands": [["--raw", "eval", "() => document.querySelector('.game-balance').textContent"]] },
                { "role": "read", "name": "stake", "description": "Read the stake",
                  "commands": [["--raw", "eval", "() => document.querySelector('.stake').textContent"]] },
                { "role": "spin", "description": "Press spin",
                  "commands": [["mousemove", "1200", "780"], ["mousedown"], ["mouseup"]] },
                { "role": "read", "name": "balance_after", "description": "Read the balance after",
                  "source": "response", "url_matches": "gameService", "matches": "balance=([\\d.]+)" },
                { "role": "read", "name": "game_balance_after", "description": "Read the game balance after",
                  "source": "response", "url_matches": "gameService", "matches": "balance=([\\d.]+)" },
                { "role": "read", "name": "currency", "description": "Read the currency",
                  "commands": [["--raw", "eval", "() => document.querySelector('.currency').textContent"]] }
            ]
        })
    }

    fn parsed(value: &Value) -> Result<Script> {
        parse(&value.to_string(), Mode::RealMoney)
    }

    #[test]
    fn accepts_a_script_that_follows_the_rules() {
        parsed(&script()).unwrap();
    }

    #[test]
    fn rejects_scripts_that_break_the_rules() {
        let spin = json!({ "role": "spin", "description": "Again", "commands": [["mousedown"]] });
        for (pointer, value) in [
            ("/script_version", json!(2)),
            ("/mode", json!("no_balance")),
            ("/game_version/etag", json!("")),
            (
                "/steps/1/commands",
                json!([["run-code", "async page => {}"]]),
            ),
            (
                "/steps/1/commands",
                json!([["goto", "https://evil.example.com"]]),
            ),
            ("/steps/1/commands", json!([["click", "text=Depositar"]])),
            (
                "/steps/1/commands",
                json!([["click", "getByText('Modo demo')"]]),
            ),
            ("/steps/1/commands", json!([["fill", "#loss-limit", "11"]])),
            ("/steps/1/commands", json!([["press", "Space"]])),
            ("/steps/1/commands", json!([["keydown", "Space"]])),
            ("/steps/1/commands", json!([["--json", "click", "#x"]])),
            ("/steps/6/name", json!("win")),
            (
                "/steps/9",
                json!({ "role": "read", "name": "win", "description": "Read the win late",
                        "commands": [["find", "Ganancia"]], "matches": "([\\d,]+)" }),
            ),
            ("/steps/1/commands", json!([["--session", "click", "#x"]])),
            ("/steps/1/commands", json!([["click", "#x", "--force"]])),
            ("/steps/1/commands", json!([])),
            (
                "/steps/4/commands",
                json!([["eval", "() => document.querySelector('#spin').click()"]]),
            ),
            (
                "/steps/4/commands",
                json!([["eval", "() => (window.x = 1)"]]),
            ),
            ("/steps/4/commands", json!([["click", "#stake"]])),
            ("/steps/2/matches", json!("(")),
            ("/steps/2/timeout_ms", json!(500_000)),
            ("/steps/6/url_matches", Value::Null),
            ("/steps/6/name", json!("stake")),
            ("/steps/0/name", json!("win")),
            ("/steps/9", spin.clone()),
            (
                "/steps/9",
                json!({ "role": "action", "description": "Close", "commands": [["click", "#x"]] }),
            ),
            (
                "/steps/5",
                json!({ "role": "pause", "description": "Wait", "timeout_ms": 10 }),
            ),
            // Both balances are read around the spin, and the currency always.
            ("/steps/3/name", json!("win")),
            ("/steps/7/name", json!("win")),
            ("/steps/8/name", json!("win")),
        ] {
            let mut broken = script();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            let target = broken.pointer_mut(parent).unwrap();
            match (target, value.is_null()) {
                (Value::Object(object), true) => {
                    object.remove(key);
                }
                (Value::Object(object), false) => {
                    object.insert(key.to_owned(), value.clone());
                }
                // An index past the end adds the step, and one within it replaces that step.
                (Value::Array(array), _) => match key.parse::<usize>().unwrap() {
                    index if index < array.len() => array[index] = value.clone(),
                    _ => array.push(value.clone()),
                },
                _ => unreachable!(),
            }
            assert!(parsed(&broken).is_err(), "{pointer} = {value} was accepted");
        }
        // Reading the balance first is required, so a script that acts first is refused.
        let mut late = script();
        late["steps"].as_array_mut().unwrap().swap(0, 1);
        assert!(parsed(&late).is_err());
    }

    #[test]
    fn rejects_a_spin_that_could_bet_twice_or_change_the_stake() {
        // A spin presses once, and does nothing else that could bet or change the stake.
        for commands in [
            json!([["click", "#spin"], ["click", "#spin"]]),
            json!([["click", "#bet-plus"], ["click", "#spin"]]),
            json!([["mousedown"], ["mouseup"], ["mousedown"]]),
            json!([["mousewheel", "0", "100"], ["mousedown"], ["mouseup"]]),
            json!([["mousemove", "1200", "780"]]),
        ] {
            let mut broken = script();
            broken["steps"][5]["commands"] = commands.clone();
            assert!(parsed(&broken).is_err(), "{commands} was accepted");
        }
        // The stake checked before the spin is the one bet, so nothing acts in between.
        let mut raised = script();
        raised["steps"].as_array_mut().unwrap().insert(
            5,
            json!({ "role": "action", "description": "Raise the stake",
                    "commands": [["click", "#bet-plus"]] }),
        );
        assert!(parsed(&raised).is_err());
        // Only an action may be skipped.
        for step in [2, 5] {
            let mut broken = script();
            broken["steps"][step]["optional"] = json!(true);
            assert!(
                parsed(&broken).is_err(),
                "an optional step {step} was accepted"
            );
        }
    }

    #[test]
    fn a_spin_may_be_one_click_and_an_action_optional() {
        let mut simple = script();
        simple["steps"][5]["commands"] = json!([["click", "#spin"]]);
        simple["steps"][1]["optional"] = json!(true);
        parsed(&simple).unwrap();
    }

    #[test]
    fn a_no_balance_script_may_end_in_the_lobby() {
        let lobby = json!({
            "script_version": 1,
            "mode": "no_balance",
            "game_version": { "build_url": "https://x/build.js", "etag": "W/\"1\"" },
            "steps": [
                { "role": "read", "name": "balance_before", "description": "Read the balance",
                  "commands": [["find", "Saldo"]], "matches": "Saldo ([\\d,]+ €)" },
                { "role": "action", "description": "Open the game", "commands": [["click", ".game"]] },
                { "role": "read", "name": "currency", "description": "Read the currency",
                  "commands": [["find", "Saldo"]], "matches": "Saldo [\\d,]+ (€)" },
                { "role": "read", "name": "language", "description": "Read the message",
                  "commands": [["find", "Saldo insuficiente"]], "matches": "(Saldo insuficiente)" },
                { "role": "expect", "description": "The lobby refuses",
                  "commands": [["find", "Saldo insuficiente"]], "matches": "Saldo insuficiente" }
            ]
        });
        parse(&lobby.to_string(), Mode::NoBalance).unwrap();
        let mut silent = lobby.clone();
        silent["steps"].as_array_mut().unwrap().pop();
        assert!(parse(&silent.to_string(), Mode::NoBalance).is_err());
        // The language of the message is read too.
        let mut unread = lobby.clone();
        unread["steps"].as_array_mut().unwrap().remove(3);
        assert!(parse(&unread.to_string(), Mode::NoBalance).is_err());
    }

    #[test]
    fn reads_money_as_pages_write_it() {
        for (text, cents) in [
            ("12,40 €", 1240),
            ("€0.20", 20),
            ("\"1.234,56 €\"", 123_456),
            ("1,234.56", 123_456),
            ("1.000 €", 100_000),
            ("Saldo: 0 €", 0),
            ("5", 500),
            ("0,1", 10),
            ("-3,00", -300),
        ] {
            assert_eq!(parse_cents(text), Some(cents), "{text}");
        }
        assert_eq!(parse_cents("no money"), None);
    }

    /// A browser that answers each command from `answer`, and keeps the commands it ran.
    struct Fake<F: FnMut(&str) -> Option<String>> {
        answer: F,
        ran: Vec<String>,
    }

    impl<F: FnMut(&str) -> Option<String>> Browser for Fake<F> {
        fn run(&mut self, arguments: &[String]) -> Reply {
            let command = arguments.join(" ");
            self.ran.push(command.clone());
            match (self.answer)(&command) {
                Some(output) => Reply { ok: true, output },
                None => Reply {
                    ok: false,
                    output: "Error: not found".to_owned(),
                },
            }
        }

        fn sleep(&mut self, _: Duration) {}
    }

    /// The answers of a game that plays a round of 0,20 € and wins nothing.
    fn game(command: &str) -> String {
        let answer = match command {
            c if c.contains("collect-messages.js") => {
                r#"[{"at":"2026-10-15T10:00:00.000Z","direction":"to_lobby","origin":"https://x","data":{"event":"loaded"}}]"#
            }
            c if c.contains(".balance") || c.contains(".game-balance") => "\"12,40 €\"",
            c if c.contains(".currency") => "\"€\"",
            c if c.contains(".stake") => "\"0,20 €\"",
            c if c.starts_with("requests --static") => {
                "1. [GET] https://x.pragmaticplay.net/gs2c/build.js?v=9 => [200] OK"
            }
            c if c.starts_with("response-headers 1") => {
                "content-type: text/javascript\netag: \"abc\""
            }
            c if c.starts_with("requests --filter") => {
                "7. [POST] https://x/gs2c/gameService => [200] OK"
            }
            c if c.starts_with("response-body 7") => "tw=0.00&balance=12.20&stime=1",
            "requests" => "3. [GET] https://x/api?token=1 => [500] Internal Server Error",
            c if c.starts_with("console") => "Total messages: 1\n\n[ERROR] boom @ https://x/a.js:3",
            c if c.starts_with("find Girar") => "- button \"Girar\"",
            _ => "",
        };
        answer.to_owned()
    }

    fn replay_with(
        answer: impl FnMut(&str) -> Option<String>,
        script: &Value,
    ) -> (Value, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let mode = if script["mode"] == "no_balance" {
            Mode::NoBalance
        } else {
            Mode::RealMoney
        };
        let context = Replay {
            mode,
            url: "https://www.betsson.es/casino",
            script_id: 3,
            run_dir: dir.path(),
            assets_dir: dir.path(),
            data_prefix: "/data/assets/x",
        };
        let mut browser = Fake {
            answer,
            ran: Vec::new(),
        };
        let script = parse(&script.to_string(), mode).unwrap();
        let document = replay(&mut browser, &context, &script);
        report::validate(&document).unwrap();
        (document, browser.ran)
    }

    fn spun(ran: &[String]) -> bool {
        ran.iter().any(|command| command == "mousedown")
    }

    #[test]
    fn a_round_whose_balance_follows_works() {
        let (document, ran) = replay_with(|command| Some(game(command)), &script());
        assert_eq!(document["status"], "working", "{document:#}");
        assert_eq!(document["runner"], "script");
        assert_eq!(document["script_id"], 3);
        assert_eq!(document["script_outcome"], "completed");
        assert_eq!(document["balance_before"], "12,40 €");
        assert_eq!(document["game_version"]["etag"], "\"abc\"");
        assert_eq!(
            document["game_version"]["build_url"],
            "https://x.pragmaticplay.net/gs2c/build.js"
        );
        assert_eq!(document["post_messages"][0]["data"]["event"], "loaded");
        let evidence = document["steps"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(evidence["console_messages"][0]["text"], "boom");
        assert_eq!(evidence["failed_requests"][0]["status_code"], 500);
        assert_eq!(evidence["failed_requests"][0]["url"], "https://x/api");
        assert_eq!(
            ran.iter().filter(|command| *command == "mousedown").count(),
            1
        );
    }

    #[test]
    fn a_balance_that_does_not_follow_is_failing() {
        let (document, _) = replay_with(
            |command| {
                if command.starts_with("response-body") {
                    Some("balance=12.00".to_owned())
                } else {
                    Some(game(command))
                }
            },
            &script(),
        );
        assert_eq!(document["status"], "failing");
        assert_eq!(document["script_outcome"], "completed");
        assert!(document.get("error").is_none());
    }

    #[test]
    fn the_game_balance_and_the_currency_are_checked_too() {
        // The game shows another balance than Betsson's after the round.
        let mut game_only = script();
        game_only["steps"][7]["matches"] = json!("stime=([\\d.]+)");
        let (document, _) = replay_with(|command| Some(game(command)), &game_only);
        assert_eq!(document["status"], "failing", "{document:#}");
        assert_eq!(document["game_balance_after"], "1");
        // The game is played in another currency.
        let (document, _) = replay_with(
            |command| {
                if command.contains(".currency") {
                    Some("\"PEN\"".to_owned())
                } else {
                    Some(game(command))
                }
            },
            &script(),
        );
        assert_eq!(document["status"], "failing", "{document:#}");
        assert_eq!(document["currency"], "PEN");
    }

    #[test]
    fn a_refused_bet_leaves_both_balances_and_speaks_spanish() {
        let refused = json!({
            "script_version": 1,
            "mode": "no_balance",
            "game_version": { "build_url": "https://x.pragmaticplay.net/gs2c/build.js", "etag": "\"abc\"" },
            "steps": [
                { "role": "read", "name": "balance_before", "description": "Read the balance",
                  "commands": [["--raw", "eval", "() => document.querySelector('.empty').textContent"]] },
                { "role": "action", "description": "Open the game", "commands": [["click", ".game"]] },
                { "role": "read", "name": "game_balance_before", "description": "Read the game balance",
                  "commands": [["--raw", "eval", "() => document.querySelector('.empty').textContent"]] },
                { "role": "read", "name": "stake", "description": "Read the stake",
                  "commands": [["--raw", "eval", "() => document.querySelector('.stake').textContent"]] },
                { "role": "spin", "description": "Press spin", "commands": [["click", "#spin"]] },
                { "role": "read", "name": "balance_after", "description": "Read the balance after",
                  "commands": [["--raw", "eval", "() => document.querySelector('.empty').textContent"]] },
                { "role": "read", "name": "game_balance_after", "description": "Read the game balance after",
                  "commands": [["--raw", "eval", "() => document.querySelector('.empty').textContent"]] },
                { "role": "read", "name": "currency", "description": "Read the currency",
                  "commands": [["--raw", "eval", "() => document.querySelector('.currency').textContent"]] },
                { "role": "read", "name": "language", "description": "Read the language of the message",
                  "source": "messages", "matches": "\"lang\":\"(\\w+)\",\"name\":\"balanceTooLow\"" },
                { "role": "expect", "description": "The game says the balance is not enough",
                  "source": "messages", "matches": "balanceTooLow" }
            ]
        });
        let answer = |language: &'static str| {
            move |command: &str| {
                Some(match command {
                    c if c.contains(".empty") => "\"0,00 €\"".to_owned(),
                    c if c.contains("collect-messages.js") => format!(
                        r#"[{{"at":"2026-10-15T10:00:00Z","direction":"to_lobby","data":{{"lang":"{language}","name":"balanceTooLow"}}}}]"#
                    ),
                    c => game(c),
                })
            }
        };
        let (document, _) = replay_with(answer("ES"), &refused);
        assert_eq!(document["status"], "working", "{document:#}");
        assert_eq!(document["language"], "ES");
        assert_eq!(document["game_balance_after"], "0,00 €");
        let (document, _) = replay_with(answer("EN"), &refused);
        assert_eq!(document["status"], "failing", "{document:#}");
    }

    #[test]
    fn knows_the_euro_and_spanish() {
        for euro in ["€", "EUR", "eur", "12,40 €"] {
            assert!(is_euro(euro), "{euro}");
        }
        assert!(!is_euro("PEN") && !is_euro("S/ 12,40"));
        for spanish in [
            "ES",
            "es-ES",
            "\"es\"",
            "Fondos insuficientes para realizar esta apuesta",
        ] {
            assert!(is_spanish(spanish), "{spanish}");
        }
        assert!(!is_spanish("EN") && !is_spanish("Insufficient funds"));
    }

    #[test]
    fn a_new_version_stops_before_the_spin() {
        let (document, ran) = replay_with(
            |command| {
                if command.starts_with("response-headers") {
                    Some("etag: \"def\"".to_owned())
                } else {
                    Some(game(command))
                }
            },
            &script(),
        );
        assert_eq!(document["status"], "test_failed");
        assert_eq!(document["script_outcome"], "version_changed");
        assert_eq!(document["game_version"]["etag"], "\"def\"");
        assert!(!spun(&ran));
        let skipped = document["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|step| step["status"] == "skipped")
            .count();
        // The spin and the three readings after it.
        assert_eq!(skipped, 4);
    }

    #[test]
    fn a_stake_above_one_euro_is_never_bet() {
        let (document, ran) = replay_with(
            |command| {
                if command.contains(".stake") {
                    Some("\"1,50 €\"".to_owned())
                } else {
                    Some(game(command))
                }
            },
            &script(),
        );
        assert_eq!(document["script_outcome"], "blocked");
        assert!(!spun(&ran));
    }

    #[test]
    fn a_broken_browser_or_the_wrong_balance_blocks_the_replay() {
        for broken in ["state-load", ".balance"] {
            let (document, ran) = replay_with(
                |command| {
                    if command.contains(broken) {
                        (broken == ".balance").then(|| "\"0,00 €\"".to_owned())
                    } else {
                        Some(game(command))
                    }
                },
                &script(),
            );
            assert_eq!(document["script_outcome"], "blocked", "{broken}");
            assert!(!ran.iter().any(|command| command.starts_with("click")));
        }
    }

    #[test]
    fn the_version_is_the_etag_wherever_build_js_comes_from() {
        let (document, ran) = replay_with(
            |command| {
                if command.starts_with("requests --static") {
                    Some(
                        "4. [GET] https://y.pragmaticplay.net/gs2c/build.js => [304] Not Modified"
                            .to_owned(),
                    )
                } else if command.starts_with("response-headers 4") {
                    Some("etag: \"abc\"".to_owned())
                } else {
                    Some(game(command))
                }
            },
            &script(),
        );
        assert_eq!(document["status"], "working", "{document:#}");
        assert!(spun(&ran));
    }

    #[test]
    fn the_balance_after_the_round_is_awaited() {
        let mut bodies = ["balance=12.40", "balance=12.40", "balance=12.20"].into_iter();
        let (document, _) = replay_with(
            |command| {
                if command.starts_with("response-body") {
                    Some(bodies.next().unwrap_or("balance=12.20").to_owned())
                } else {
                    Some(game(command))
                }
            },
            &script(),
        );
        assert_eq!(document["status"], "working", "{document:#}");
        assert_eq!(document["balance_after"], "12.20");
    }

    #[test]
    fn a_probe_reads_the_latest_message() {
        let mut messages = script();
        messages["steps"][6] = json!({
            "role": "read", "name": "balance_after", "description": "Read the balance after",
            "source": "messages", "matches": "\"balance\":([\\d.]+)"
        });
        let (document, _) = replay_with(
            |command| {
                if command.contains("collect-messages.js") {
                    Some(
                        r#"[{"at":"2026-10-15T10:00:00Z","direction":"to_lobby","data":{"balance":12.4}},
                           {"at":"2026-10-15T10:00:05Z","direction":"to_lobby","data":{"balance":12.2}}]"#
                            .to_owned(),
                    )
                } else {
                    Some(game(command))
                }
            },
            &messages,
        );
        assert_eq!(document["status"], "working", "{document:#}");
        assert_eq!(document["post_messages"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn a_check_that_fails_after_the_spin_waits_for_the_next_test() {
        let (document, ran) = replay_with(
            |command| (!command.starts_with("response-body")).then(|| game(command)),
            &script(),
        );
        assert!(spun(&ran));
        assert_eq!(document["status"], "test_failed");
        assert_eq!(document["script_outcome"], "failed_after_spin");
    }

    #[test]
    fn an_optional_step_with_nothing_to_act_on_is_skipped() {
        let mut with_form = script();
        with_form["steps"].as_array_mut().unwrap().insert(
            2,
            json!({ "role": "action", "description": "Fill the gaming-session form",
                    "optional": true,
                    "commands": [["click", "getByRole('button', { name: '15 Min' })"]] }),
        );
        let (document, ran) = replay_with(
            |command| (!command.contains("15 Min")).then(|| game(command)),
            &with_form,
        );
        assert_eq!(document["status"], "working", "{document:#}");
        assert_eq!(document["steps"][8]["status"], "skipped");
        assert!(spun(&ran));
        // The same step, when every visit needs it, breaks the script.
        with_form["steps"][2]["optional"] = json!(false);
        let (document, ran) = replay_with(
            |command| (!command.contains("15 Min")).then(|| game(command)),
            &with_form,
        );
        assert_eq!(document["script_outcome"], "step_failed");
        assert!(!spun(&ran));
    }

    #[test]
    fn a_step_that_no_longer_runs_breaks_the_script() {
        let (document, ran) = replay_with(
            |command| (!command.starts_with("click")).then(|| game(command)),
            &script(),
        );
        assert_eq!(document["status"], "test_failed");
        assert_eq!(document["script_outcome"], "step_failed");
        assert!(!spun(&ran));
    }
}
