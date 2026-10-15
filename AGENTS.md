# AGENTS

testson tests Betsson casino games in demo mode with an AI coding agent that drives
`playwright-cli`, and shows the results on a dashboard: a CLI that runs the agent and writes to
SQLite, an axum server that reads it, and pages in plain HTML, CSS and JavaScript. See `README.md`
for the layout and the routes.

## Checks

Clippy runs with the pedantic group denied, so its warnings are errors. Fix them instead of
allowing them.

The tests never call an agent or the live site: the catalogue is read through the `Fetcher`
trait, which they replace. Running a real test spends model tokens and needs a valid
`session-block.txt`, so do it only when asked.

For a change to the pages or the routes, start the server with `cargo run -- serve` from the root
of the repository and look at the result at <http://127.0.0.1:8000>. The pages are compiled into
the binary, so restart it after each edit to them.

## Design

- The server only reads. The `test` and `watch` commands write games and tests, straight through
  `src/db.rs`, and can run beside it.
- A test is one JSON document that follows `schema/test-game.schema.json`. The `status` and
  `started_at` columns of `tests` are generated from it, so the document is the only place they
  are written. A change to the shape of the document starts in the schema. The schema is also
  compiled into the prompt in `prompts/test-game.txt`, so the agent sees the same one; keep the
  prompt's instructions and example in step with it, and `check_report` in `src/run.rs` with
  what the pages rely on.
- Migrations are numbered files in `migrations/`, listed in `MIGRATIONS` in `src/db.rs` and
  tracked with `user_version`. Add a new one instead of editing one that was already applied unless
  changes are not yet committed.
- Every SQL query lives in `src/db.rs`. The handlers in `src/router.rs` do not touch the database
  themselves.
- `web/` has no build step and no dependencies. `common.js` and `common.css` hold what the pages
  share; each page has a script and a stylesheet of its own beside them. `src/router.rs` compiles
  every file in it into the binary with `include_str!`, so the server does not depend on the
  directory it starts in: a new file there needs its own line in `PAGES`.
- Handlers run their reads of the database through `spawn_blocking`, so a slow read does not hold
  up the async runtime.
- `data-dir/` is ignored by git. It holds the database, the assets, the private run directories
  and `session-block.txt`, the login. Only `data-dir/assets/` is served: nothing that holds the
  session, such as the `session.js` of a run, the filled prompt, the restore script or the
  Playwright trace, may be written there. Of it, only the videos and images in `ASSET_TYPES` in
  `src/router.rs` are served, and never through a link.
- The login never goes into the prompt or onto a command line: the model provider, its logs and
  process listings would keep it. The agent restores it by running `restore-session.js`, which
  loads the private `session.js` by path.
- The agent never places a real-money bet. Changes to the prompt must keep its demo-only rules.

## Conventions

- Code, comments and documentation are in English. The text the pages show is in Spanish.
