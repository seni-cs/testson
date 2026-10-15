# testson

Tests every Pragmatic Play game of the Betsson casino with two test accounts, and shows the
results on a dashboard.

Each game is tested twice, in real-money mode, never in demo mode:

| Mode         | Account        | What it checks                                                      |
| ------------ | -------------- | ------------------------------------------------------------------- |
| `real-money` | Has balance    | One round at the minimum stake completes and the balance follows it |
| `no-balance` | Has no balance | Trying to bet shows a message that the balance is not enough        |

An AI coding agent (Claude Code, Codex, or Grok) drives the browser through `playwright-cli`. It
spends real money in the real-money test: one spin at the minimum stake, never above 1 €, with no
autoplay, bonus buy or deposit. When Betsson asks for the gaming session Spanish rules require
before a real-money game loads, the agent starts one of 15 minutes with a 1 € loss limit. Test one
game by name, or `watch` the catalogue and test every Pragmatic Play game in both modes. Every test
is stored in SQLite, and the dashboard lists the games with the outcome of their latest test in
each mode and shows every test step by step, with its screenshots, video, console messages,
failed requests, and the postMessages between the lobby and the game.

The agent tests each game once per version of it. While it tests, it writes a replay script: the
`playwright-cli` commands that worked. testson replays that script itself, without an agent and so
without model tokens, until the game changes. See [Scripts](#scripts).

## Install

Requires Rust 1.89 or newer. Running tests also requires Node.js 20 or newer and at least one
agent CLI: `claude`, `codex`, or `grok`.

```sh
cargo build --release
npm install -g @playwright/cli@0.1.22
playwright-cli install-browser chromium
```

The prompt is written for the commands of `playwright-cli` 0.1.22. The agent opens the Chromium
installed above, so Google Chrome is not needed.

On Windows, the agent CLIs and `playwright-cli` are found with the extensions in `PATHEXT`, so
their npm `.cmd` shims work. `claude` and `codex` read the prompt from standard input; `grok`
takes it as an argument, which a `.cmd` shim cannot pass, so it needs a native `grok.exe`.

The examples below use `cargo run --`. The data directory, `data-dir/` by default, is found
relative to the directory a command starts in; every command takes `--data-dir` (or
`TESTSON_DATA_DIR`) to keep it elsewhere. The pages are compiled into the binary.

## Dashboard

```sh
cargo run -- serve
```

Then open [`http://127.0.0.1:8000`](http://127.0.0.1:8000). The first run creates `data-dir/`
with an empty database. The server only reads: `test` and `watch` write the games and the tests,
and can run while it serves.

## Session

Each mode plays with its own account. `testson login` logs in to each one and saves its cookies
and localStorage, which every test of that mode then restores:

```sh
cargo run -- login
cargo run -- login --mode no-balance
```

Betsson blocks the login in the browser Playwright launches, so the login runs in Google Chrome
instead. testson starts it with a profile of the account's own, `data-dir/browser-<mode>/`, and a
free DevTools port, attaches `playwright-cli` to that port, and logs in with the scripts in
`scripts/`, without an agent: it opens the login form, types the username and password, submits
it, and waits for the login. An open
DevTools port makes Chrome set `navigator.webdriver`, which Betsson's fraud check refuses, so
testson starts Chrome with `--disable-blink-features=AutomationControlled`. The fraud check
remembers a refused browser by its profile: if the login keeps failing with "Actualiza la página y
vuelve a intentarlo en breve", delete `data-dir/browser-<mode>/` and log in again. Once the page
holds the login, testson saves the state of the browser and closes Chrome; a page that is already
logged in is only saved again. If the site asks for a captcha or a code sent by SMS or email,
complete it in the Chrome window: testson waits up to five minutes for it. A login takes about 15
seconds.

The login reads the username and password of each account from its credentials file:

| Mode         | Credentials file                      | Saved session                      |
| ------------ | ------------------------------------- | ---------------------------------- |
| `real-money` | `data-dir/credentials-real-money.env` | `data-dir/session-real-money.json` |
| `no-balance` | `data-dir/credentials-no-balance.env` | `data-dir/session-no-balance.json` |

```sh
BETSSON_USERNAME=user@example.com
BETSSON_PASSWORD=the password
```

testson passes the credentials to `playwright-cli` as two secrets and removes the file that held
them before the login starts. They are typed by name, `BETSSON_USERNAME` or `BETSSON_PASSWORD`, so
no output, snapshot, or file of the run holds their values.

To log in on a browser of your own instead, start it with remote debugging and a profile that is
not your everyday one, with the same flag, and pass its endpoint with the mode of the account it
should log in to:

```sh
"/Applications/Google Chrome.app/Contents/MacOS/Google Chrome" \
  --remote-debugging-port=9222 --user-data-dir="$HOME/.testson-chrome" \
  --disable-blink-features=AutomationControlled
cargo run -- login --mode real-money --cdp http://127.0.0.1:9222
```

testson then leaves that browser open. Chrome is found in its usual place; `--chrome` (or
`TESTSON_CHROME`) gives another path.

Each test copies the saved session of its mode into its private run directory as `session.json`,
and the agent loads it with `playwright-cli state-load` by its path, so the login is never in the
prompt. If the saved session is missing, the run continues without a login and the test should
come back as `test_failed`. Run `login` again when a test says the session expired. A swapped
account is caught too: the real-money test stops when the balance is zero, and the no-balance test
stops, before opening the game, when the account has balance.

## Test one game

```sh
cargo run -- test "Big Bass Blast"
cargo run -- test "Big Bass Blast" --mode no-balance
cargo run -- test "Big Bass Blast" --url "https://www.betsson.es/casino"
cargo run -- test "Big Bass Blast" --harness codex --model gpt-5.5
cargo run -- test "Big Bass Blast" --runner agent
```

Both modes run, one after the other, unless `--mode` picks one. `--runner` (or `TESTSON_RUNNER`)
picks what tests the game: `auto`, the default, replays its script and falls back to the agent;
`agent` always runs the agent; `script` only replays, and fails when there is no script.

The game is looked up by name in the public games catalogue, which gives its identifier, provider
and image. A game spelt exactly as given wins over one that only differs in case. When the
catalogue cannot be read or does not have the game, the stored game with that name is tested as it
is, and only when there is none is a new one stored, under an identifier made from its name. The
URL defaults to `https://www.betsson.es/casino` and must be `https` on a `betsson` host.

For each test it runs, the command prints the runner, status, summary, version, video, the
report, where the Playwright trace is and the address of the test on the dashboard. At the end it
prints how long the command took and what its agent runs cost, as the `claude` CLI reports it. It
exits 0 only when, in every mode, the latest test that reached a verdict is `working` and the agent
CLI exited cleanly.

A run may take 20 minutes. One that takes longer is stopped and stored as `test_failed`. Either
way, testson then closes the browser the agent opened.

The trace records the restored login, so it stays in the private run directory and the dashboard
never links it. Open it locally with `npx playwright show-trace data-dir/runs/<run>/.playwright-cli/traces/<file>.trace`.

A test ends in one of three statuses, as `schema/test-game.schema.json` defines them:

| Status        | Meaning                                                                          |
| ------------- | -------------------------------------------------------------------------------- |
| `working`     | The game did what the mode expects: a round completed, or the bet was refused    |
| `failing`     | The game did not: it did not launch, the round broke, or no message appeared     |
| `test_failed` | The test itself broke, such as an expired session or the wrong account           |

Every report is validated against the schema. When the agent returns no report, or one that does
not follow the schema, the test is stored as `test_failed` with the reason, and the model log keeps
what the agent said.

## Scripts

A script is a JSON document that follows `schema/test-script.schema.json`. It holds the steps that
are particular to the game: read the balance in the lobby, open the game, fill the gaming session,
wait for it to load, read the stake, spin, and read or expect what the mode checks. testson opens
the browser, records the video, captures the postMessages, loads the saved login and collects the
evidence itself, so the login is never in a script.

The version of a game is the ETag the server of the provider sends with its `build.js`. A script
belongs to one game, one mode and one version:

1. A game and mode without a script is tested by the agent.
2. When the agent finds it `working`, the `script.json` it wrote is checked and stored, and
   replayed at once as a test of its own. In real-money mode that is a second round.
3. Later tests replay the script. Before the spin, the replay checks that the `build.js` the
   browser loaded has the ETag of the script, and stops before betting when it does not.
4. A replay that stops because a step no longer runs, or because the version changed, is stored
   as `test_failed`. Its script is retired, and the agent tests the game again.

Every test, by the agent or by a script, is stored on its own, with `runner` saying which. A replay
that ran every step is `working`, or, in real-money mode, `failing` when the balance after the
round is not the balance before it less the stake plus the win, to the cent. Any other step that
does not hold, such as a message that does not appear, may be the script as much as the game, so
the agent tests the game and gives the verdict.

The script was written by a model and runs unattended, so it is checked before every replay:

- Only page commands, such as `click`, `fill`, `mousedown` or `find`, are allowed. `run-code` is
  not, and an `eval` may only read.
- It spins once at most, and exactly once with real money, and acts on nothing after the spin.
  Only the spin may press Space, which spins most games.
- It reads the balance before doing anything, and the stake before the spin. The replay stops
  before betting when the stake is not between 0 and 1 €, or is above the balance, or when the
  account is not the one of the mode.
- It navigates only to Betsson, types no number above 5, and names no cashier, deposit, demo,
  bonus or buy control.

The game page shows who ran each test, IA or Script, the version it saw, and a history of the
game: its first test, its new versions, the scripts created and retired, and its status changes.

## Watch the Pragmatic Play games

`watch` reads the games whose provider is `PragmaticPlay` from the public games API, about 300 of
them. Listing does not need a login. The tests need both sessions: when the session of a mode is
missing, or a test says it expired, the watch runs `testson login` for that mode first, and tests
the game again after an expired one.

```sh
cargo run -- watch --list
cargo run -- watch
cargo run -- watch --every 30m --harness codex --model gpt-5.5
```

`--list` prints the identifier and name of each game and does not save or test.

To put every game on the dashboard without testing any, run `cargo run -- sync`.

Every check adds the games to the dashboard and runs the same tests as `test` for each game and
mode that has no `working` or `failing` test yet for the current version of the game. The version
is read with a HEAD request for the `build.js` the latest test of the game loaded, without a
browser, and each test records the version read this way before it ran. A game is tested again
only when its ETag changed: its script finds the new version before betting and hands over to the
agent, which leaves a script for it. A game whose version cannot be read keeps its verdict. A test
that ends in `test_failed` is tried again at the next check, up to three attempts in that mode and
version; a replay that hands over to the agent is not an attempt. After that the watch leaves it
alone, and you can test it by hand with `testson test`. A test that cannot start, such as with a
saved session that is not valid JSON or a tool missing, stops the check without counting an
attempt. A game
that leaves the catalogue stays on the dashboard. The first check tests every game in both modes,
which spends one minimum stake per game on the real-money account. Run `test` to test a game
again.

`--limit` tests at most that many games per check, the games new to the dashboard first, and leaves
the rest for the next check:

```sh
cargo run -- watch --limit 1
```

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

The agent runs with every permission bypassed, in the private directory of its run. Before the
full prompt is sent, the harness is pinged with a one-word prompt and the same model. If that
fails (the CLI is logged out, the account cannot use the model, or no answer arrives within two
minutes), the test does not start. Check a combination on its own with:

```sh
cargo run -- ping --harness codex --model gpt-5.5
```

Adding another agent CLI is one `Harness` variant in `src/harness.rs`, plus its arguments in
`Agent::command`.

## How it fits together

| Path                           | What it is                                                      |
| ------------------------------ | --------------------------------------------------------------- |
| `src/main.rs`                  | The CLI and the start of the server                             |
| `src/router.rs`                | The routes: the JSON API, the pages and the assets              |
| `src/db.rs`                    | The migrations and every query, grouped by table                |
| `src/catalog.rs`               | The public games catalogue and the games of Pragmatic Play      |
| `src/watch.rs`                 | The watch over those games and its lock                         |
| `src/run.rs`                   | One test in one mode: the agent or the replay, and the fallback |
| `src/script.rs`                | The rules of a replay script and the replay that runs one       |
| `src/login.rs`                 | The login in Chrome over its DevTools port, and the saved state |
| `scripts/login-*.js`           | The steps of the login, run with `playwright-cli run-code`      |
| `src/report.rs`                | The validation of a report against the schema                   |
| `src/harness.rs`               | The agent CLIs, model selection and the ping                    |
| `src/events.rs`                | Readable progress from the claude and grok event streams        |
| `src/prompt.rs`                | Filling the prompts, the saved sessions and the URL checks      |
| `prompts/test-game.txt`        | The prompt sent to the agent, compiled into the binary          |
| `prompts/real-money.txt`       | The rules and criteria of each mode, put into that prompt       |
| `prompts/no-balance.txt`       |                                                                 |
| `migrations/`                  | The SQL migrations, compiled into the binary                    |
| `schema/test-game.schema.json` | The JSON Schema of a test, also compiled into the prompt        |
| `schema/test-script.schema.json` | The JSON Schema of a replay script, also in the prompt        |
| `prompts/capture-messages.js`  | Captures the postMessages of the page; the agent and the replay run it |
| `prompts/collect-messages.js`  | Reads them back, with tokens redacted                           |
| `clippy.toml`                  | Lets the docs write ETag as the header writes it                |
| `web/`                         | The pages: plain HTML, CSS and JavaScript, compiled in as is    |

A test is stored whole as a JSON document that follows the schema. The few fields the lists need,
its mode, runner, status, version and when it started, are columns SQLite generates from the
document. Tests stored before version 3 of the schema were run by the agent. Tests
stored before the modes, with version 1 of the schema, were played in demo mode and show as Demo. A
game goes by the text identifier the catalogue gives it, such as `nyxAviator`, and a test by a
number.

The data directory holds:

```text
data-dir/
  testson.db                  games, tests and scripts
  credentials-<mode>.env      the username and password of the account, never served
  session-<mode>.json         the saved login of the account, never served
  browser-<mode>/             the Chrome profile `login` uses, never served
  watch.lock
  runs/login-<mode>-<time>/     private: the login scripts and .playwright-cli/
  runs/<game>-<mode>-<time>/    private: prompt.filled.txt, session.json, the two postMessage
                                scripts, model.log, script.json, report.json,
                                .playwright-cli/traces/
  assets/<game>-<mode>-<time>/  served at /data/assets/: session.webm, screenshots/
```

| Route                 | Responds with                                                |
| --------------------- | ------------------------------------------------------------ |
| `/`                   | The list of the stored games                                 |
| `/game/{game_id}`     | The page of a game                                           |
| `/api/games`          | Every stored game, with its latest test of each mode         |
| `/api/game/{game_id}` | A game and the summaries of its tests and scripts            |
| `/api/test/{test_id}` | The JSON document of a test                                  |
| `/data/assets/...`    | The videos and images in `data-dir/assets/`                  |
