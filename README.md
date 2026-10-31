# tidepool

tidepool keeps local copies of YouTube playlists in sync. For each video it
saves the video file, its details (title, channel, description, …) and the
largest available cover image. Rules decide which videos to keep, and limits
keep each folder from growing forever.

> **Early days.** You can set up tidepool, write and check your configuration,
> and see what each source contains and which videos would be downloaded.
> Downloading isn't available yet.

Works on Linux and macOS.

## Requirements

- [`ffmpeg`](https://ffmpeg.org) (including `ffprobe`), e.g.
  `brew install ffmpeg` or `apt install ffmpeg`.
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp): you don't need to install it;
  tidepool downloads it and keeps it up to date (see [yt-dlp](#yt-dlp)).

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
doubles as a reference. It comes with a sample source holding one short
video, so you can see tidepool work before adding your own.

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
| `[defaults]` | Settings every source uses unless it sets its own |
| `[hooks.<name>]` | Your own scripts to run after each download |
| `[sources.<name>]` | What to keep in sync |

A *source* is something to keep in sync: for now, a YouTube playlist. A
minimal one looks like this:

```toml
[sources.lofi]
url = "https://www.youtube.com/playlist?list=PLxxxxxxxx"
```

`url` is the playlist's address, as you'd copy it from your browser. A link to
a video inside a playlist works too, as long as it contains `list=`. The name
(`lofi` here) is up to you: lowercase letters, digits, `-` and `_`. It's also
the name of the source's folder, unless you set `dir`.

Any setting from `[defaults]` can be changed for a single source by adding it
under that source. See [`examples/config.toml`](examples/config.toml) for a
complete example.

### Applying changes

tidepool keeps its own copy of your settings, so a half-edited file never
affects anything. After editing the file, apply it:

```sh
tidepool config import
```

This reads `~/.config/tidepool/config.toml`. If your file lives somewhere else,
pass its path: `tidepool config import ~/dotfiles/tidepool.toml`.

Or add `--config <path>` to any command; the file is applied first if it
changed. This is handy when you keep the file in a dotfiles repository:

```sh
tidepool filter test "Lofi LIVE radio" --source lofi --config ~/.config/tidepool/config.toml
```

### Checking for mistakes

```sh
tidepool config check ~/.config/tidepool/config.toml
```

This shows every problem at once, with the line and column where it is:

```
config.toml:2:1 library.concurency: unknown key "concurency"
config.toml:20:12 sources.lofi.cleanup.max_size: "20 gigs" is not a valid byte size (e.g. "20GiB", "500MB")
```

It also checks that `ffmpeg` and yt-dlp are available and that the video
folder is writable. Then it prints your settings with every default filled in,
so you can see exactly what each source will do. A file with mistakes is
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

### Checking your sources

To see what each source contains and what tidepool would do with it:

```sh
tidepool index
```

```
sample: 1 video (1 new): 1 to download
lofi: 12 videos (12 new): 2 to download, 1 to check before download, 9 skipped (5 unavailable, 1 excluded by a rule, 1 published before the cutoff, 1 too long, 1 too short)
```

Add `--source <name>` (or `-s`) to check only some sources. Videos are skipped
when they're private, deleted, members-only or not out yet (premieres and live
streams are picked up once they're over), when they're outside `min_duration`
and `max_duration`, when they were published before `cutoff`, or because of
your `include` and `exclude` rules. "To check before download" means a rule
needs a detail YouTube didn't list (often the publish date or description);
it's looked up right before downloading.

After you change your rules, run `tidepool index` again: videos that haven't
been downloaded yet are re-checked against the new rules. If a source can't
be read (for example, a wrong or private playlist), the others are still
checked, and the command ends with exit code 3.

### Testing your filters

`include` and `exclude` rules decide which videos are kept. They're regular
expressions, tested against the video title by default:

```toml
[sources.lofi]
url = "https://www.youtube.com/playlist?list=PLxxxxxxxx"
include = ['(?i)\blive\b']                 # only videos with "live" in the title
exclude = ['(?i)\b(trailer|teaser)\b']     # but never trailers or teasers
```

A video is kept when it matches at least one `include` rule (or there are
none) and no `exclude` rule. Try a title against a source's rules:

```sh
tidepool filter test "Lofi LIVE radio" --source lofi
```

```
source lofi (PLxxxxxxxx)
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

### yt-dlp

tidepool downloads [yt-dlp](https://github.com/yt-dlp/yt-dlp) the first time
it's needed and keeps it up to date, checking for a new release at most once an
hour. Before installing a release, it checks that the release was signed by the
yt-dlp project and that the file wasn't altered, so a tampered download is
never used.

| Command | What it does |
| --- | --- |
| `tidepool ytdlp status` | Show the installed version, where it is, and when tidepool last checked for updates |
| `tidepool ytdlp update` | Check for a new release now and install it |
| `tidepool ytdlp rollback` | Go back to the version you had before the last update. The release you rolled back from is skipped until a newer one comes out |

By default tidepool follows yt-dlp's *nightly* releases, which is what the
yt-dlp project recommends, since YouTube changes often. To follow the regular
releases instead, or check less often:

```toml
[ytdlp]
channel = "stable"         # stable | nightly | master
update_check = "1d"
```

#### Using your own yt-dlp

If you'd rather install and update yt-dlp yourself:

```toml
[ytdlp]
managed = false
path = "/usr/local/bin/yt-dlp"
```

tidepool then uses that copy as it is, and never updates it.

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

Each set of sources with its own settings is a *library*. You get one called
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
| 2 | The command was used incorrectly (unknown option, unknown source) |
| 3 | Some sources couldn't be checked; the others were |
