-- Output-stream identity is independent of event-retention epochs. This state
-- survives deletion of any or all retained segments; a new database starts a
-- new epoch. Copying/restoring an entire database also copies its epoch.
CREATE TABLE IF NOT EXISTS segment_stream_state (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    cursor_epoch TEXT NOT NULL CHECK(
        typeof(cursor_epoch) = 'text'
        AND length(cursor_epoch) = 32
        AND cursor_epoch NOT GLOB '*[^0-9a-f]*'
    ),
    max_segment_id INTEGER NOT NULL CHECK(
        typeof(max_segment_id) = 'integer' AND max_segment_id >= 0
    )
);

-- This seed runs only during installation, never as a current-schema repair.
-- The subqueries keep WHERE NOT EXISTS outside the aggregates, including when
-- output_segments is empty or fresh initialization applies this SQL twice.
-- Legacy segment IDs were plain rowids, so retention could already have freed
-- the highest ones. A secret-scan checkpoint is the one stored segment ID with
-- no foreign key; seeding past it keeps an incremental scan from skipping
-- reissued IDs.
INSERT INTO segment_stream_state (singleton, cursor_epoch, max_segment_id)
SELECT 1, lower(hex(randomblob(16))),
       MAX((SELECT COALESCE(MAX(id), 0) FROM output_segments),
           (SELECT COALESCE(MAX(last_segment_id), 0) FROM secret_scan_reports))
WHERE NOT EXISTS (SELECT 1 FROM segment_stream_state WHERE singleton = 1);

CREATE INDEX IF NOT EXISTS idx_segments_pane_id ON output_segments(pane_id, id);

CREATE TRIGGER IF NOT EXISTS segment_stream_state_insert_guard
BEFORE INSERT ON segment_stream_state
WHEN EXISTS (SELECT 1 FROM segment_stream_state)
BEGIN
    SELECT RAISE(ABORT, 'segment stream authority cannot be replaced');
END;

CREATE TRIGGER IF NOT EXISTS segment_stream_state_delete_guard
BEFORE DELETE ON segment_stream_state
BEGIN
    SELECT RAISE(ABORT, 'segment stream authority cannot be deleted');
END;

CREATE TRIGGER IF NOT EXISTS segment_stream_state_update_guard
BEFORE UPDATE ON segment_stream_state
WHEN NEW.singleton IS NOT OLD.singleton
  OR NEW.cursor_epoch IS NOT OLD.cursor_epoch
  OR NEW.max_segment_id < OLD.max_segment_id
BEGIN
    SELECT RAISE(ABORT, 'segment stream identity and high-water mark cannot regress');
END;

-- Use the assigned NEW.id, including for legacy implicit-rowid inserts. Guard
-- and advance in one trigger so AFTER-trigger ordering cannot reject our own
-- allocation. ABORT rolls back the row and all other trigger side effects.
CREATE TRIGGER IF NOT EXISTS output_segments_monotonic_id_ai
AFTER INSERT ON output_segments
BEGIN
    SELECT CASE WHEN NOT EXISTS (
        SELECT 1 FROM segment_stream_state WHERE singleton = 1
    ) OR NEW.id <= COALESCE((
        SELECT max_segment_id FROM segment_stream_state WHERE singleton = 1
    ), 9223372036854775807)
    THEN RAISE(ABORT, 'segment id must advance durable stream high-water mark') END;
    UPDATE segment_stream_state SET max_segment_id = NEW.id WHERE singleton = 1;
END;

CREATE TRIGGER IF NOT EXISTS output_segments_id_update_guard
BEFORE UPDATE ON output_segments
WHEN NEW.id IS NOT OLD.id
BEGIN
    SELECT RAISE(ABORT, 'segment stream ids are immutable');
END;
