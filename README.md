# testson

Tests Betsson casino games in demo mode (Modo prueba), and shows the results on a dashboard.

An AI coding agent (Claude Code, Codex, or Grok) drives the browser through `playwright-cli`. It
never places a real-money bet: when demo mode is missing, the game is reported as failing. Test
one game by name, or watch [Novedades](https://www.betsson.es/casino/novedades) and test each game
that appears there. Every test is stored in SQLite, and the dashboard lists the games with the
outcome of their latest test and shows every test step by step, with its screenshots, video,
console messages and failed requests.

## Install

Requires Rust 1.89 or newer. Running tests also requires Node.js 20 or newer and at least one
agent CLI: `claude`, `codex`, or `grok`.

Inside the repository, rustup uses the toolchain pinned in `rust-toolchain.toml`, Rust 1.99 with
Clippy and rustfmt, and installs it the first time `cargo` runs there, so that `cargo clippy` and
`cargo fmt` flag the same lints on every machine. Rust 1.89 is the minimum supported version:
build with it through `cargo +1.89 build`, or with a toolchain that rustup does not manage.

```sh
cargo build --release
npm install -g @playwright/cli@0.1.22
playwright-cli install-browser chromium
```

The prompt is written for the commands of `playwright-cli` 0.1.22, the version
`PLAYWRIGHT_CLI_VERSION` in `src/browser.rs` names. Before each test, testson reads
`playwright-cli --version` and warns when it is another. The agent opens the Chromium installed
above, not the system Google Chrome, so Chrome is not needed. On a Linux server, add `--with-deps`
to `install-browser` to install the libraries Chromium needs.

On Windows, the commands are looked up with the extensions in `PATHEXT`, and a native `.exe`
anywhere on `PATH` is used before an npm `.cmd` shim. `claude` and `codex` read the prompt from
standard input, so their npm shims work. `grok` takes the prompt as an argument, which Windows
cannot pass to a `.cmd` shim when it spans several lines, so a `grok` test stops with an error
that says so unless `grok` has a native `.exe` on `PATH`.

The examples below use `cargo run --`. Run them from the root of the repository: the data
directory, `data-dir/` by default, is found relative to the directory a command starts in. Every
command takes `--data-dir` (or `TESTSON_DATA_DIR`) to keep the data elsewhere. Its path must not
contain quotes, `$`, backticks, line breaks or a doubled backslash: the agent gets it inside
quoted shell commands. For a network share, map it to a drive letter instead of using a
`\server\share` path.

## Dashboard

```sh
cargo run -- serve
```

Then open [`http://127.0.0.1:8000`](http://127.0.0.1:8000). The first run creates `data-dir/`
with an empty database. The server only reads: `test` and `watch` write the games and the tests,
and can run while it serves.

The pages in `web/` are compiled into the binary, so the server finds them wherever it starts.
After editing one, restart `cargo run -- serve`, which builds it again.

## Session

`data-dir/session-block.txt` is the only copy of the login, and testson never puts it in the
prompt. Each test copies its statements into a private `session.js` in its run directory and
writes a `restore-session.js` that refers to it only by path. The agent runs that script with
`playwright-cli run-code --filename`: it loads `session.js` into the page as an init script and
reloads. playwright-cli prints the code it runs, which shows the path of `session.js` but no
token. So the tokens are not in the prompt, on any command line, or in the output of the restore,
and the model log does not get them from the restore. The browser holds them once restored,
though: an agent that asks it for them, with `localstorage-list` or by reading request headers,
or that reads `session.js`, would still see them, and only the rules in the prompt forbid that.

The file is read first, and the run stops before pinging or calling the agent while any
`REPLACE_WITH_` placeholder remains. If the file is missing, `test` continues without a login,
the restore script tells the agent so, and the test should come back as `test_failed`; `watch`
stops instead, since no queued game could be tested.

On [betsson.es](https://www.betsson.es), while logged in, paste this into the DevTools console. It
copies the four `localStorage.setItem` lines.

```javascript
(() => {
  const keys = ["authReducer", "session", "authSession", "customerId"];
  const missing = keys.filter((key) => localStorage.getItem(key) == null);
  if (missing.length) {
    throw new Error(
      "Missing localStorage keys: " + missing.join(", ") +
      ". Stay on https://www.betsson.es, log in, and run this again."
    );
  }
  const text = keys.map((key) => {
    const escaped = localStorage.getItem(key)
      .replace(/\\/g, "\\\\")
      .replace(/'/g, "\\'")
      .replace(/\r/g, "\\r")
      .replace(/\n/g, "\\n");
    return `localStorage.setItem('${key}', '${escaped}');`;
  }).join("\n");
  copy(text);
  return "Copied 4 lines. Paste them into data-dir/session-block.txt.";
})();
```

Paste the copied lines into `data-dir/session-block.txt`. Lines starting with `//` or `#` are
comments. Refresh it when the site asks you to log in again, or when the demo button no longer
appears after hover.

Once the restore script has reloaded the page, the agent runs `check-session.js`. Once the page
settles, it reads the logged-in and logged-out controls in the site header until the answer is
stable, and the agent confirms a logged-out answer with a snapshot. Demo buttons only appear
while logged in, so when the login cannot be confirmed the test ends as `test_failed` and asks
you to refresh the file, instead of reporting the game as `failing`.

## Test one game

```sh
cargo run -- test "Los Piratas Golfetes"
cargo run -- test "Los Piratas Golfetes" --url "https://www.betsson.es/casino"
cargo run -- test "Los Piratas Golfetes" --harness codex --model gpt-5.5
cargo run -- test "Aviator" --game-id nyxAviator
```

The game is looked up by name in the public games catalogue, which gives its identifier, provider
and image, and the stored game is brought up to date with them. The name is matched ignoring
case, but a game spelt exactly as given wins over one that differs in case. When several games of
the catalogue still share the name, the command stops and lists their identifiers: pick one with
`--game-id`, which looks the game up by its identifier instead, in the catalogue or else among
the stored games.

When the catalogue cannot be read, or does not have the game, the stored game with that name is
tested as it is, and its provider and image are kept. Only when no stored game has the name
either is a new one stored, under an identifier made from its name and without a provider or an
image. If another game is already stored under that identifier, the command stops and asks for
`--game-id` instead. The warnings say which of these happened.

The URL defaults to `https://www.betsson.es/casino` and must be `https` on `www.betsson.es` or
`betsson.es`, the market the session is for, since the login is injected into that site.

The command prints the status, summary, video, the report and the address of the test on the
dashboard, and where the Playwright trace was saved. It exits 0 only when the game is `working`
and the agent CLI exited cleanly.

The trace records the restored login, so it stays in the private run directory and the dashboard
never links it. Open it locally with `npx playwright show-trace data-dir/runs/<run>/trace.zip`.

A test ends in one of three statuses, as `schema/test-game.schema.json` defines them:

| Status        | Meaning                                                                        |
| ------------- | ------------------------------------------------------------------------------ |
| `working`     | The game launched in demo mode and a full round completed                      |
| `failing`     | The game did not: no demo mode, or the round could not be completed            |
| `test_failed` | The test itself broke, such as an expired session; it says nothing of the game |

The report is read only from the agent's final answer, which must be one JSON object; a draft it
printed along the way does not count. When the agent returns no report, or one the dashboard
cannot show, the test is stored as `test_failed` with the reason, and the model log keeps what
the agent said. A report must follow the schema. A video or screenshot that is not a file of its
own run under `/data/assets/`, and a `game_url` that is not `https` on a `betsson` host, are
dropped from it with a warning, so the verdict on the game is kept. A `trace_path` is always
dropped, since the trace stays private. Its dates are stored in UTC with milliseconds.

A `test_failed` test caused by a missing or expired login also has `error.reason` set to
`session`, so `watch` can tell it from a problem with the run itself.

## Watch Novedades

`watch` reads the New Games collection from the public games API. Listing does not need a login.
A demo test still needs `session-block.txt`.

```sh
cargo run -- watch --list
cargo run -- watch
cargo run -- watch --every 30m --harness codex --model gpt-5.5
cargo run -- watch --every 1h --max-attempts 5
```

`--list` prints the identifier and name of each game and does not save or test.

The first check stores the games already on Novedades and does not test them. Every check adds
the games to the dashboard, and a later check runs the same demo test as `test` for each game
that was not on Novedades at the check before. Games that leave Novedades are reported and stay on
the dashboard.

New games wait in a queue, tested in the order they were found. Each try counts one attempt
before the agent starts, so a run that is killed half way still counts:

- A game whose test is `working` or `failing` leaves the queue, even when the agent CLI then
  exits with an error: the verdict is stored.
- A test the agent ran that ends in `test_failed`, or a run that stored no test, keeps its attempt
  and keeps the game queued for the next check. After `--max-attempts` attempts (or
  `TESTSON_MAX_ATTEMPTS`, default 3) the game leaves the queue, and the check says so: test it by
  hand with `testson test "<name>"`. A game that already has them, from runs killed half way,
  leaves the queue without being tried.
- A test the environment stopped says nothing about the game and gives its attempt back. That is
  a test that stops before the agent runs it (session file missing or with placeholders, a
  missing tool, a failed ping, a run directory that cannot be written), or one the agent stored
  as `test_failed` with `error.reason` `session` because the login was missing or expired. Fix
  the environment, usually by refreshing `session-block.txt`, before the next check.
- The first test without a verdict ends the testing of that check. What broke it, such as an
  expired session, would most likely break the rest, and each try costs a full agent run. The
  games after it stay queued untried, with no attempt counted, for the next check.

A check that leaves a test without a verdict exits with an error.

`--every` repeats the check until you stop it. The interval is at least 60 seconds (`30m`, `1h`,
or a number of seconds). One watch runs at a time per data directory.

## Harness and model

`test`, `ping` and `watch` take `--harness`, `--model`, and `--effort`, or the `TESTSON_HARNESS`,
`TESTSON_MODEL`, and `TESTSON_EFFORT` environment variables.

| Harness            | Default model              | `--effort`                         |
| ------------------ | -------------------------- | ---------------------------------- |
| `claude` (default) | `sonnet`                   | yes, default `medium`              |
| `codex`            | the CLI's configured model | yes, sets `model_reasoning_effort` |
| `grok`             | the CLI's configured model | yes, sets `--reasoning-effort`     |

The agent runs in the private directory of its run, with the permissions [Safety](#safety)
describes. `claude` and `codex` read the prompt from standard input, so it does not show in
process listings; `grok` takes it on the command line (`--single`), the only way its CLI is known
to accept one. Before the full prompt is sent, the harness is pinged with a one-word prompt and
the same model. If that
fails (the CLI is logged out, the account cannot use the model, or no answer arrives within two
minutes), the test does not start. Check a combination on its own with:

```sh
cargo run -- ping --harness codex --model gpt-5.5
```

`test` and `watch` also take `--timeout` (or `TESTSON_TIMEOUT`): how long one run may take, 20
minutes by default and at least 60 seconds (`20m`, `1h`, or a number of seconds). A run that
takes longer is stopped, together with every process the agent started, and stored as
`test_failed`. However the run ends, testson then closes the browser session the agent opened with
`playwright-cli --session <name> close`. Ctrl+C during a run stops the agent in the same way, and
testson exits once the browser is closed and the test is stored as `test_failed`. Output is read for
at most two seconds after the agent exits, so a background process it left holding its output
cannot keep testson waiting.

Adding another agent CLI is one `Harness` variant in `src/harness.rs`, plus its arguments in
`Agent::command`.

The browser shows a window, so a test can be watched. `test` and `watch` take `--headless`, or
`TESTSON_HEADLESS=true`, to run it without one. On Linux with neither `DISPLAY` nor
`WAYLAND_DISPLAY` set there is no display to show a window on, so the browser runs headless and
the command says so. The video and the screenshots are recorded either way. A headless browser
sends a `HeadlessChrome` user agent, which the site or a game provider may treat differently, so
a headed run is the reference when the two disagree.

## Safety

The prompt tells the agent to stay in demo mode, but the agent reads pages served by the casino and
the game providers, which can carry instructions of their own, and its prompt holds the tokens of
a real account. A model that follows such instructions, or simply makes a mistake, can act with
that account and with everything the operating-system user running testson can reach.

Nothing in testson contains that. The real defences are outside it:

- Log in with a Betsson account kept for testing, with a balance of zero, a deposit limit of zero,
  and no card or other payment method saved. This is the only mitigation that holds whatever the
  agent does with the session.
- Run testson in a container or a virtual machine that holds no other credentials (no SSH keys,
  cloud logins, password stores or browser profiles) and, ideally, cannot see the host's files.
  The agent CLI's own login lives there too, so treat it as exposed.

The agent needs `playwright-cli`, and `playwright-cli` is enough to do anything. Its `run-code`
command, which the test uses to restore the session, runs JavaScript in the Node.js process of the
Playwright server, not in the page: playwright-core itself calls it "RCE-equivalent". The code gets
`fetch` and `Buffer`, and reaches `process` through the `page` object, so it can start programs,
read any file the user can read and send it anywhere. `eval` runs JavaScript in the page with the
session, and the other commands can open any address, click any control and upload files. With the
session, the agent can therefore still deposit or bet, and an injected instruction can still run
code as the user with network access.

What each harness restricts on top of that only reduces accidental damage, and keeps the agent off
its own web and file tools:

- `claude` runs in `dontAsk` mode. A test may run shell commands that start with
  `playwright-cli`, `echo`, `date` or `ls`, and read files in its run and evidence directories
  with the `Read` tool, such as the screenshots it took. It may not copy or move files: the
  browser writes the evidence where it belongs. Every other call that needs approval is denied
  without asking. Web fetch and search, file edits, subagents, MCP servers, Chrome and the
  PowerShell tool are off, so on Windows Claude Code needs Git Bash. Claude Code still runs its
  built-in read-only commands, such as `cat`, `grep` and `find`, without asking and on any path,
  and allow rules from your own Claude Code settings still apply and widen the list.
- `codex` may do everything: no sandbox and no approvals, as before. Only web search is off. None
  of its sandbox modes is known to let `playwright-cli` work: `workspace-write` turns the network
  off by default, and the browser and its daemon write outside the workspace, under the system
  temporary directory and `ms-playwright` in the user's local data.
- `grok` may do everything: every permission is bypassed.

`claude --restricted` also ignores the user, project and local settings files, so no allow rule of
yours can widen the list. testson does not pass it, because a login or API key helper configured
in those settings would stop working; it is worth trying where the login does not depend on them.

## How it fits together

| Path                           | What it is                                                      |
| ------------------------------ | --------------------------------------------------------------- |
| `src/main.rs`                  | The CLI and the start of the server                             |
| `src/router.rs`                | The routes: the JSON API, the pages and the assets              |
| `src/db.rs`                    | The migrations and every query                                  |
| `src/catalog.rs`               | The public games catalogue and its Novedades collection         |
| `src/watch.rs`                 | The watch over Novedades, its queue and its lock                |
| `src/run.rs`                   | One test: its directories, the agent run and the stored test    |
| `src/harness.rs`               | The agent CLIs, model selection and the ping                    |
| `src/browser.rs`               | The browser the agent opens and the `playwright-cli` version    |
| `src/events.rs`                | Readable progress from the claude and grok event streams        |
| `src/prompt.rs`                | Filling the prompt, the session restore and the URL checks      |
| `prompts/test-game.txt`        | The prompt sent to the agent, compiled into the binary          |
| `prompts/check-session.js`     | The login check the agent runs after restoring the session      |
| `migrations/`                  | The SQL migrations, compiled into the binary                    |
| `schema/test-game.schema.json` | The JSON Schema of a test, also compiled into the prompt        |
| `web/`                         | The pages: plain HTML, CSS and JavaScript, compiled in as is    |

A test is stored whole as a JSON document that follows the schema. The few fields the lists need,
its status and when it started, are columns SQLite generates from the document. A game goes by
the text identifier the catalogue gives it, such as `nyxAviator`, and a test by a number. The
tests are ordered by that number, so the latest test of a game is the one stored last: the start
time comes from the agent, which may get it wrong, and is only shown.

The data directory holds:

```text
data-dir/
  testson.db              games, tests, and the watch queue
  session-block.txt       the login, never served
  watch.lock
  runs/<game>-<time>/     private: prompt.filled.txt, session.js, restore-session.js,
                          check-session.js, stop-trace.js, model.log, report.json, trace.zip,
                          .playwright-cli/, from-assets/
  assets/<game>-<time>/   served at /data/assets/: session.webm, screenshots/
```

A run whose `<game>-<time>` is taken, because another run of the game started in the same
second, gets `-2`, `-3`, and so on after it. Each run drives a playwright-cli session named
after its directory, so two runs in one data directory never share files or a browser.

After each run, any file in its assets that is not a video or an image, such as a trace the agent
copied there, is moved into `from-assets/` in its run directory, and the test stops naming it.

Older versions saved the trace as `data-dir/assets/<game>-<time>/trace.zip`. The dashboard no
longer serves it, but it holds the login session: delete those traces, and refresh
`session-block.txt` if the dashboard was reachable from other machines while it served them.

| Route                                 | Responds with                                                 |
| ------------------------------------- | ------------------------------------------------------------- |
| `/`, `/index.html`                    | The list of games                                             |
| `/game/{game_id}`                     | The page of a game, or the 404 page when no game has that id  |
| `/common.css`, `/common.js`           | The styles and the script every page shares                   |
| `/index.css`, `/index.js`             | The styles and the script of the list of games                |
| `/game.css`, `/game.js`, `/game.html` | The styles, the script and the bare page of a game            |
| `/404.html`                           | The page for what does not exist                              |
| `/api/games`                          | Every game with its latest test                               |
| `/api/game/{game_id}`                 | A game and the summaries of its tests, or 404                 |
| `/api/test/{test_id}`                 | The JSON document of a test, or 404                           |
| `/data/assets/...`                    | The videos and images in `data-dir/assets/`, or 404           |
| Any other path                        | The page for what does not exist, with status 404             |

The pages, their scripts and their styles are sent with `Cache-Control: no-cache` and an `ETag`
made from their contents, so a browser never runs a page against the script or the styles of an
older version, and gets `304 Not Modified` instead of the file when its copy is current. Every
response says `X-Content-Type-Options: nosniff`, and the pages get a Content Security Policy that
lets them load only their own scripts and styles, the images of the games over `https`, and the
assets.

Of the assets, only `.webm` videos and `.png`, `.jpg`, `.jpeg`, `.webp` and `.gif` images are
served, each with a fixed type and a policy that keeps it from running anything. Any other file,
a directory, a path through a symbolic link or a junction, or, on Unix, a file with another hard
link is not found.

