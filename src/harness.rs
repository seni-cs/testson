//! Coding-agent CLIs that can drive the test, and the model each one runs.

use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::events;

const PING_PROMPT: &str =
    "Reply with exactly the word PONG and nothing else. Do not use any tools.";
const PING_TIMEOUT: Duration = Duration::from_secs(120);
const PING_OUTPUT_LINES: usize = 15;

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
    /// The final answer, when the harness reports it apart from the log.
    pub last_message: Option<String>,
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
        let last_message = scratch.path().join("last-message.txt");
        let mut child = self
            .command(PING_PROMPT, scratch.path(), &last_message, false)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("{} failed to start", self.harness))?;
        let stdout = collect(child.stdout.take());
        let stderr = collect(child.stderr.take());
        let deadline = Instant::now() + PING_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            thread::sleep(Duration::from_millis(200));
        };
        let wait = Duration::from_secs(2);
        let stdout = stdout.recv_timeout(wait).unwrap_or_default();
        let stderr = stderr.recv_timeout(wait).unwrap_or_default();
        let answer = if self.harness.writes_last_message() {
            fs::read_to_string(&last_message).unwrap_or_default()
        } else {
            stdout.clone()
        };
        let problem = match status {
            None => format!("did not answer within {}s", PING_TIMEOUT.as_secs()),
            Some(status) if !status.success() => {
                format!("exited with status {}", exit_code(status))
            }
            Some(_) if !answer.to_uppercase().contains("PONG") => {
                "answered, but not with the expected reply".to_owned()
            }
            Some(_) => return Ok(()),
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

    /// Runs `prompt` with full tool permissions in `dir`.
    ///
    /// Combined stdout and stderr are streamed to the terminal and to `log`.
    ///
    /// # Errors
    ///
    /// Fails when the CLI cannot start or the log cannot be written.
    pub fn run(&self, prompt: &str, dir: &Path, log: &mut fs::File) -> Result<Transcript> {
        let last_message = dir.join("last-message.txt");
        let mut child = self
            .command(prompt, dir, &last_message, true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("{} failed to start", self.harness))?;
        let streamed = match tee(&mut child, log, self.harness.streams_events()) {
            Ok(streamed) => streamed,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
        };
        let status = child.wait()?;
        log.flush()?;
        let last_message = if self.harness.writes_last_message() {
            fs::read_to_string(&last_message).ok()
        } else {
            streamed
        };
        Ok(Transcript {
            exit_code: exit_code(status),
            last_message,
        })
    }

    /// `autonomous` grants the tool permissions a test needs. A ping runs without them.
    fn command(&self, prompt: &str, dir: &Path, last_message: &Path, autonomous: bool) -> Command {
        let mut command = Command::new(self.harness.binary());
        command.current_dir(dir);
        match self.harness {
            Harness::Claude => {
                command.arg("--print");
                if autonomous {
                    command.args(["--permission-mode", "bypassPermissions"]);
                    command.args(["--output-format", "stream-json", "--verbose"]);
                }
                if let Some(model) = &self.model {
                    command.arg("--model").arg(model);
                }
                if let Some(effort) = &self.effort {
                    command.arg("--effort").arg(effort);
                }
                command.arg("--").arg(prompt);
            }
            Harness::Codex => {
                command.args(["exec", "--skip-git-repo-check", "--color", "never"]);
                command.arg("--output-last-message").arg(last_message);
                if autonomous {
                    command.arg("--dangerously-bypass-approvals-and-sandbox");
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
                command.arg("--").arg(prompt);
            }
            Harness::Grok => {
                if autonomous {
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
        command
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

/// Whether `name` is an executable file in a `PATH` directory.
#[must_use]
pub fn on_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| is_executable(&directory.join(name)))
}

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

/// Reads a pipe to the end on a background thread.
fn collect(pipe: Option<impl Read + Send + 'static>) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        let _ = sender.send(String::from_utf8_lossy(&bytes).into_owned());
    });
    receiver
}

/// Streams both pipes, line by line, to the terminal and to `log`.
///
/// For a harness that emits stream events, stdout events are rendered as readable lines and the
/// final answer is returned.
fn tee(child: &mut Child, log: &mut impl Write, streams: bool) -> Result<Option<String>> {
    let stdout = child.stdout.take().context("stdout was not piped")?;
    let stderr = child.stderr.take().context("stderr was not piped")?;
    let (sender, receiver) = mpsc::channel::<(bool, String)>();
    let stderr_sender = sender.clone();
    thread::spawn(move || pump(stdout, true, &sender));
    thread::spawn(move || pump(stderr, false, &stderr_sender));
    let mut console = io::stdout();
    let mut result = None;
    for (from_stdout, line) in receiver {
        let lines = match events::parse(&line) {
            _ if !(streams && from_stdout) => vec![line],
            events::Event::Show(lines) => lines,
            events::Event::Result { text, is_error } => {
                let shown = if is_error {
                    vec![format!("agent error: {text}")]
                } else {
                    vec![text.clone()]
                };
                result = Some(text);
                shown
            }
        };
        for line in lines {
            writeln!(console, "{line}")?;
            writeln!(log, "{line}").context("could not write the model log")?;
        }
        console.flush()?;
    }
    Ok(result)
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

    fn args(agent: &Agent, autonomous: bool) -> Vec<String> {
        agent
            .command(
                "PROMPT",
                Path::new("/tmp"),
                Path::new("/tmp/last.txt"),
                autonomous,
            )
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
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
                "bypassPermissions",
                "--output-format",
                "stream-json",
                "--verbose",
                "--model",
                "sonnet",
                "--effort",
                "medium",
                "--",
                "PROMPT"
            ]
        );
        assert!(!args(&agent, false).contains(&"bypassPermissions".to_owned()));
    }

    #[test]
    fn codex_passes_model_effort_and_last_message_file() {
        let agent = Agent::new(Harness::Codex, Some("gpt-5".into()), Some("high".into()));
        let full = args(&agent, true);
        assert_eq!(full[0], "exec");
        assert!(full.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
        assert!(full.contains(&"/tmp/last.txt".to_owned()));
        assert!(full.contains(&"model_reasoning_effort=high".to_owned()));
        assert_eq!(full.last().unwrap(), "PROMPT");
        let ping = args(&agent, false);
        assert!(!ping.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
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

    #[test]
    fn tail_keeps_the_last_non_blank_lines() {
        assert_eq!(tail("a\n\nb\nc\n", 2), "b\nc");
        assert_eq!(tail("", 3), "");
    }
}
