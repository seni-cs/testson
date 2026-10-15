-- Who ran a test, the version of the game it was run on and, for a replay, which script it
-- replayed and how far it got, from its document. Tests stored before version 3 of the schema were
-- all run by the agent and record no version.
ALTER TABLE tests ADD COLUMN runner TEXT NOT NULL
    GENERATED ALWAYS AS (coalesce(json_extract(test, '$.runner'), 'agent')) VIRTUAL
    CHECK (runner IN ('agent', 'script'));
ALTER TABLE tests ADD COLUMN etag TEXT
    GENERATED ALWAYS AS (json_extract(test, '$.game_version.etag')) VIRTUAL;
ALTER TABLE tests ADD COLUMN script_id INTEGER
    GENERATED ALWAYS AS (json_extract(test, '$.script_id')) VIRTUAL;
ALTER TABLE tests ADD COLUMN script_outcome TEXT
    GENERATED ALWAYS AS (json_extract(test, '$.script_outcome')) VIRTUAL;

-- The replay scripts agent runs left, each for one game, one mode and the version of the game
-- it was written for, as `schema/test-script.schema.json` describes them. A script stops being
-- replayed once it is retired: the game changed version, a step of it no longer ran, or a newer
-- agent run left another one. The mode and the version are columns computed from the script,
-- which stays the only place where they are written.
CREATE TABLE scripts (
    id              INTEGER PRIMARY KEY,
    game_id         TEXT NOT NULL REFERENCES games(id),
    script          TEXT NOT NULL CHECK (json_valid(script)),
    mode            TEXT NOT NULL GENERATED ALWAYS AS (json_extract(script, '$.mode')) VIRTUAL
                    CHECK (mode IN ('real_money', 'no_balance')),
    etag            TEXT NOT NULL
                    GENERATED ALWAYS AS (json_extract(script, '$.game_version.etag')) VIRTUAL,
    created_by_test INTEGER NOT NULL REFERENCES tests(id),
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ', 'now')),
    retired_at      TEXT,
    retired_reason  TEXT CHECK (retired_reason IN ('version_changed', 'step_failed', 'replaced')),
    CHECK ((retired_at IS NULL) = (retired_reason IS NULL))
) STRICT;

CREATE INDEX scripts_game_id_mode ON scripts(game_id, mode);
