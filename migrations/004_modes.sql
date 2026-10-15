-- Every game of one provider is now tested twice, once per test account, instead of the new games
-- once in demo mode. Whether a game still needs a test of a mode, and how many attempts it has
-- had, is read from its tests, so the queue and the record of the New Games collection go.
DROP TABLE pending_tests;
ALTER TABLE games DROP COLUMN in_new_games;

-- The account a test played with, from its document. Tests stored before version 2 of the schema
-- have no mode: they were played in demo mode.
ALTER TABLE tests ADD COLUMN mode TEXT NOT NULL
    GENERATED ALWAYS AS (coalesce(json_extract(test, '$.mode'), 'demo')) VIRTUAL
    CHECK (mode IN ('demo', 'real_money', 'no_balance'));

CREATE INDEX tests_game_id_mode_started_at ON tests(game_id, mode, started_at);
