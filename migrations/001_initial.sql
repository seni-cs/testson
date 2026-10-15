-- A game as the catalogue of the site describes it, under the identifier the catalogue gives it.
-- `image_url` points at a square image.
CREATE TABLE games (
    id                   TEXT NOT NULL PRIMARY KEY,
    name                 TEXT NOT NULL,
    provider             TEXT NOT NULL,
    is_available_for_fun INTEGER NOT NULL CHECK (is_available_for_fun IN (0, 1)),
    image_url            TEXT NOT NULL,
    added_on             TEXT NOT NULL DEFAULT (date('now'))
) STRICT;

-- The fields the list views need are columns computed from the JSON document when it is stored,
-- so the document stays the only place where they are written and the lists never parse it.
-- The statuses are the ones `schema/test-game.schema.json` allows.
CREATE TABLE tests (
    id         INTEGER PRIMARY KEY,
    game_id    TEXT NOT NULL REFERENCES games(id),
    test       TEXT NOT NULL CHECK (json_valid(test)),
    status     TEXT NOT NULL GENERATED ALWAYS AS (json_extract(test, '$.status')) STORED
               CHECK (status IN ('working', 'failing', 'test_failed')),
    started_at TEXT NOT NULL GENERATED ALWAYS AS
               (strftime('%Y-%m-%dT%H:%M:%fZ', json_extract(test, '$.started_at'))) STORED
) STRICT;

CREATE INDEX tests_game_id_started_at ON tests(game_id, started_at);
