-- The library cache. Everything here is rebuildable from the sources; there is deliberately no
-- user-edit column yet. Imported cues/grids are kept with their provenance so a later read layer can
-- apply a clock offset without losing the source values.

CREATE TABLE IF NOT EXISTS source (
    id            INTEGER PRIMARY KEY,
    kind          TEXT NOT NULL,
    name          TEXT NOT NULL,
    root          TEXT NOT NULL,
    origin        TEXT NOT NULL DEFAULT 'local',
    fingerprint   TEXT,
    created_at    INTEGER NOT NULL,
    last_scan_at  INTEGER,
    UNIQUE (kind, root)
);

CREATE TABLE IF NOT EXISTS playlist (
    id          INTEGER PRIMARY KEY,
    source_id   INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    parent_id   INTEGER REFERENCES playlist(id) ON DELETE CASCADE,
    native_key  TEXT NOT NULL,
    name        TEXT NOT NULL,
    kind        TEXT NOT NULL,
    position    INTEGER NOT NULL DEFAULT 0,
    UNIQUE (source_id, native_key)
);

CREATE INDEX IF NOT EXISTS playlist_parent ON playlist(source_id, parent_id, position);

CREATE TABLE IF NOT EXISTS track (
    id               INTEGER PRIMARY KEY,
    content_key      TEXT NOT NULL UNIQUE,
    title            TEXT,
    artist           TEXT,
    album            TEXT,
    genre            TEXT,
    year             INTEGER,
    duration_frames  INTEGER NOT NULL DEFAULT 0,
    sample_rate      INTEGER NOT NULL DEFAULT 0,
    channels         INTEGER NOT NULL DEFAULT 0,
    bpm              REAL,
    key_pc           INTEGER,
    key_mode         TEXT,
    key_confidence   REAL,
    created_at       INTEGER NOT NULL,
    updated_at       INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS track_location (
    id              INTEGER PRIMARY KEY,
    track_id        INTEGER NOT NULL REFERENCES track(id) ON DELETE CASCADE,
    source_id       INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    path            TEXT NOT NULL,
    relative_path   TEXT NOT NULL,
    file_size       INTEGER NOT NULL,
    mtime_ns        INTEGER NOT NULL,
    file_hash       TEXT NOT NULL,
    native_id       TEXT,
    source_meta_json TEXT,
    last_seen_scan  INTEGER NOT NULL DEFAULT 0,
    UNIQUE (source_id, path)
);

CREATE INDEX IF NOT EXISTS location_track ON track_location(track_id);
CREATE INDEX IF NOT EXISTS location_scan ON track_location(source_id, last_seen_scan);

CREATE TABLE IF NOT EXISTS playlist_item (
    playlist_id  INTEGER NOT NULL REFERENCES playlist(id) ON DELETE CASCADE,
    location_id  INTEGER NOT NULL REFERENCES track_location(id) ON DELETE CASCADE,
    position     INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, position)
);

CREATE INDEX IF NOT EXISTS playlist_item_location ON playlist_item(location_id);

CREATE TABLE IF NOT EXISTS scan_run (
    id             INTEGER PRIMARY KEY,
    source_id      INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    started_at     INTEGER NOT NULL,
    finished_at    INTEGER,
    status         TEXT NOT NULL,
    files_seen     INTEGER NOT NULL DEFAULT 0,
    files_added    INTEGER NOT NULL DEFAULT 0,
    files_updated  INTEGER NOT NULL DEFAULT 0,
    files_removed  INTEGER NOT NULL DEFAULT 0,
    errors         INTEGER NOT NULL DEFAULT 0
);

-- The tables below are filled by the Rekordbox/Serato adapters in the next phase. Keeping them in
-- the first migration means the cache shape does not change when those adapters land.

CREATE TABLE IF NOT EXISTS cue (
    id           INTEGER PRIMARY KEY,
    location_id  INTEGER NOT NULL REFERENCES track_location(id) ON DELETE CASCADE,
    source_id    INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    origin       TEXT NOT NULL,
    kind         TEXT NOT NULL,
    idx          INTEGER,
    raw_time_ms  REAL NOT NULL,
    clock        TEXT NOT NULL,
    offset_ms    REAL NOT NULL DEFAULT 0.0,
    color        TEXT,
    comment      TEXT,
    UNIQUE (location_id, origin, kind, idx)
);

CREATE TABLE IF NOT EXISTS beatgrid (
    id                 INTEGER PRIMARY KEY,
    location_id        INTEGER NOT NULL REFERENCES track_location(id) ON DELETE CASCADE,
    source_id          INTEGER NOT NULL REFERENCES source(id) ON DELETE CASCADE,
    origin             TEXT NOT NULL,
    bpm                REAL,
    anchor_raw_ms      REAL,
    clock              TEXT NOT NULL,
    offset_ms          REAL NOT NULL DEFAULT 0.0,
    confidence         REAL,
    algorithm_version  TEXT,
    UNIQUE (location_id, origin)
);

CREATE TABLE IF NOT EXISTS beat_anchor (
    grid_id     INTEGER NOT NULL REFERENCES beatgrid(id) ON DELETE CASCADE,
    seq         INTEGER NOT NULL,
    beat        INTEGER NOT NULL,
    bpm         REAL,
    raw_time_ms REAL NOT NULL,
    time_ms     REAL NOT NULL,
    PRIMARY KEY (grid_id, seq)
);

CREATE TABLE IF NOT EXISTS clock_calibration (
    id         INTEGER PRIMARY KEY,
    source_id  INTEGER REFERENCES source(id) ON DELETE CASCADE,
    codec      TEXT NOT NULL,
    sample_rate INTEGER NOT NULL,
    offset_ms  REAL NOT NULL,
    method     TEXT NOT NULL,
    confidence REAL,
    updated_at INTEGER NOT NULL,
    UNIQUE (source_id, codec, sample_rate)
);

CREATE TABLE IF NOT EXISTS analysis (
    track_id          INTEGER PRIMARY KEY REFERENCES track(id) ON DELETE CASCADE,
    bpm               REAL,
    key_pc            INTEGER,
    key_mode          TEXT,
    key_confidence    REAL,
    beat_frames_json  TEXT NOT NULL,
    sample_rate       INTEGER NOT NULL,
    algorithm         TEXT NOT NULL,
    version           TEXT NOT NULL,
    analysed_at       INTEGER NOT NULL
);
