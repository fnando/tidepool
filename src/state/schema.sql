-- tidepool state schema, v1.

CREATE TABLE sources (
  key TEXT PRIMARY KEY,            -- config key
  lists TEXT NOT NULL,             -- JSON: the ids of its urls (playlist ids, channels as written)
  title TEXT, etag TEXT, rules_hash TEXT,
  last_indexed_at TEXT, last_index_source TEXT
);

CREATE TABLE videos (              -- shared across sources
  id TEXT PRIMARY KEY,
  title TEXT, channel TEXT, channel_id TEXT, description TEXT, tags_json TEXT,
  duration_s INTEGER, published_at TEXT, availability TEXT,
  thumbs_json TEXT, updated_at TEXT,
  kind TEXT                        -- video, short or live, once known
);

CREATE TABLE items (               -- a video's membership in a source
  source_key TEXT NOT NULL REFERENCES sources(key),
  video_id TEXT NOT NULL REFERENCES videos(id),
  position INTEGER, added_at TEXT, in_remote INTEGER NOT NULL,
  status TEXT NOT NULL, decision TEXT,
  lists TEXT,                       -- JSON: ids of the source's lists it was last seen in
  dir TEXT, format TEXT,            -- output_dir (absolute); format string it was downloaded with
  files TEXT,                       -- JSON: every file placed for it, absolute paths (VideoFiles)
  metadata TEXT,                    -- a copy of its metadata.json, to restore it
  missing TEXT,                     -- JSON list of files found missing: media, cover, subtitle:<lang>, metadata
  media_present INTEGER NOT NULL DEFAULT 1, media_bytes INTEGER, downloaded_at TEXT,
  attempts INTEGER NOT NULL DEFAULT 0, next_attempt_at TEXT, last_error TEXT,
  pinned INTEGER NOT NULL DEFAULT 0, cleaned_up_at TEXT, cleanup_reason TEXT,
  PRIMARY KEY (source_key, video_id)
);

CREATE TABLE hook_runs (
  source_key TEXT, video_id TEXT, hook TEXT, type TEXT,
  status TEXT, exit_code INTEGER, attempts INTEGER, started_at TEXT, finished_at TEXT,
  error TEXT,                       -- why it failed
  input_values TEXT,                -- JSON: the placeholder values it ran with, to run it again
  PRIMARY KEY (source_key, video_id, hook, type)
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

CREATE TABLE channels (           -- channel sources, looked up once
  ref TEXT PRIMARY KEY,            -- as in the URL: @handle, UC…, c/name, user/name
  channel_id TEXT NOT NULL,        -- UC…; its uploads playlist is UU…
  title TEXT,
  resolved_at TEXT
);

CREATE TABLE quota_usage (
  day TEXT NOT NULL,               -- Pacific-time date, matching Google's reset
  key_id TEXT NOT NULL,            -- which API key: the start of its SHA-256, never the key
  source_key TEXT, run_id INTEGER,
  units INTEGER NOT NULL
);

CREATE TABLE problems (
  id INTEGER PRIMARY KEY, kind TEXT NOT NULL, -- e.g. ytdlp_extraction, ytdlp_bot_check, hook_failing
  first_seen TEXT, last_seen TEXT, count INTEGER NOT NULL,
  sample TEXT, ytdlp_version TEXT, sources_json TEXT, hint TEXT,
  shown_cli_at TEXT, acknowledged_at TEXT, resolved_at TEXT
);

CREATE INDEX items_status ON items(source_key, status);
