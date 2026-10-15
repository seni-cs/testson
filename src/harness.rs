//! Coding-agent CLIs that can drive the test, and the model each one runs.

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::events;

const PING_PROMPT: &str =
    "Reply with exactly the word PONG and nothing else. Do not use any tools.";
const PING_TIMEOUT: Duration = Duration::from_secs(120);
const PING_OUTPUT_LINES: usize = 15;
/// Where Codex writes its final message, in the directory it runs in.
const LAST_MESSAGE_FILE: &str = "last-message.txt";

/// How long a run may take unless `--timeout` says otherwise.
pub const DEFAULT_TIMEOUT: &str = "20m";
/// How often a supervised process is checked on.
const POLL: Duration = Duration::from_millis(200);
/// How long output is still read after the process exits. Whatever it left behind that keeps
/// its stdout or stderr open is not waited for beyond this.
const DRAIN: Duration = Duration::from_secs(2);
/// How long a process tree has to exit after it is asked to, before it is killed.
#[cfg(unix)]
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Starts a process outside the console's Ctrl+C group, as a process group puts it on Unix.
#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Set when the operator interrupts testson.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);
/// How many [`DeferInterrupts`] are held.
static DEFERRING: AtomicUsize = AtomicUsize::new(0);

/// The built-in Claude tools a test can see: the shell, and reads of its own directories.
const CLAUDE_TOOLS: &str = "Bash,Read";

/// The shell commands a Claude test may run: the browser, progress lines, timestamps, and
/// listings. In `dontAsk` mode every other call that needs approval is denied. Nothing may copy
/// or move files: `playwright-cli` writes the evidence where it belongs, and a copy could put the
/// trace or a script that holds the session among the served assets.
///
/// This is not a sandbox: `playwright-cli run-code` runs arbitrary JavaScript in the Node.js
/// process of the Playwright server. It keeps the agent off its own web and file tools, and the
/// README says what actually contains a test.
const CLAUDE_ALLOWED: [&str; 4] = [
    "Bash(playwright-cli:*)",
    "Bash(echo:*)",
    "Bash(date:*)",
    "Bash(ls:*)",
];

/// Tools a Claude test is refused even when the operator's own settings allow them.
const CLAUDE_DENIED: [&str; 8] = [
    "WebFetch",
    "WebSearch",
    "Edit",
    "Write",
    "NotebookEdit",
    "PowerShell",
    "Agent",
    "mcp__*",
];

/// An agent CLI that runs a prompt non-interactively.
#[allow(
    clippy::doc_markdown,
    reason = "the variant docs are shown as plain text in --help"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Harness {
    /// Anthropic Claude Code (`claude`).
    Claude,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// xAI Grok CLI (`grok`).
    Grok,
}

impl Harness {
    #[must_use]
    pub fn binary(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Grok => "grok",
        }
    }

    fn default_model(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("sonnet"),
            Self::Codex | Self::Grok => None,
        }
    }

    fn default_effort(self) -> Option<&'static str> {
        match self {
            Self::Claude => Some("medium"),
            Self::Codex | Self::Grok => None,
        }
    }

    /// Codex writes its final message to a file. The others print it on stdout.
    fn writes_last_message(self) -> bool {
        matches!(self, Self::Codex)
    }

    /// Claude and Codex read the prompt from stdin, which keeps it out of process listings. The
    /// Grok CLI is only known to take it as the `--single` argument.
    fn reads_prompt_from_stdin(self) -> bool {
        matches!(self, Self::Claude | Self::Codex)
    }

    /// Claude and Grok print only the final answer unless asked for an event stream.
    fn streams_events(self) -> bool {
        matches!(self, Self::Claude | Self::Grok)
    }

    fn install_hint(self) -> &'static str {
        match self {
            Self::Claude => "npm install -g @anthropic-ai/claude-code",
            Self::Codex => "npm install -g @openai/codex",
            Self::Grok => "the Grok CLI installer, then add ~/.grok/bin to PATH",
        }
    }

    fn login_hint(self) -> &'static str {
        match self {
            Self::Claude => "run `claude`, then `/login`",
            Self::Codex => "run `codex login`",
            Self::Grok => "run `grok login`",
        }
    }
}

impl fmt::Display for Harness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.binary())
    }
}

/// A harness plus the model and reasoning effort it is asked to use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub harness: Harness,
    /// `None` leaves the choice to the CLI's own configuration.
    pub model: Option<String>,
    pub effort: Option<String>,
}

/// What an agent run left behind.
pub struct Transcript {
    /// Exit status. A signal is reported as `128 + signal` on Unix.
    pub exit_code: i32,
    /// The final answer: the `result` event of a stream, or the file Codex writes it to.
    pub last_message: Option<String>,
    /// The harness marked the run as failed, so `last_message` describes an error, not an answer.
    pub ended_in_error: bool,
    /// The run outlived its timeout and the agent was stopped.
    pub timed_out: bool,
    /// The operator interrupted testson and the agent was stopped.
    pub interrupted: bool,
}

impl Transcript {
    /// The transcript of a run that ended on its own and whose final answer is the `result`
    /// event a [`Relay`] kept.
    #[must_use]
    pub fn from_stream(exit_code: i32, result: Option<(String, bool)>) -> Self {
        let (last_message, ended_in_error) =
            result.map_or((None, false), |(text, is_error)| (Some(text), is_error));
        Self {
            exit_code,
            last_message,
            ended_in_error,
            timed_out: false,
            interrupted: false,
        }
    }
}

impl Agent {
    /// Applies the harness defaults to whatever the operator left unset.
    #[must_use]
    pub fn new(harness: Harness, model: Option<String>, effort: Option<String>) -> Self {
        Self {
            harness,
            model: model.or_else(|| harness.default_model().map(str::to_owned)),
            effort: effort.or_else(|| harness.default_effort().map(str::to_owned)),
        }
    }

    #[must_use]
    pub fn model_label(&self) -> &str {
        self.model.as_deref().unwrap_or("CLI default")
    }

    /// Fails with install instructions when the harness CLI is not on `PATH`.
    ///
    /// # Errors
    ///
    /// Fails when the binary is missing.
    pub fn ensure_installed(&self) -> Result<()> {
        if !on_path(self.harness.binary()) {
            bail!(
                "{} is not on PATH.\nInstall it with: {}",
                self.harness.binary(),
                self.harness.install_hint()
            );
        }
        Ok(())
    }

    /// Sends a one-word prompt and fails unless the model answers it.
    ///
    /// This catches a logged-out CLI or a model the account cannot use before a full test starts.
    ///
    /// # Errors
    ///
    /// Fails when the CLI cannot start, exits non-zero, times out, or does not answer `PONG`.
    pub fn ping(&self) -> Result<()> {
        let scratch = tempfile::tempdir().context("could not create a temporary directory")?;
        let last_message = last_message_file(scratch.path())?;
        let command = self.command(PING_PROMPT, scratch.path(), None)?;
        let program = PathBuf::from(command.get_program());
        let (mut stdout, mut stderr) = (String::new(), String::new());
        let deferred = defer_interrupts();
        let ended = supervise(
            command,
            self.stdin_prompt(PING_PROMPT),
            PING_TIMEOUT,
            true,
            |from_stdout, line| {
                let output = if from_stdout {
                    &mut stdout
                } else {
                    &mut stderr
                };
                output.push_str(&line);
                output.push('\n');
                Ok(())
            },
        )
        .map_err(|error| self.failed(&program, "failed to start", error));
        drop(deferred);
        exit_if_interrupted();
        let ended = ended?;
        let answer = if self.harness.writes_last_message() {
            fs::read_to_string(&last_message).unwrap_or_default()
        } else {
            stdout.clone()
        };
        let problem = if ended.timed_out {
            format!("did not answer within {}s", PING_TIMEOUT.as_secs())
        } else if !ended.status.success() {
            format!("exited with status {}", exit_code(ended.status))
        } else if !answer.to_uppercase().contains("PONG") {
            "answered, but not with the expected reply".to_owned()
        } else {
            return Ok(());
        };
        bail!(
            "{harness} {problem} when pinged with model {model}.\n\
             Make sure you are logged in to the {harness} CLI with an account that can use that \
             model ({hint}), then try again.\n{output}",
            harness = self.harness,
            model = self.model_label(),
            hint = self.harness.login_hint(),
            output = tail(&format!("{stdout}\n{stderr}"), PING_OUTPUT_LINES),
        );
    }

    /// Runs `prompt` in `dir` with the tools a test needs, and `assets` as its evidence directory,
    /// for at most `timeout`.
    ///
    /// `assets` must be absolute, since the agent runs in `dir`. `Agent::command` says what each
    /// harness is restricted to. Combined stdout and stderr are streamed to the terminal and to
    /// `log`. When the timeout passes, or the operator interrupts testson, the agent and every
    /// process it started are stopped. Interrupts are deferred while it runs; see
    /// [`handle_interrupts`].
    ///
    /// # Errors
    ///
    /// Fails when the CLI cannot start or the log cannot be written.
    pub fn run(
        &self,
        prompt: &str,
        dir: &Path,
        assets: &Path,
        log: &mut fs::File,
        timeout: Duration,
    ) -> Result<Transcript> {
        let last_message = last_message_file(dir)?;
        let command = self.command(prompt, dir, Some(assets))?;
        let program = PathBuf::from(command.get_program());
        let mut relay = Relay::new(io::stdout(), &mut *log, self.harness.streams_events());
        let _deferred = defer_interrupts();
        let input = self.stdin_prompt(prompt);
        let ended = supervise(command, input, timeout, true, |from_stdout, line| {
            relay.line(from_stdout, line)
        })
        .map_err(|error| self.failed(&program, "run failed", error))?;
        let streamed = relay.finish();
        if !ended.pipes_closed {
            writeln!(
                log,
                "# testson: stopped reading {}s after the agent ended, while a process it started \
                 still held its output",
                DRAIN.as_secs()
            )?;
        }
        log.flush()?;
        let code = exit_code(ended.status);
        let transcript = if self.harness.writes_last_message() {
            Transcript {
                last_message: fs::read_to_string(&last_message).ok(),
                ..Transcript::from_stream(code, None)
            }
        } else {
            Transcript::from_stream(code, streamed)
        };
        Ok(Transcript {
            timed_out: ended.timed_out,
            interrupted: ended.interrupted,
            ..transcript
        })
    }

    /// What to write on the harness's stdin: the prompt, for a harness that reads it from there.
    fn stdin_prompt<'a>(&self, prompt: &'a str) -> Option<&'a str> {
        self.harness.reads_prompt_from_stdin().then_some(prompt)
    }

    /// Says what went wrong with a run of `program`, and explains the refusal that is particular
    /// to a Windows batch shim: Rust starts one through `cmd.exe` and refuses an argument that
    /// spans several lines. Only a harness that takes the prompt as an argument can meet it.
    fn failed(&self, program: &Path, what: &str, error: anyhow::Error) -> anyhow::Error {
        let refused = error
            .downcast_ref::<io::Error>()
            .is_some_and(|error| error.kind() == io::ErrorKind::InvalidInput);
        if refused && is_batch_file(program) {
            error.context(format!(
                "{harness} failed to start: the prompt cannot be passed as an argument to the \
                 batch shim {shim}, because Windows refuses batch file arguments that span \
                 several lines. Put a native {harness}.exe on PATH, which is used before a shim.",
                harness = self.harness,
                shim = program.display(),
            ))
        } else {
            error.context(format!("{} {what}", self.harness))
        }
    }

    /// The prompt is an argument only for a harness that cannot read it from stdin, since
    /// arguments show in process listings. The program is the one a `PATH` lookup finds, see
    /// [`find_on_path`].
    ///
    /// `assets` is the evidence directory of a test and grants the tools a test needs. A ping
    /// passes `None` and runs without them.
    ///
    /// Claude runs in `dontAsk` mode: only the shell commands in `CLAUDE_ALLOWED` and reads in
    /// `dir` and `assets` go through, and no MCP server or browser integration is loaded. Codex
    /// and Grok still run unsandboxed with every approval skipped, because none of their sandbox
    /// modes is known to let `playwright-cli` start a browser. Codex at least loses web search.
    ///
    /// # Errors
    ///
    /// Fails when the Codex last-message file cannot be given as an absolute path.
    fn command(&self, prompt: &str, dir: &Path, assets: Option<&Path>) -> Result<Command> {
        let binary = self.harness.binary();
        let mut command = Command::new(find_on_path(binary).unwrap_or_else(|| binary.into()));
        command.current_dir(dir);
        match self.harness {
            Harness::Claude => {
                command.arg("--print");
                if let Some(assets) = assets {
                    command.args(["--permission-mode", "dontAsk"]);
                    command.args(["--tools", CLAUDE_TOOLS]);
                    command.arg("--allowedTools").args(CLAUDE_ALLOWED);
                    command.arg("--disallowedTools").args(CLAUDE_DENIED);
                    command.arg("--add-dir").arg(assets);
                    command.args(["--strict-mcp-config", "--no-chrome"]);
                    command.args(["--output-format", "stream-json", "--verbose"]);
                }
                if let Some(model) = &self.model {
                    command.arg("--model").arg(model);
                }
                if let Some(effort) = &self.effort {
                    command.arg("--effort").arg(effort);
                }
                // With no prompt argument, `--print` reads the prompt from stdin.
            }
            Harness::Codex => {
                command.args(["exec", "--skip-git-repo-check", "--color", "never"]);
                command
                    .arg("--output-last-message")
                    .arg(last_message_file(dir)?);
                if assets.is_some() {
                    command.arg("--dangerously-bypass-approvals-and-sandbox");
                    command.args(["--config", "web_search=disabled"]);
                } else {
                    command.arg("--ephemeral");
                }
                if let Some(model) = &self.model {
                    command.arg("--model").arg(model);
                }
                if let Some(effort) = &self.effort {
                    command
                        .arg("--config")
                        .arg(format!("model_reasoning_effort={effort}"));
                }
                // `-` reads the prompt from stdin.
                command.arg("-");
            }
            Harness::Grok => {
                if assets.is_some() {
                    command.args(["--permission-mode", "bypassPermissions"]);
                    command.args(["--output-format", "streaming-messages-json"]);
                }
                if let Some(model) = &self.model {
                    command.arg("--model").arg(model);
                }
                if let Some(effort) = &self.effort {
                    command.arg("--reasoning-effort").arg(effort);
                }
                command.arg("--single").arg(prompt);
            }
        }
        Ok(command)
    }
}

impl fmt::Display for Agent {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} (model {}", self.harness, self.model_label())?;
        if let Some(effort) = &self.effort {
            write!(formatter, ", effort {effort}")?;
        }
        formatter.write_str(")")
    }
}

/// The file Codex writes its final message to when it runs in `dir`.
///
/// The CLI runs in `dir`, so a relative path would be resolved against it a second time and the
/// file would land where it is never read.
///
/// # Errors
///
/// Fails when the path cannot be made absolute.
fn last_message_file(dir: &Path) -> Result<PathBuf> {
    let path = dir.join(LAST_MESSAGE_FILE);
    std::path::absolute(&path).with_context(|| format!("could not resolve {}", path.display()))
}

/// Whether a `PATH` lookup of `name` finds a program that can be started.
#[must_use]
pub fn on_path(name: &str) -> bool {
    find_on_path(name).is_some()
}

/// The full path of the program a `PATH` lookup of `name` starts.
///
/// `Command::new` with a bare name looks only for `name.exe` on Windows, which misses the `.cmd`
/// shims npm installs, so the program is spawned by this path instead.
#[must_use]
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    #[cfg(windows)]
    {
        find_in(name, &path, std::env::var_os("PATHEXT").as_deref())
    }
    #[cfg(not(windows))]
    {
        find_in(name, &path)
    }
}

/// The first executable file named `name` in the directories of `path`, made absolute.
#[cfg(not(windows))]
fn find_in(name: &str, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
        .map(absolute)
}

/// `name` plus one of the `pathext` extensions in the directories of `path`, made absolute.
///
/// Only the extensions `CreateProcess` can start count: an extensionless file, such as the shell
/// script npm puts beside each shim, or a `.js` file, is not a program there. A native `.exe` or
/// `.com` anywhere on `path` is preferred over a `.bat` or `.cmd` shim, since a batch file cannot
/// take every argument.
#[cfg(windows)]
fn find_in(name: &str, path: &OsStr, pathext: Option<&OsStr>) -> Option<PathBuf> {
    let extensions = launchable_extensions(pathext);
    let (native, batch): (Vec<&str>, Vec<&str>) = extensions
        .iter()
        .map(String::as_str)
        .partition(|extension| matches!(*extension, ".com" | ".exe"));
    [native, batch]
        .iter()
        .find_map(|group| {
            std::env::split_paths(path).find_map(|directory| {
                group
                    .iter()
                    .map(|extension| directory.join(format!("{name}{extension}")))
                    .find(|candidate| candidate.is_file())
            })
        })
        .map(absolute)
}

/// The `PATHEXT` extensions Rust can start, in lower case, or the Windows default when none is
/// left.
#[cfg(windows)]
fn launchable_extensions(pathext: Option<&OsStr>) -> Vec<String> {
    let parse = |pathext: &str| -> Vec<String> {
        pathext
            .split(';')
            .map(|extension| extension.trim().to_ascii_lowercase())
            .filter(|extension| matches!(extension.as_str(), ".com" | ".exe" | ".bat" | ".cmd"))
            .collect()
    };
    let extensions = pathext.map(|pathext| parse(&pathext.to_string_lossy()));
    match extensions {
        Some(extensions) if !extensions.is_empty() => extensions,
        _ => parse(".COM;.EXE;.BAT;.CMD"),
    }
}

/// The program runs in the run directory, so a relative `PATH` entry must not stay relative.
fn absolute(path: PathBuf) -> PathBuf {
    std::path::absolute(&path).unwrap_or(path)
}

#[cfg(not(windows))]
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// Rust starts a `.bat` or `.cmd` file through `cmd.exe`, and refuses the arguments it cannot
/// quote for it.
fn is_batch_file(program: &Path) -> bool {
    program.extension().is_some_and(|extension| {
        extension.eq_ignore_ascii_case("bat") || extension.eq_ignore_ascii_case("cmd")
    })
}

/// Writes `text` to the child's stdin on a background thread, then closes it.
///
/// The write runs beside the readers of the output pipes, so a prompt larger than the pipe buffer
/// cannot block while the child waits for its output to be read. A failed write means the child
/// exited early, which its exit status reports.
fn feed(child: &mut Child, text: &str) {
    let Some(mut stdin) = child.stdin.take() else {
        return;
    };
    let text = text.to_owned();
    thread::spawn(move || {
        let _ = stdin.write_all(text.as_bytes());
    });
}

/// Shows each line of an agent's output and writes it to the model log.
///
/// For a harness that emits stream events, stdout events are rendered as readable lines and the
/// final answer is kept, with whether the harness marked it as an error.
pub(crate) struct Relay<'a, C, L> {
    /// `None` once the console is gone.
    console: Option<C>,
    log: &'a mut L,
    streams: bool,
    result: Option<(String, bool)>,
}

impl<'a, C: Write, L: Write> Relay<'a, C, L> {
    pub(crate) fn new(console: C, log: &'a mut L, streams: bool) -> Self {
        Self {
            console: Some(console),
            log,
            streams,
            result: None,
        }
    }

    /// Shows and logs one line of output, from stdout or from stderr.
    ///
    /// The console is best effort, so the agent is never cut off mid-test by it, and the log
    /// still gets every line. A line that cannot be shown is dropped. Once the console is gone,
    /// because the terminal went away or a pipe such as `| head` stopped reading, nothing more is
    /// written to it; after any other error, such as a non-blocking stdout that is full, the next
    /// line is tried.
    ///
    /// # Errors
    ///
    /// Fails when the log cannot be written.
    pub(crate) fn line(&mut self, from_stdout: bool, line: String) -> Result<()> {
        for line in render(line, self.streams && from_stdout, &mut self.result) {
            if let Some(console) = self.console.as_mut()
                && writeln!(console, "{line}").is_err_and(|error| is_gone(&error))
            {
                self.console = None;
            }
            writeln!(self.log, "{line}").context("could not write the model log")?;
        }
        if let Some(console) = self.console.as_mut()
            && console.flush().is_err_and(|error| is_gone(&error))
        {
            self.console = None;
        }
        Ok(())
    }

    /// The final answer and whether it is an error, if a `result` event came.
    pub(crate) fn finish(self) -> Option<(String, bool)> {
        self.result
    }
}

/// Whether a write failed because nothing reads the stream any more.
///
/// A pipe whose reader exited is `BrokenPipe` everywhere: Rust ignores `SIGPIPE` on Unix, and
/// maps Windows' `ERROR_BROKEN_PIPE` and `ERROR_NO_DATA` to it.
fn is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::BrokenPipe
}

/// The lines to show for one line of output. For a stream event, the readable lines; the final
/// answer is also kept in `result`.
fn render(line: String, event: bool, result: &mut Option<(String, bool)>) -> Vec<String> {
    match events::parse(&line) {
        _ if !event => vec![line],
        events::Event::Show(lines) => lines,
        events::Event::Result {
            text,
            is_error,
            subtype,
        } => {
            let text = events::Event::answer(text, is_error, subtype);
            let shown = if is_error {
                vec![format!("agent error: {text}")]
            } else {
                vec![text.clone()]
            };
            *result = Some((text, is_error));
            shown
        }
    }
}

/// Lets testson stop what it supervises before it exits on Ctrl+C or a termination signal.
///
/// While nothing defers interrupts, testson exits at once. While a [`DeferInterrupts`] is held,
/// an interrupt stops the agent's process tree and is otherwise only noted: the run goes on to
/// clean up and store its test, and then calls [`exit_if_interrupted`].
///
/// # Errors
///
/// Fails when the handler cannot be installed.
pub fn handle_interrupts() -> Result<()> {
    ctrlc::set_handler(|| {
        INTERRUPTED.store(true, Ordering::SeqCst);
        if DEFERRING.load(Ordering::SeqCst) == 0 {
            std::process::exit(130);
        }
    })
    .context("could not install the interrupt handler")
}

/// Exits with the conventional status when testson was interrupted.
pub fn exit_if_interrupted() {
    if INTERRUPTED.load(Ordering::SeqCst) {
        esay!("Interrupted. The agent was stopped.");
        std::process::exit(130);
    }
}

/// Holds off the exit on an interrupt until it is dropped and [`exit_if_interrupted`] is called.
#[must_use = "interrupts are deferred only while this is held"]
pub struct DeferInterrupts(());

/// Starts deferring interrupts. See [`handle_interrupts`].
pub fn defer_interrupts() -> DeferInterrupts {
    DEFERRING.fetch_add(1, Ordering::SeqCst);
    DeferInterrupts(())
}

impl Drop for DeferInterrupts {
    fn drop(&mut self) {
        DEFERRING.fetch_sub(1, Ordering::SeqCst);
    }
}

/// How a supervised process ended.
#[derive(Debug)]
struct Ended {
    status: ExitStatus,
    /// The timeout passed and the process tree was stopped.
    timed_out: bool,
    /// The operator interrupted testson and the process tree was stopped.
    interrupted: bool,
    /// Both pipes reached their end, so nothing the process started still holds them.
    pipes_closed: bool,
}

/// Runs `command` as a process tree of its own, writes `input`, if any, to its stdin, hands each
/// line of its stdout and stderr to `on_line`, and stops the whole tree once it exits, fails, or
/// outlives `timeout`, or, if it is `interruptible`, once the operator interrupts testson.
///
/// The tree does not receive the terminal's Ctrl+C itself, so a cleanup command finishes even
/// when it runs during one.
///
/// Output is read until both pipes close, or for [`DRAIN`] after the process exits, whichever
/// comes first, so a background process that inherited the pipes cannot keep testson waiting.
///
/// # Errors
///
/// Fails when the process cannot start or be waited for, or when `on_line` fails. The tree is
/// stopped in every case.
fn supervise(
    mut command: Command,
    input: Option<&str>,
    timeout: Duration,
    interruptible: bool,
    mut on_line: impl FnMut(bool, String) -> Result<()>,
) -> Result<Ended> {
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    #[cfg(windows)]
    std::os::windows::process::CommandExt::creation_flags(&mut command, CREATE_NEW_PROCESS_GROUP);
    let mut child = command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(input) = input {
        feed(&mut child, input);
    }
    let stdout = child.stdout.take().context("stdout was not piped")?;
    let stderr = child.stderr.take().context("stderr was not piped")?;
    let (sender, receiver) = mpsc::channel::<(bool, String)>();
    let stderr_sender = sender.clone();
    thread::spawn(move || pump(stdout, true, &sender));
    thread::spawn(move || pump(stderr, false, &stderr_sender));
    let deadline = Instant::now() + timeout;
    let mut pipes_closed = false;
    let mut exited: Option<(ExitStatus, Instant)> = None;
    let mut timed_out = false;
    let mut interrupted = false;
    loop {
        if pipes_closed {
            thread::sleep(POLL);
        } else {
            match receiver.recv_timeout(POLL) {
                Ok((from_stdout, line)) => {
                    if let Err(error) = on_line(from_stdout, line) {
                        let _ = stop_tree(&mut child);
                        return Err(error);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => pipes_closed = true,
            }
        }
        if let Some((status, at)) = exited {
            if pipes_closed || at.elapsed() >= DRAIN {
                return Ok(Ended {
                    status,
                    timed_out,
                    interrupted,
                    pipes_closed,
                });
            }
            continue;
        }
        let running = child.try_wait()?.is_none();
        interrupted = running && interruptible && INTERRUPTED.load(Ordering::SeqCst);
        timed_out = running && !interrupted && Instant::now() >= deadline;
        if !running || interrupted || timed_out {
            // Whatever the process left running in its tree goes with it.
            exited = Some((stop_tree(&mut child)?, Instant::now()));
        }
    }
}

/// Runs `command` without showing its output, for at most `timeout`. An interrupt does not stop
/// it, so it can clean up after one.
///
/// # Errors
///
/// Fails when the process cannot start, or does not finish in time and is stopped.
pub fn run_quietly(command: Command, timeout: Duration) -> Result<ExitStatus> {
    let ended = supervise(command, None, timeout, false, |_, _| Ok(()))?;
    if ended.timed_out {
        bail!("did not finish within {}s", timeout.as_secs());
    }
    Ok(ended.status)
}

/// Stops `child` and every process left in its group: politely, then by force.
///
/// A group ID cannot be reused while its leader is unreaped or any member is alive, so the group
/// is only signalled while one of those holds. `child` may already be reaped; then the group is
/// signalled only if it still has members.
#[cfg(unix)]
fn stop_tree(child: &mut Child) -> io::Result<ExitStatus> {
    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::Pid;

    if let Ok(id) = i32::try_from(child.id()) {
        let group = Pid::from_raw(id);
        let held = |child: &mut Child| -> io::Result<bool> {
            Ok(child.try_wait()?.is_none() || killpg(group, None).is_ok())
        };
        if held(child)? {
            let _ = killpg(group, Signal::SIGTERM);
            let deadline = Instant::now() + KILL_GRACE;
            while held(child)? && Instant::now() < deadline {
                thread::sleep(POLL);
            }
            if held(child)? {
                let _ = killpg(group, Signal::SIGKILL);
            }
        }
    }
    // In case the group could not be signalled. Harmless once `child` has exited.
    let _ = child.kill();
    child.wait()
}

/// Kills `child` and the processes it started. Windows finds them through the parent of each
/// process, so the tree is walked while `child` still runs; once it has exited there is no tree
/// left to walk.
#[cfg(not(unix))]
fn stop_tree(child: &mut Child) -> io::Result<ExitStatus> {
    if child.try_wait()?.is_none() {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &child.id().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    child.wait()
}

/// Parses `30m`, `1h`, or a number of seconds, at least `minimum` seconds. `what` names the
/// value in the error.
///
/// # Errors
///
/// Fails when the text is not a duration or is shorter than `minimum`.
pub fn parse_duration(value: &str, what: &str, minimum: u64) -> Result<Duration, String> {
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
        .ok_or_else(|| format!("{what} must look like 30m, 1h, or 300"))?;
    let seconds = amount
        .checked_mul(factor)
        .ok_or_else(|| format!("{what} is too large"))?;
    if seconds < minimum {
        return Err(format!("{what} must be at least {minimum}s"));
    }
    Ok(Duration::from_secs(seconds))
}

/// Parses the run timeout, at least 60s. Used as a clap value parser.
///
/// # Errors
///
/// Fails when the text is not a duration or is shorter than 60 seconds.
pub fn parse_timeout(value: &str) -> Result<Duration, String> {
    parse_duration(value, "timeout", 60)
}

fn pump(reader: impl Read, from_stdout: bool, sender: &mpsc::Sender<(bool, String)>) {
    let mut reader = BufReader::new(reader);
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        match reader.read_until(b'\n', &mut buffer) {
            Ok(0) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buffer);
                let line = line.trim_end_matches(['\n', '\r']).to_owned();
                if sender.send((from_stdout, line)).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        return code;
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return 128 + signal;
        }
    }
    1
}

fn tail(text: &str, lines: usize) -> String {
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    kept[kept.len().saturating_sub(lines)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::prompt::TEMPLATE;

    /// The arguments of a test when `test` is true, of a ping otherwise.
    fn args(agent: &Agent, test: bool) -> Vec<String> {
        agent
            .command(
                "PROMPT",
                Path::new("/tmp"),
                test.then_some(Path::new("/tmp/assets")),
            )
            .unwrap()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The programs `CLAUDE_ALLOWED` lets a Claude test run.
    fn allowed_programs() -> Vec<&'static str> {
        CLAUDE_ALLOWED
            .iter()
            .map(|rule| {
                rule.strip_prefix("Bash(")
                    .and_then(|rule| rule.strip_suffix(":*)"))
                    .expect("every allow rule is a Bash prefix rule")
            })
            .collect()
    }

    #[test]
    fn claude_keeps_the_previous_defaults() {
        let agent = Agent::new(Harness::Claude, None, None);
        assert_eq!(agent.to_string(), "claude (model sonnet, effort medium)");
        assert_eq!(
            args(&agent, true),
            [
                "--print",
                "--permission-mode",
                "dontAsk",
                "--tools",
                "Bash,Read",
                "--allowedTools",
                "Bash(playwright-cli:*)",
                "Bash(echo:*)",
                "Bash(date:*)",
                "Bash(ls:*)",
                "--disallowedTools",
                "WebFetch",
                "WebSearch",
                "Edit",
                "Write",
                "NotebookEdit",
                "PowerShell",
                "Agent",
                "mcp__*",
                "--add-dir",
                "/tmp/assets",
                "--strict-mcp-config",
                "--no-chrome",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "sonnet",
                "--effort",
                "medium"
            ]
        );
        assert_eq!(
            args(&agent, false),
            ["--print", "--model", "sonnet", "--effort", "medium"]
        );
    }

    #[test]
    fn claude_never_bypasses_permissions() {
        let agent = Agent::new(Harness::Claude, None, None);
        for test in [true, false] {
            let args = args(&agent, test);
            for flag in [
                "bypassPermissions",
                "--dangerously-skip-permissions",
                "--allow-dangerously-skip-permissions",
            ] {
                assert!(!args.contains(&flag.to_owned()), "{flag} in {args:?}");
            }
        }
    }

    #[test]
    fn the_prompt_runs_only_what_claude_allows() {
        assert!(
            !TEMPLATE.contains("$("),
            "an allow rule cannot approve a command substitution reliably"
        );
        let programs = allowed_programs();
        for line in TEMPLATE.lines().map(str::trim) {
            if line.starts_with("playwright-cli ") || line.starts_with("echo ") {
                let program = line.split_whitespace().next().unwrap();
                assert!(programs.contains(&program), "{line}");
            }
        }
        let (last, rest) = programs.split_last().unwrap();
        let sentence = format!("run only {}, and {last}.", rest.join(", "));
        assert!(
            TEMPLATE.contains(&sentence),
            "the prompt lacks {sentence:?}"
        );
    }

    #[test]
    fn claude_and_codex_never_take_the_prompt_as_an_argument() {
        for harness in [Harness::Claude, Harness::Codex] {
            assert!(harness.reads_prompt_from_stdin());
            let agent = Agent::new(harness, Some("m".into()), Some("high".into()));
            for autonomous in [true, false] {
                let args = args(&agent, autonomous);
                assert!(
                    !args.iter().any(|arg| arg.contains("PROMPT")),
                    "{harness}: {args:?}"
                );
            }
        }
        assert!(!Harness::Grok.reads_prompt_from_stdin());
    }

    #[test]
    fn feeds_a_large_prompt_on_stdin_and_closes_it() {
        // `sort` reads stdin to the end before it writes, so it only exits once stdin is closed.
        let lines = 50_000;
        let text = "a line of the prompt\n".repeat(lines);
        let mut count = 0;
        let ended = supervise(
            Command::new("sort"),
            Some(&text),
            Duration::from_secs(60),
            false,
            |from_stdout, _| {
                count += usize::from(from_stdout);
                Ok(())
            },
        )
        .unwrap();
        assert!(ended.status.success() && !ended.timed_out);
        assert_eq!(count, lines);
    }

    #[test]
    fn codex_passes_model_effort_and_last_message_file() {
        let agent = Agent::new(Harness::Codex, Some("gpt-5".into()), Some("high".into()));
        let full = args(&agent, true);
        assert_eq!(full[0], "exec");
        assert!(full.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
        assert!(full.contains(&"web_search=disabled".to_owned()));
        let file = last_message_file(Path::new("/tmp")).unwrap();
        assert!(full.contains(&file.to_string_lossy().into_owned()));
        assert!(full.contains(&"model_reasoning_effort=high".to_owned()));
        assert_eq!(full.last().unwrap(), "-");
        let ping = args(&agent, false);
        assert!(!ping.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
    }

    #[test]
    fn codex_writes_its_last_message_to_an_absolute_path() {
        // Codex runs in the run directory, which is relative under the default data directory.
        let dir = Path::new("data-dir/runs/alpha-20261014-120000");
        let agent = Agent::new(Harness::Codex, None, None);
        let command = agent
            .command("PROMPT", dir, Some(Path::new("/tmp/assets")))
            .unwrap();
        let args: Vec<_> = command.get_args().collect();
        let index = args
            .iter()
            .position(|arg| *arg == "--output-last-message")
            .unwrap();
        let file = Path::new(args[index + 1]);
        assert!(file.is_absolute(), "{}", file.display());
        assert!(file.ends_with(dir.join(LAST_MESSAGE_FILE)));
        assert_eq!(file, last_message_file(dir).unwrap());
    }

    #[test]
    fn grok_uses_the_cli_default_model_and_passes_effort() {
        let agent = Agent::new(Harness::Grok, None, None);
        assert_eq!(agent.to_string(), "grok (model CLI default)");
        assert_eq!(
            args(&agent, true),
            [
                "--permission-mode",
                "bypassPermissions",
                "--output-format",
                "streaming-messages-json",
                "--single",
                "PROMPT"
            ]
        );
        let tuned = Agent::new(Harness::Grok, Some("grok-5".into()), Some("high".into()));
        assert_eq!(
            args(&tuned, false),
            [
                "--model",
                "grok-5",
                "--reasoning-effort",
                "high",
                "--single",
                "PROMPT"
            ]
        );
    }

    /// Runs `unix` with `sh` on Unix, and `windows` with `cmd` elsewhere.
    fn shell(unix: &str, windows: &[&str]) -> Command {
        if cfg!(unix) {
            let mut command = Command::new("sh");
            command.args(["-c", unix]);
            command
        } else {
            let mut command = Command::new("cmd");
            command.arg("/C").args(windows);
            command
        }
    }

    fn supervised(command: Command, timeout: Duration) -> (Ended, Vec<String>, Duration) {
        let started = Instant::now();
        let mut lines = Vec::new();
        let ended = supervise(command, None, timeout, true, |_, line| {
            lines.push(line);
            Ok(())
        })
        .unwrap();
        (ended, lines, started.elapsed())
    }

    #[test]
    fn a_run_that_outlives_its_timeout_is_stopped_with_its_tree() {
        // The shell waits on a child of its own that holds the output for 30 seconds.
        let command = shell(
            "echo started; sleep 30; echo done",
            &[
                "echo",
                "started",
                "&",
                "ping",
                "-n",
                "31",
                "127.0.0.1",
                "&",
                "echo",
                "done",
            ],
        );
        // Three seconds, not one: a cold `cmd.exe` can take over a second to print the first line
        // and start its child, and the tree must exist when the timeout stops it.
        let (ended, lines, elapsed) = supervised(command, Duration::from_secs(3));
        assert!(ended.timed_out);
        assert!(!ended.status.success());
        assert!(
            ended.pipes_closed,
            "a process of the stopped tree still holds the output"
        );
        assert!(elapsed < Duration::from_secs(15), "took {elapsed:?}");
        assert!(lines.iter().any(|line| line.contains("started")));
        assert!(!lines.iter().any(|line| line == "done"));
    }

    #[test]
    fn a_background_process_holding_the_output_does_not_keep_the_run_waiting() {
        // The shell exits at once and leaves a process behind that holds its stdout.
        let command = shell(
            "echo started; sleep 30 &",
            &[
                "echo",
                "started",
                "&",
                "start",
                "/B",
                "ping",
                "-n",
                "31",
                "127.0.0.1",
            ],
        );
        let (ended, lines, elapsed) = supervised(command, Duration::from_secs(60));
        assert!(!ended.timed_out);
        assert!(ended.status.success());
        assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
        assert!(lines.iter().any(|line| line.contains("started")));
        // On Unix the process group, leftovers included, is stopped once the shell exits.
        // Windows cannot find an orphan, so testson stops reading instead.
        assert_eq!(ended.pipes_closed, cfg!(unix));
    }

    #[test]
    fn a_quick_command_runs_to_its_end() {
        let command = shell("echo one; echo two", &["echo", "one", "&", "echo", "two"]);
        let (ended, lines, _) = supervised(command, Duration::from_secs(60));
        assert!(ended.status.success() && !ended.timed_out && ended.pipes_closed);
        let lines: Vec<&str> = lines.iter().map(|line| line.trim()).collect();
        assert_eq!(lines, ["one", "two"]);
    }

    #[test]
    fn parses_timeouts() {
        assert_eq!(
            parse_timeout(DEFAULT_TIMEOUT),
            Ok(Duration::from_secs(1_200))
        );
        assert_eq!(parse_timeout("1h"), Ok(Duration::from_secs(3_600)));
        assert_eq!(parse_timeout("90"), Ok(Duration::from_secs(90)));
        assert_eq!(
            parse_timeout("59s"),
            Err("timeout must be at least 60s".to_owned())
        );
        assert_eq!(
            parse_timeout("soon"),
            Err("timeout must look like 30m, 1h, or 300".to_owned())
        );
        assert_eq!(parse_duration("5", "delay", 1), Ok(Duration::from_secs(5)));
    }

    /// Writes `name` into `dir`, executable on Unix.
    fn touch(dir: &Path, name: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, "@echo off\r\necho %*\r\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    fn lookup(name: &str, directories: &[&Path]) -> Option<PathBuf> {
        let path = std::env::join_paths(directories).unwrap();
        #[cfg(windows)]
        {
            find_in(name, &path, None)
        }
        #[cfg(not(windows))]
        {
            find_in(name, &path)
        }
    }

    #[test]
    fn a_path_lookup_finds_the_program_the_platform_starts() {
        let dir = tempfile::tempdir().unwrap();
        let shim = touch(dir.path(), "tool.cmd");
        let script = touch(dir.path(), "tool");
        let expected = if cfg!(windows) { shim } else { script };
        assert_eq!(lookup("tool", &[dir.path()]), Some(expected));
        assert_eq!(lookup("missing", &[dir.path()]), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_native_exe_is_preferred_over_a_shim_and_pathext_is_honoured() {
        let shims = tempfile::tempdir().unwrap();
        let native = tempfile::tempdir().unwrap();
        touch(shims.path(), "tool");
        let shim = touch(shims.path(), "tool.cmd");
        assert_eq!(lookup("tool", &[shims.path()]), Some(shim.clone()));
        let exe = touch(native.path(), "tool.exe");
        assert_eq!(lookup("tool", &[shims.path(), native.path()]), Some(exe));
        let path = std::env::join_paths([shims.path()]).unwrap();
        assert_eq!(find_in("tool", &path, Some(OsStr::new(".EXE"))), None);
        assert_eq!(
            find_in("tool", &path, Some(OsStr::new(".JS;.Cmd"))),
            Some(shim)
        );
        touch(shims.path(), "only");
        assert_eq!(lookup("only", &[shims.path()]), None);
    }

    #[cfg(windows)]
    #[test]
    fn a_pathext_with_nothing_launchable_falls_back_to_the_default() {
        let default = [".com", ".exe", ".bat", ".cmd"];
        for pathext in ["", ";;", "EXE;CMD", ".JS;.VBS"] {
            assert_eq!(launchable_extensions(Some(OsStr::new(pathext))), default);
        }
        assert_eq!(launchable_extensions(None), default);
        assert_eq!(
            launchable_extensions(Some(OsStr::new(" .EXE ; .Cmd ;.js"))),
            [".exe", ".cmd"]
        );
        let dir = tempfile::tempdir().unwrap();
        let shim = touch(dir.path(), "tool.cmd");
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(find_in("tool", &path, Some(OsStr::new(""))), Some(shim));
    }

    #[test]
    fn a_relative_path_entry_resolves_to_an_absolute_path() {
        let dir = tempfile::tempdir_in(".").unwrap();
        let relative = Path::new(dir.path().file_name().unwrap());
        let name = if cfg!(windows) { "tool.cmd" } else { "tool" };
        touch(relative, name);
        let found = lookup("tool", &[relative]).unwrap();
        assert!(found.is_absolute());
        assert_eq!(found, std::path::absolute(relative.join(name)).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn a_multi_line_prompt_for_a_batch_shim_is_explained() {
        let dir = tempfile::tempdir().unwrap();
        let shim = touch(dir.path(), "grok.cmd");
        let agent = Agent::new(Harness::Grok, None, None);
        let run = |argument: &str| {
            let mut command = Command::new(&shim);
            command.arg(argument);
            supervise(command, None, Duration::from_secs(60), false, |_, _| Ok(()))
                .map_err(|error| agent.failed(&shim, "run failed", error))
        };
        let error = run("line one\nline two").unwrap_err();
        assert!(format!("{error:#}").contains("cannot be passed as an argument to the batch shim"));
        assert!(run("one line").unwrap().status.success());
    }

    /// A harness that reads the prompt from stdin gets the whole of it through an npm shim too,
    /// which runs under `cmd.exe`.
    #[cfg(windows)]
    #[test]
    fn a_batch_shim_reads_a_multi_line_prompt_from_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("claude.cmd");
        fs::write(&shim, "@findstr \"^\"\r\n").unwrap();
        let mut lines = Vec::new();
        let ended = supervise(
            Command::new(&shim),
            Some("line one\nline two\n"),
            Duration::from_secs(60),
            false,
            |from_stdout, line| {
                if from_stdout {
                    lines.push(line);
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(ended.status.success() && !ended.timed_out);
        assert_eq!(lines, ["line one", "line two"]);
    }

    #[test]
    fn batch_files_are_recognised_by_extension() {
        assert!(is_batch_file(Path::new(r"C:\npm\codex.cmd")));
        assert!(is_batch_file(Path::new("run.BAT")));
        assert!(!is_batch_file(Path::new("claude.exe")));
        assert!(!is_batch_file(Path::new("claude")));
    }

    /// Hands `lines` to a [`Relay`], as the supervised run does, and returns its final answer.
    fn relay(
        lines: impl IntoIterator<Item = (bool, String)>,
        console: impl Write,
        log: &mut Vec<u8>,
        streams: bool,
    ) -> Result<Option<String>> {
        let mut relay = Relay::new(console, log, streams);
        for (from_stdout, line) in lines {
            relay.line(from_stdout, line)?;
        }
        Ok(relay.finish().map(|(text, _)| text))
    }

    /// A console whose reader is gone: every write fails, as with a broken pipe.
    struct ClosedConsole {
        attempts: usize,
    }

    impl Write for ClosedConsole {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            self.attempts += 1;
            Err(io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> io::Result<()> {
            self.attempts += 1;
            Err(io::ErrorKind::BrokenPipe.into())
        }
    }

    #[test]
    fn a_closed_console_does_not_stop_the_run_or_the_log() {
        let lines = [
            (true, r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo '[PROGRESS] Opening the lobby'"}}]}}"#.to_owned()),
            (false, "a warning on stderr".to_owned()),
            (true, r#"{"type":"result","is_error":false,"result":"{\"ok\":1}"}"#.to_owned()),
        ];
        let mut console = ClosedConsole { attempts: 0 };
        let mut log = Vec::new();
        let result = relay(lines, &mut console, &mut log, true).unwrap();
        assert_eq!(result.as_deref(), Some(r#"{"ok":1}"#));
        assert_eq!(
            String::from_utf8(log).unwrap(),
            "[PROGRESS] Opening the lobby\na warning on stderr\n{\"ok\":1}\n"
        );
        assert_eq!(
            console.attempts, 1,
            "the console is left alone after it fails"
        );
    }

    /// A non-blocking console that is full once, then takes everything.
    struct BusyConsole {
        busy: bool,
        shown: Vec<u8>,
    }

    impl Write for BusyConsole {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if std::mem::take(&mut self.busy) {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            self.shown.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_console_that_fails_for_a_moment_keeps_getting_lines() {
        let lines = ["one", "two", "three"].map(|line| (false, line.to_owned()));
        let mut console = BusyConsole {
            busy: true,
            shown: Vec::new(),
        };
        let mut log = Vec::new();
        relay(lines, &mut console, &mut log, false).unwrap();
        assert_eq!(console.shown, b"two\nthree\n");
        assert_eq!(log, b"one\ntwo\nthree\n");
    }

    #[test]
    fn an_open_console_gets_every_line() {
        let lines = [(false, "one".to_owned()), (false, "two".to_owned())];
        let mut console = Vec::new();
        let mut log = Vec::new();
        assert_eq!(relay(lines, &mut console, &mut log, false).unwrap(), None);
        assert_eq!(console, b"one\ntwo\n");
        assert_eq!(log, b"one\ntwo\n");
    }

    #[test]
    fn tail_keeps_the_last_non_blank_lines() {
        assert_eq!(tail("a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("", 3), "");
    }
}
