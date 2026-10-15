-- How many checks of the watch have tried the demo test of a queued game without reaching a
-- verdict on it. The watch takes the game off the queue once this reaches its maximum, so a game
-- that never gets a verdict does not cost an agent run at every check forever.
ALTER TABLE pending_tests ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0);
