# tidepool

Keep local mirrors of YouTube playlists in sync: media, a normalized
`metadata.json` and the largest cover image per video, with include/exclude
rules, post-download hooks and cleanup policies.

> **Status: M1 (skeleton and config).** The CLI surface, configuration
> (validation, merging, storage with history and rollback) and rule testing
> work. Indexing and downloading land in M2; see [Roadmap](#roadmap).

Supports Linux and macOS. Requires [`yt-dlp`](https://github.com/yt-dlp/yt-dlp)
on `PATH` (and `ffmpeg`/`ffprobe` when embedding covers).

## Install

```sh
cargo install --path .
```

## Quick start

```sh
mkdir -p ~/.config/tidepool
cp examples/config.toml ~/.config/tidepool/config.toml
$EDITOR ~/.config/tidepool/config.toml

tidepool config check ~/.config/tidepool/config.toml   # validate + print the effective config
tidepool config import ~/.config/tidepool/config.toml  # store it as revision 1
tidepool filter test "Lofi LIVE radio" -p lofi          # which rule keeps or drops a title?
```

## Configuration

One TOML file with `[library]`, `[youtube]`, `[schedule]`, `[defaults]`,
`[hooks.<name>]` and `[playlists.<key>]`. Playlists override `[defaults]` key by
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
| `tidepool config` | Print the current revision (API keys masked unless `--show-secrets`) |
| `tidepool config import <path>` | Validate and store as a new revision; no-op if unchanged |
| `tidepool config export [--rev N]` | Print a revision as TOML |
| `tidepool config history` | List revisions (`*` = current) |
| `tidepool config rollback <rev>` | Make an old revision current, as a new revision |
| `tidepool config check [<path>]` | Validate a file (or the current revision), check `yt-dlp`, the media root and file permissions, print the effective config |
| `tidepool config diff [<path>]` | Key-level summary + text diff against the current revision (defaults to the last imported file) |
| `tidepool config schema` | Print the JSON Schema |

`--config <path>` (or `TIDEPOOL_CONFIG`) on any command imports the file first
if it changed, so a git-managed config stays a single cron line. If the current
revision came from a rollback (or, later, the UI) the run stops instead of
overwriting it; `--force-config` imports anyway.

### Paths

| What | Where |
| --- | --- |
| Config file (convention) | `~/.config/tidepool/config.toml` |
| State, lock, logs | `~/.local/share/tidepool/libraries/<name>/` |
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
| M1 Skeleton and config ✅ | `config …`, `filter test` |
| M2 yt-dlp path end to end | `sync`, `index`, `download`, `reset` |
| M3 API indexer | (indexing via YouTube Data API v3) |
| M4 Hooks and embedding | `hooks run`, `embed` |
| M5 Cleanup | `cleanup`, `pin`, `unpin`, `config check --reconcile` |
| M6 Polish | `list`, progress, release builds |
| M7 Management UI | `run`, `server`, `config merge` |

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

Error messages for invalid configs are snapshot-tested with
[`insta`](https://insta.rs) over `tests/fixtures/config/invalid/*.toml`; review
changes with `cargo insta review`.
