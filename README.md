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
each mode and shows every test step by step, with its screenshots, video, console messages, and
failed requests.

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

Each mode logs in with its own account, from its own file in the data directory:

| Mode         | Session file                      |
| ------------ | --------------------------------- |
| `real-money` | `data-dir/session-real-money.txt` |
| `no-balance` | `data-dir/session-no-balance.txt` |

The file is the only copy of that login. Each test of a mode copies its statements into a private
`restore-session.js` in its run directory, which the agent runs by its path, so the login is never
in the prompt. The run stops before calling the agent while any `REPLACE_WITH_` placeholder
remains.
If the file is missing, the run continues without a login and the test should come back as
`test_failed`. A swapped file is caught too: the real-money test stops when the balance is zero,
and the no-balance test stops, before opening the game, when the account has balance.

On [betsson.es](https://www.betsson.es), logged in with the account of the mode, paste this into
the DevTools console. It copies the four `localStorage.setItem` lines.

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
  return "Copied 4 lines. Paste them into the session file of this account.";
})();
```

Paste the copied lines into the session file of that account. Lines starting with `//` or `#` are
comments. Refresh it when the site asks you to log in again. Log out of one account before you log
in to the other, so the two files never hold the same session.

## Test one game

```sh
cargo run -- test "Big Bass Blast"
cargo run -- test "Big Bass Blast" --mode no-balance
cargo run -- test "Big Bass Blast" --url "https://www.betsson.es/casino"
cargo run -- test "Big Bass Blast" --harness codex --model gpt-5.5
```

Both modes run, one after the other, unless `--mode` picks one.

The game is looked up by name in the public games catalogue, which gives its identifier, provider
and image. A game spelt exactly as given wins over one that only differs in case. When the
catalogue cannot be read or does not have the game, the stored game with that name is tested as it
is, and only when there is none is a new one stored, under an identifier made from its name. The
URL defaults to `https://www.betsson.es/casino` and must be `https` on a `betsson` host.

For each mode, the command prints the status, summary, video, the report, where the Playwright
trace is and the address of the test on the dashboard. It exits 0 only when every test is
`working` and the agent CLI exited cleanly.

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

## Watch the Pragmatic Play games

`watch` reads the games whose provider is `PragmaticPlay` from the public games API, about 300 of
them. Listing does not need a login. The tests need both session files.

```sh
cargo run -- watch --list
cargo run -- watch
cargo run -- watch --every 30m --harness codex --model gpt-5.5
```

`--list` prints the identifier and name of each game and does not save or test.

To put every game on the dashboard without testing any, run `cargo run -- sync`.

Every check adds the games to the dashboard and runs the same tests as `test` for each game and
mode that has no `working` or `failing` test yet. A test that ends in `test_failed` is tried again
at the next check, up to three attempts in that mode; after that the watch leaves it alone, and you
can test it by hand with `testson test`. A test that cannot start, such as with a session file
still holding placeholders or a tool missing, stops the check without counting an attempt. A game
that leaves the catalogue stays on the dashboard. The first check tests every game in both modes,
which spends one minimum stake per game on the real-money account. Run `test` to test a game
again.

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
| `src/run.rs`                   | One test in one mode: its run, the agent and the stored test    |
| `src/report.rs`                | The validation of a report against the schema                   |
| `src/harness.rs`               | The agent CLIs, model selection and the ping                    |
| `src/events.rs`                | Readable progress from the claude and grok event streams        |
| `src/prompt.rs`                | Filling the prompt, the session restore and the URL checks      |
| `prompts/test-game.txt`        | The prompt sent to the agent, compiled into the binary          |
| `prompts/real-money.txt`       | The rules and criteria of each mode, put into that prompt       |
| `prompts/no-balance.txt`       |                                                                 |
| `migrations/`                  | The SQL migrations, compiled into the binary                    |
| `schema/test-game.schema.json` | The JSON Schema of a test, also compiled into the prompt        |
| `web/`                         | The pages: plain HTML, CSS and JavaScript, compiled in as is    |

A test is stored whole as a JSON document that follows the schema. The few fields the lists need,
its mode, its status and when it started, are columns SQLite generates from the document. Tests
stored before the modes, with version 1 of the schema, were played in demo mode and show as Demo. A
game goes by the text identifier the catalogue gives it, such as `nyxAviator`, and a test by a
number.

The data directory holds:

```text
data-dir/
  testson.db              games and tests
  session-real-money.txt  the login of the account with balance, never served
  session-no-balance.txt  the login of the account without balance, never served
  watch.lock
  runs/<game>-<mode>-<time>/    private: prompt.filled.txt, restore-session.js, model.log,
                                report.json, .playwright-cli/traces/
  assets/<game>-<mode>-<time>/  served at /data/assets/: session.webm, screenshots/
```

| Route                 | Responds with                                                |
| --------------------- | ------------------------------------------------------------ |
| `/`                   | The list of the Pragmatic Play games                         |
| `/game/{game_id}`     | The page of a game                                           |
| `/api/games`          | Every Pragmatic Play game, with its latest test of each mode |
| `/api/game/{game_id}` | A game and the summaries of its tests                        |
| `/api/test/{test_id}` | The JSON document of a test                                  |
| `/data/assets/...`    | The videos and images in `data-dir/assets/`                  |
