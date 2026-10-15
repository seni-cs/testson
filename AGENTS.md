# AGENTS

testson tests every Pragmatic Play game of the Betsson casino with two test accounts, one with
balance that plays a real-money round and one without balance that must be told it cannot bet, with
an AI coding agent that drives `playwright-cli`, and shows the results on a dashboard: a CLI that
runs the agent and writes to SQLite, an axum server that reads it, and pages in plain HTML, CSS and
JavaScript. The agent tests each version of a game once and leaves a replay script, which testson
runs without an agent until the version, the ETag of the game's `build.js`, changes. See `README.md` for the layout and the routes.

## Checks

Clippy runs with the pedantic group denied, so its warnings are errors. Fix them instead of
allowing them.

The tests never call an agent or the live site: the catalogue is read through the `Fetcher`
trait, which they replace. Running a real test spends model tokens and real money and needs valid
session files, so do it only when asked.

For a change to the pages or the routes, start the server with `cargo run -- serve` from the root
of the repository and look at the result at <http://127.0.0.1:8000>. The pages are compiled into
the binary, so restart it after each edit to them.

## Design

- The server only reads. The `test` and `watch` commands write games and tests, straight through
  `src/db.rs`, and can run beside it.
- A replay script follows `schema/test-script.schema.json`, which is also compiled into the prompt
  and mirrored by the types and the rules in `src/script.rs`; change them together, as for a test.
  `script::validate` checks the safety rules below before every replay, because the script was
  written by a model and runs unattended. Loosen them only with the same care as the prompts.
- A test is one JSON document that follows `schema/test-game.schema.json`. The `status` and
  `started_at` columns of `tests` are generated from it, so the document is the only place they
  are written. A change to the shape of the document starts in the schema. The schema is also
  compiled into the prompt in `prompts/test-game.txt`, so the agent sees the same one; keep the
  prompt's instructions and example in step with it, and the types in `src/report.rs`, which
  validate every report against it. The rules of each mode are in `prompts/real-money.txt` and
  `prompts/no-balance.txt`.
- Migrations are numbered files in `migrations/`, listed in `MIGRATIONS` in `src/db.rs` and
  tracked with `user_version`. Add a new one instead of editing one that was already applied unless
  changes are not yet committed.
- Every SQL query lives in `src/db.rs`, grouped by table. The handlers in `src/router.rs` do not
  touch the database themselves.
- `web/` has no build step and no dependencies. `common.js` and `common.css` hold what the pages
  share; each page has a script and a stylesheet of its own beside them. `src/router.rs` compiles
  every file in it into the binary, so a new file there needs its own line in `PAGES`.
- `data-dir/` is ignored by git. It holds the database, the assets, the private run directories,
  and for each of the two accounts its credentials (`credentials-<mode>.env`), its saved session
  (`session-<mode>.json`, a Playwright storage state) and the Chrome profile `testson login` uses
  (`browser-<mode>/`). Only `data-dir/assets/` is served: nothing that holds the session, such as
  its copy in a run directory or the Playwright trace, may be written there, and only the videos
  and images in `ASSET_EXTENSIONS` in `src/router.rs` are served from it.
- The login never goes into the prompt. A test loads the saved session with `state-load` by its
  path. `testson login` runs in Google Chrome, reached over its DevTools port, because Betsson
  blocks the login in the browser Playwright launches, with
  `--disable-blink-features=AutomationControlled`, without which the port alone sets
  `navigator.webdriver` and Betsson's fraud check refuses the login. The login takes no agent:
  testson runs the scripts in `scripts/` with `run-code`, and types the credentials with `fill` as
  `playwright-cli` secrets, so no output or file holds them; the file that holds them is removed
  before the first step. `run-code` prints the code it runs, so never put a secret in a script.
- The agent never plays in demo mode, never deposits, withdraws or opens the cashier, and presses
  spin once per test at the minimum stake, never above 1 €. The no-balance test never bets on an
  account with balance. The only limit it sets is the gaming session Spanish rules require before
  a real-money game loads: 15 minutes and a 10 € loss limit. Changes to the prompts must keep these
  rules, and so must changes to `src/script.rs`: a replay spins once at most, with a single press,
  right after reading a stake it checks is at most 1 € and with no action in between, acts on
  nothing after the spin, does not hand over to the agent in the same test once it spun, stops
  before betting when the version of the game changed, and never runs `run-code` from a script.

## Conventions

- Code, comments and documentation are in English. The text the pages show is in English too.
- Rust items keep their doc comments. Any other comment only says what the code cannot: a
  reason, a constraint of the site or a tool, a workaround. Do not add comments that restate the
  code, name a section, or describe a change; a well-named function or selector needs none.
