-- tidepool state schema, v1.

CREATE TABLE playlists (
  key TEXT PRIMARY KEY,            -- config key
  youtube_id TEXT NOT NULL UNIQUE,
  title TEXT, etag TEXT, rules_hash TEXT,
  last_indexed_at TEXT, last_index_source TEXT
);

CREATE TABLE videos (              -- shared across playlists
  id TEXT PRIMARY KEY,
  title TEXT, channel TEXT, description TEXT, tags_json TEXT,
  duration_s INTEGER, published_at TEXT, availability TEXT,
  thumbs_json TEXT, updated_at TEXT
);

CREATE TABLE items (               -- a video's membership in a playlist
  playlist_key TEXT NOT NULL REFERENCES playlists(key),
  video_id TEXT NOT NULL REFERENCES videos(id),
  position INTEGER, added_at TEXT, in_remote INTEGER NOT NULL,
  status TEXT NOT NULL, decision TEXT,
  dir TEXT, media_present INTEGER NOT NULL DEFAULT 1, media_bytes INTEGER, downloaded_at TEXT,
  attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at TEXT, last_error TEXT,
  pinned INTEGER NOT NULL DEFAULT 0, cleaned_up_at TEXT, cleanup_reason TEXT,
  PRIMARY KEY (playlist_key, video_id)
);

CREATE TABLE hook_runs (
  playlist_key TEXT, video_id TEXT, hook TEXT, event TEXT,
  status TEXT, exit_code INTEGER, attempts INTEGER, started_at TEXT, finished_at TEXT,
  PRIMARY KEY (playlist_key, video_id, hook, event)
);

CREATE TABLE runs (
  id INTEGER PRIMARY KEY, command TEXT, started_at TEXT, finished_at TEXT,
  quota_units INTEGER, summary_json TEXT
);

CREATE TABLE config_revisions (
  rev INTEGER PRIMARY KEY, created_at TEXT NOT NULL,
  source TEXT NOT NULL,            -- import | ui | rollback | merge
  source_path TEXT, source_hash TEXT,
  toml TEXT NOT NULL,              -- as imported or edited, comments kept
  json TEXT NOT NULL               -- validated, normalized
);

CREATE TABLE quota_usage (
  day TEXT NOT NULL,               -- Pacific-time date, matching Google's reset
  playlist_key TEXT, run_id INTEGER,
  units INTEGER NOT NULL
);

CREATE TABLE problems (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL, -- e.g. ytdlp_extraction, ytdlp_bot_check, hook_failing
  first_seen TEXT, last_seen TEXT, count INTEGER NOT NULL,
  sample TEXT, ytdlp_version TEXT, playlists_json TEXT, hint TEXT,
  shown_cli_at TEXT, acknowledged_at TEXT, resolved_at TEXT
);

CREATE INDEX items_status ON items(playlist_key, status);
