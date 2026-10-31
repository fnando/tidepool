# tidepool

Keep local mirrors of YouTube playlists in sync: media, a normalized
`metadata.json` and the largest cover image per video, with include/exclude
rules, post-download hooks and cleanup policies.

> **Status: M1 (skeleton and config).** `tidepool init`, the CLI surface,
> configuration (validation, merging, storage with history and rollback) and
> rule testing work. Indexing and downloading land in M2; see
> [Roadmap](#roadmap).

Supports Linux and macOS. Requires `ffmpeg`/`ffprobe` (merging formats,
embedding). [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) is downloaded and kept
up to date by tidepool (from M2); set `ytdlp.managed = false` to use your own.

## Install

```sh
cargo install --path .
```

## Quick start

```sh
tidepool init                                   # asks a few questions
tidepool filter test "Lofi LIVE radio" -p lofi  # which rule keeps or drops a title?
```

`init` writes a commented config file (`~/.config/tidepool/config.toml`, or
`--config <path>`) listing every option with its default, creates the library
and imports the file as revision 1. For scripts, answer with flags:

```sh
tidepool init --yes --root ~/Videos/youtube \
  --playlist https://www.youtube.com/playlist?list=PLxxxxxxxx \
  --interval 6h --no-server
```

Playlists added with flags are named after their id, lowercased. `init` never
overwrites: an existing file needs `--force` (the old one is kept as `.bak`),
and a library that already has a config is never touched. `--print` writes the
config to stdout and creates nothing.

## Configuration

One TOML file with `[library]`, `[ytdlp]`, `[youtube]`, `[schedule]`,
`[server]`, `[defaults]`, `[hooks.<name>]` and `[playlists.<key>]`. Playlists override `[defaults]` key by
key; lists (`include`, `exclude`, `hooks`, `ytdlp_args`) replace the defaults
unless `inherit_rules = true` / `inherit_hooks = true`. See
[`examples/config.toml`](examples/config.toml) for every option.

### Validation

The config's shape is a JSON Schema (draft 2020-12) generated from the Rust
types: [`schema/tidepool.v1.schema.json`](schema/tidepool.v1.schema.json), also
printed by `tidepool config schema`. Every error is reported at once with its
location:

```
config.toml:2:1 library.concurency: unknown key "concurency"
config.toml:20:12 playlists.lofi.cleanup.max_size: "20 gigs" is not a valid byte size (e.g. "20GiB", "500MB")
```

For autocompletion in Taplo-based editors (e.g. Even Better TOML), start the
file with:

```toml
#:schema https://raw.githubusercontent.com/fnando/tidepool/v0.1.0/schema/tidepool.v1.schema.json
```

### Storage

Each library keeps its config in its state DB as numbered revisions; TOML files
are how it gets in and out (comments are kept).

| Command | What it does |
| --- | --- |
| `tidepool config` | Print the current revision (`youtube.api_key` and `server.token` masked unless `--show-secrets`) |
| `tidepool config import <path>` | Validate and store as a new revision; no-op if unchanged |
| `tidepool config export [--rev N]` | Print a revision as TOML |
| `tidepool config history` | List revisions (`*` = current) |
| `tidepool config rollback <rev>` | Make an old revision current, as a new revision |
| `tidepool config check [<path>]` | Validate a file (or the current revision), check `yt-dlp`, `ffmpeg`/`ffprobe`, the media root and file permissions, print the effective config |
| `tidepool config diff [<path>]` | Key-level summary + text diff against the current revision (defaults to the last imported file) |
| `tidepool config schema` | Print the JSON Schema |

`--config <path>` (or `TIDEPOOL_CONFIG`) on any command imports the file first
if it changed, so a git-managed config stays a single cron line. If the current
revision came from a rollback (or, later, the web UI) the run stops instead of
overwriting it; `--force-config` imports anyway.

### Paths

| What | Where |
| --- | --- |
| Config file (convention) | `~/.config/tidepool/config.toml` |
| State, lock, logs | `~/.local/share/tidepool/libraries/<name>/` |
| Managed yt-dlp | `~/.local/share/tidepool/bin/` |
| Media (default `library.root`) | `~/.local/share/tidepool/media` |

`$XDG_CONFIG_HOME` and `$XDG_DATA_HOME` are honored on macOS too.
`--library <name>` (default `default`) picks the library.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Fatal error (bad config, DB error, lock held, not implemented yet) |
| 2 | Usage error (bad flags, unknown `--playlist`) |
| 3 | Partial failure (M2+) |
| 4 | API quota exhausted and fallback disabled (M3+) |

## Roadmap

| Milestone | Commands |
| --- | --- |
| M1 Skeleton and config ✅ | `init`, `config …`, `filter test` |
| M2 yt-dlp path end to end | `sync`, `index`, `download`, `reset`, managed yt-dlp (`ytdlp …`), `problems` |
| M3 API indexer | (indexing via YouTube Data API v3) |
| M4 Hooks and embedding | `hooks run`, `embed` |
| M5 Cleanup | `cleanup`, `pin`, `unpin`, `config check --reconcile` |
| M6 Polish | `list`, progress, release builds |
| M7 Daemon and management UI | `run` (with the web UI from `[server]`), `config merge` |

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

The schema file is checked by a test. After changing config types (or bumping
the version, which is part of the schema `$id`), regenerate it:

```sh
cargo run -- config schema > schema/tidepool.v1.schema.json
```

`tests/bin/` holds fake `yt-dlp`, `ffmpeg` and `ffprobe` scripts that the
end-to-end tests put first on `PATH`, so tests never depend on (or run) the
real tools.

The template `tidepool init` writes is tested against the schema: a new config
option must be added to it (with its real default) or the tests fail.

Error messages for invalid configs are snapshot-tested with
[`insta`](https://insta.rs) over `tests/fixtures/config/invalid/*.toml`; review
changes with `cargo insta review`.
