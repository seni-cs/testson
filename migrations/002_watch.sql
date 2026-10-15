-- The primary type the catalogue gives a game, such as `Slots`. Empty when it gives none.
ALTER TABLE games ADD COLUMN primary_type TEXT NOT NULL DEFAULT '';

-- Whether the game was in the New Games collection (Novedades) at the latest check of the watch.
-- A game that turns up there and was not in it at the previous check is new.
ALTER TABLE games ADD COLUMN in_new_games INTEGER NOT NULL DEFAULT 0
    CHECK (in_new_games IN (0, 1));

-- Games the watch found new and whose demo test has not reached a verdict on the game yet. A
-- test that ends in `test_failed`, or a run that produced no test, leaves the game here to be
-- tried again at the next check.
CREATE TABLE pending_tests (
    game_id   TEXT NOT NULL PRIMARY KEY REFERENCES games(id),
    queued_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
) STRICT;
