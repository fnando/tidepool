# tidepool

tidepool keeps local copies of YouTube playlists in sync. For each video it
saves the video file, its details (title, channel, description, …) and the
largest available cover image. Rules decide which videos to keep, and limits
keep each playlist's folder from growing forever.

> **Early days.** You can set up tidepool, write and check your configuration,
> and test your filters. Downloading isn't available yet.

Works on Linux and macOS.

## Requirements

- [`ffmpeg`](https://ffmpeg.org) (including `ffprobe`), e.g.
  `brew install ffmpeg` or `apt install ffmpeg`.
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp): you won't need to install it.
  tidepool will download it and keep it up to date once downloading is
  available. If you'd rather use your own copy, see
  [Using your own yt-dlp](#using-your-own-yt-dlp).

## Installation

TBD.

## Getting started

Run:

```sh
tidepool init
```

It creates your config file at `~/.config/tidepool/config.toml`, sets up
tidepool to use it, and tells you what to do next. Nothing is asked.

The config file lists every setting with a short explanation. Settings you
haven't changed are commented out and show their default value, so the file
doubles as a reference. It comes with a sample playlist holding one short
video, so you can see tidepool work before adding your own playlists.

| Option | Meaning |
| --- | --- |
| `--config <path>` | Write the config file somewhere else |
| `--print` | Only show the config file it would write; create nothing |
| `--force` | Replace an existing config file (the old one is kept as `config.toml.bak`) |

`init` never overwrites anything by accident: if the config file already
exists, it stops and suggests what to do instead.

## Configuration

Your settings live in one file, `~/.config/tidepool/config.toml`. Open it in
any text editor. Its sections:

| Section | What it's for |
| --- | --- |
| `[library]` | Where videos go, how many download at once |
| `[ytdlp]` | Which yt-dlp to use and how it's kept up to date |
| `[youtube]` | Your YouTube API key (optional) |
| `[schedule]` | How often to sync in the background |
| `[server]` | The web interface |
| `[defaults]` | Settings every playlist uses unless it sets its own |
| `[hooks.<name>]` | Your own scripts to run after each download |
| `[playlists.<name>]` | The playlists to keep in sync |

A minimal playlist looks like this:

```toml
[playlists.lofi]
url = "https://www.youtube.com/playlist?list=PLxxxxxxxx"
```

Any setting from `[defaults]` can be changed for a single playlist by adding it
under that playlist. See [`examples/config.toml`](examples/config.toml) for a
complete example.

### Applying changes

tidepool keeps its own copy of your settings, so a half-edited file never
affects anything. After editing the file, apply it:

```sh
tidepool config import ~/.config/tidepool/config.toml
```

Or add `--config <path>` to any command; the file is applied first if it
changed. This is handy when you keep the file in a dotfiles repository:

```sh
tidepool filter test "Lofi LIVE radio" --playlist lofi --config ~/.config/tidepool/config.toml
```

### Checking for mistakes

```sh
tidepool config check ~/.config/tidepool/config.toml
```

This shows every problem at once, with the line and column where it is:

```
config.toml:2:1 library.concurency: unknown key "concurency"
config.toml:20:12 playlists.lofi.cleanup.max_size: "20 gigs" is not a valid byte size (e.g. "20GiB", "500MB")
```

It also checks that `ffmpeg` and yt-dlp are available and that the video
folder is writable. Then it prints your settings with every default filled in,
so you can see exactly what each playlist will do. A file with mistakes is
never applied.

Editors that understand the `#:schema` line at the top of the file (for
example VS Code with the *Even Better TOML* extension) suggest setting names
and flag mistakes as you type.

### History and undo

Every applied change is kept, so you can always go back:

| Command | What it does |
| --- | --- |
| `tidepool config` | Show the current settings |
| `tidepool config history` | List every applied version (`*` marks the current one) |
| `tidepool config diff` | Show how your file differs from the current settings |
| `tidepool config export [--rev N]` | Show an older version |
| `tidepool config rollback N` | Go back to version `N` (saved as a new version, so nothing is lost) |

Your API key and web interface token are hidden in this output; add
`--show-secrets` to see them.

If you roll back and later pass `--config` with a file that differs from the
current settings, tidepool stops instead of silently undoing your rollback. Use
`tidepool config diff` to compare, and `--force-config` if you do want the file
applied.

### Testing your filters

`include` and `exclude` rules decide which videos are kept. They're regular
expressions, tested against the video title by default:

```toml
[playlists.lofi]
url = "https://www.youtube.com/playlist?list=PLxxxxxxxx"
include = ['(?i)\blive\b']                 # only videos with "live" in the title
exclude = ['(?i)\b(trailer|teaser)\b']     # but never trailers or teasers
```

A video is kept when it matches at least one `include` rule (or there are
none) and no `exclude` rule. Try a title against a playlist's rules:

```sh
tidepool filter test "Lofi LIVE radio" --playlist lofi
```

```
playlist lofi (PLxxxxxxxx)
  include[0] "(?i)\\blive\\b" on title: match
  exclude[0] "(?i)\\b(trailer|teaser)\\b" on title: no match
decision: keep (matches include[0])
```

### YouTube API key

An API key is optional. Once downloading is available, a key will make
checking playlists for new videos faster and bring more details per video;
without one, tidepool uses yt-dlp for everything. To add a key later, set it in the config file and apply the change:

```toml
[youtube]
api_key = "AIza..."
```

The config file is created so only you can read it. If you create it some
other way and it holds a key or token, `config check` warns you if other users
can read it.

### Using your own yt-dlp

```toml
[ytdlp]
managed = false
path = "/usr/local/bin/yt-dlp"
```

### The web interface

The web interface is coming soon, but you can already choose its settings. By
default it listens on `http://127.0.0.1:8787`, so only your own computer can
reach it. To reach it from other machines, change `listen` and set a `token` (a
password); tidepool won't accept other addresses without one:

```toml
[server]
listen = "0.0.0.0:8787"
token = "a long random secret"
```

### More than one library

Each set of playlists with its own settings is a *library*. You get one called
`default`; add `--library <name>` to any command to use another. Each library
has its own settings and history:

```sh
tidepool init --library music --config ~/.config/tidepool/music.toml
tidepool config history --library music
```

## Where things are

| What | Where |
| --- | --- |
| Your config file | `~/.config/tidepool/config.toml` |
| Videos (unless you choose another folder) | `~/.local/share/tidepool/media` |
| tidepool's own data | `~/.local/share/tidepool/` |

If you set `XDG_CONFIG_HOME` or `XDG_DATA_HOME`, tidepool uses those folders
instead, on macOS too.

## Exit codes

For scripts:

| Code | Meaning |
| --- | --- |
| 0 | Success |
| 1 | Something went wrong (e.g. a config mistake); the message says what |
| 2 | The command was used incorrectly (unknown option, unknown playlist) |
