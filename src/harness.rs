//! Coding-agent CLIs that can drive the test, and the model each one runs.

use std::fmt;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::events;

const PING_PROMPT: &str =
    "Reply with exactly the word PONG and nothing else. Do not use any tools.";
const PING_TIMEOUT: Duration = Duration::from_secs(120);
const PING_OUTPUT_LINES: usize = 15;
/// How long output is still read after the CLI exits.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

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

    /// Claude and Codex read the prompt from stdin, which works through an npm `.cmd` shim on
    /// Windows and keeps the prompt out of process listings. Grok only takes it as an argument.
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
    /// Exit status, or `None` when the run was stopped for taking too long. A signal is
    /// reported as `128 + signal` on Unix.
    pub exit_code: Option<i32>,
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
        let mut output = String::new();
        let run = self.execute(
            PING_PROMPT,
            scratch.path(),
            false,
            PING_TIMEOUT,
            |_, line| {
                output.push_str(&line);
                output.push('\n');
            },
        )?;
        let answer = run.last_message.as_deref().unwrap_or(&output);
        let problem = match run.exit_code {
            None => format!("did not answer within {}s", PING_TIMEOUT.as_secs()),
            Some(code) if code != 0 => format!("exited with status {code}"),
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
            output = tail(&output, PING_OUTPUT_LINES),
        );
    }

    /// Runs `prompt` with full tool permissions in `dir`, and stops it after `timeout`.
    ///
    /// Combined stdout and stderr are streamed to the terminal and to `log`.
    ///
    /// # Errors
    ///
    /// Fails when the CLI cannot start or the log cannot be written.
    pub fn run(
        &self,
        prompt: &str,
        dir: &Path,
        log: &mut fs::File,
        timeout: Duration,
    ) -> Result<Transcript> {
        let streams = self.harness.streams_events();
        let mut result = None;
        let mut written = Ok(());
        let mut transcript = self.execute(prompt, dir, true, timeout, |from_stdout, line| {
            let lines = match events::parse(&line) {
                _ if !(streams && from_stdout) => vec![line],
                events::Event::Show(lines) => lines,
                events::Event::Result { text, is_error } => {
                    let shown = if is_error {
                        format!("agent error: {text}")
                    } else {
                        text.clone()
                    };
                    result = Some(text);
                    vec![shown]
                }
            };
            for line in lines {
                // The terminal may be gone; the log is what must be kept.
                let _ = writeln!(io::stdout(), "{line}");
                if written.is_ok() {
                    written = writeln!(log, "{line}");
                }
            }
        })?;
        written.context("could not write the model log")?;
        log.flush()?;
        if transcript.last_message.is_none() {
            transcript.last_message = result;
        }
        Ok(transcript)
    }

    /// Starts the CLI in `dir`, hands each line it prints to `on_line`, with whether it came from
    /// stdout, and kills it once `timeout` passes.
    ///
    /// `autonomous` grants the tool permissions a test needs. A ping runs without them.
    fn execute(
        &self,
        prompt: &str,
        dir: &Path,
        autonomous: bool,
        timeout: Duration,
        mut on_line: impl FnMut(bool, String),
    ) -> Result<Transcript> {
        let last_message = dir.join("last-message.txt");
        let stdin_prompt = self.harness.reads_prompt_from_stdin();
        let mut child = self
            .command(prompt, dir, &last_message, autonomous)
            .stdin(if stdin_prompt {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("{} failed to start", self.harness))?;
        if let Some(mut stdin) = child.stdin.take() {
            // On a thread, so a prompt larger than the pipe cannot block while the CLI writes.
            let prompt = prompt.to_owned();
            thread::spawn(move || stdin.write_all(prompt.as_bytes()));
        }
        let stdout = child.stdout.take().context("stdout was not piped")?;
        let stderr = child.stderr.take().context("stderr was not piped")?;
        let (sender, receiver) = mpsc::channel();
        let stderr_sender = sender.clone();
        thread::spawn(move || pump(stdout, true, &sender));
        thread::spawn(move || pump(stderr, false, &stderr_sender));

        let deadline = Instant::now() + timeout;
        let mut exited_at = None;
        let mut timed_out = false;
        loop {
            match receiver.recv_timeout(Duration::from_millis(200)) {
                Ok((from_stdout, line)) => on_line(from_stdout, line),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if exited_at.is_none() && child.try_wait()?.is_some() {
                exited_at = Some(Instant::now());
            }
            // A process the agent left running may hold its output open after it exits.
            if exited_at.is_some_and(|exited: Instant| exited.elapsed() >= OUTPUT_GRACE) {
                break;
            }
            if exited_at.is_none() && Instant::now() >= deadline {
                timed_out = true;
                let _ = child.kill();
                break;
            }
        }
        let status = child.wait()?;
        let last_message = if self.harness.writes_last_message() {
            fs::read_to_string(&last_message).ok()
        } else {
            None
        };
        Ok(Transcript {
            exit_code: (!timed_out).then(|| exit_code(status)),
            last_message,
        })
    }

    fn command(&self, prompt: &str, dir: &Path, last_message: &Path, autonomous: bool) -> Command {
        let binary = self.harness.binary();
        let mut command = Command::new(find_on_path(binary).unwrap_or_else(|| binary.into()));
        command.current_dir(dir);
        match self.harness {
            Harness::Claude => {
                // With no prompt argument, `--print` reads the prompt from stdin.
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
                // `-` reads the prompt from stdin.
                command.arg("-");
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

/// Whether `name` is a program on `PATH`.
#[must_use]
pub fn on_path(name: &str) -> bool {
    find_on_path(name).is_some()
}

/// The path of the program `name` on `PATH`. On Windows, the extensions in `PATHEXT` are tried,
/// so an npm `.cmd` shim is found as well as an `.exe`.
#[must_use]
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .filter(|extension| !extension.is_empty())
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&std::env::var_os("PATH")?).find_map(|directory| {
        extensions
            .iter()
            .map(|extension| directory.join(format!("{name}{extension}")))
            .find(|path| is_executable(path))
    })
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
        assert_eq!(full.last().unwrap(), "-");
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
