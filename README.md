# tidepool

tidepool keeps local copies of YouTube playlists in sync. For each video it
saves the video file, its details (title, channel, description, …) and the
largest available cover image. Rules decide which videos to keep, and limits
keep each folder from growing forever.

> **Early days.** tidepool can keep your sources downloaded. Running your own
> scripts after each download, automatic cleanup, running in the background and
> the web interface aren't available yet.

Works on Linux and macOS.

## Requirements

- [`ffmpeg`](https://ffmpeg.org) (including `ffprobe`), e.g.
  `brew install ffmpeg` or `apt install ffmpeg`.
- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) and [Deno](https://deno.com)
  (which yt-dlp needs for YouTube): you don't need to install them; tidepool
  downloads them and keeps them up to date (see [Tools](#tools)).

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
video, so you can see tidepool work before adding your own:

```sh
tidepool run
```

```
installed yt-dlp 2026.10.29.232701
sample: 1 video (1 new): 1 to download
sample: downloaded “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)” (243.8 MB)
downloaded 1 video (243.8 MB)
```

The video lands in `~/.local/share/tidepool/media/sample/`. tidepool downloads
the best quality available by default, which for this video is 4K (about
240 MB); set `format` to download smaller files, e.g.
`format = "bv*[height<=1080]+ba/b"` for 1080p at most. See
[Downloading](#downloading) for what you get and how to run it regularly.

| Option | Meaning |
| --- | --- |
| `--config <path>` | Write the config file somewhere else |
| `--print` | Only show the config file it would write; create nothing |
| `--force` | Replace an existing config file with a fresh one, keeping a backup |

`init` never overwrites anything by accident: if the config file already
exists, it stops and suggests what to do instead.

With `--force`, the old file is first saved next to it as
`config.<time>.bak` (for example `config.20261031T213045Z.bak`, in UTC; an
earlier backup is never overwritten). tidepool then shows both paths and what
changed, with API keys hidden:

```console
$ tidepool init --force
Backed up ~/.config/tidepool/config.toml to ~/.config/tidepool/config.20261031T213045Z.bak
Wrote a fresh config to ~/.config/tidepool/config.toml

--- ~/.config/tidepool/config.20261031T213045Z.bak
+++ ~/.config/tidepool/config.toml
@@ -9,7 +9,7 @@
 version = 1
 
 [general]                           # settings for the whole run
-concurrency = 4                    # parallel yt-dlp downloads
+# concurrency = 2                   # parallel yt-dlp downloads
 ...

The stored config (revision 2) is unchanged; apply the new file with `tidepool config import` (until then, runs stop at the config check)
```

The settings tidepool uses don't change until you run `tidepool config import`.
If tidepool has no settings stored yet, the fresh file is applied right away.

If you deleted your config file, `tidepool init` writes back the settings
tidepool is using, so you can keep editing them.

## Configuration

Your settings live in one file, `~/.config/tidepool/config.toml`. Open it in
any text editor. Its sections:

| Section | What it's for |
| --- | --- |
| `[general]` | How many downloads run at once, and other settings for the whole run |
| `[tools.yt-dlp]`, `[tools.deno]` | Which yt-dlp and Deno to use and how they're kept up to date |
| `[schedule]` | How often to sync in the background |
| `[server]` | The web interface |
| `[category.<name>]` | Where videos go and how they're downloaded, named and filtered; see [Categories](#categories) |
| `[hooks.<name>]` | Your own commands, run at each stage of a download |
| `[sources.<name>]` | What to keep in sync |

A *source* is something to keep in sync: for now, a YouTube playlist. A
minimal one looks like this:

```toml
[sources.lofi]
url = "https://www.youtube.com/playlist?list=PLxxxxxxxx"
```

`url` is the playlist's address, as you'd copy it from your browser. A link to
a video inside a playlist works too, as long as it contains `list=`. The name
(`lofi` here) is up to you: lowercase letters, digits, `-` and `_`. By
default it's also the name of the source's folder (see `output_dir` below).

See [`examples/config.toml`](examples/config.toml) for a complete example.

### Categories

Most settings live in categories: where videos go (`media_dir`, `output_dir`,
`output_file`, …), the quality (`format`), filters, limits and
your YouTube API key. Every source uses `[category.default]`. Give a source
another category to change some of those settings for a group of sources;
the category only lists what differs from `default`:

```toml
[category.default]
media_dir = "~/Videos/youtube"
format = "bv*[height<=1080]+ba/b"
exclude = ['(?i)#shorts']

[category.music]
media_dir = "~/Music/youtube"            # music goes somewhere else
format = "ba/b"                          # audio only
ytdlp_args = ["-x", "--audio-format", "opus"]

[sources.lofi]
url = "https://www.youtube.com/playlist?list=PLxxxxxxxx"
category = "music"
```

Here `lofi` saves audio in `~/Music/youtube/lofi/`, and still skips `#shorts`
from `default`. A source's settings come from `[category.default]`, then its
category, then anything set on the source itself, so a single source can still
change any setting:

```toml
[sources.talks]
url = "https://www.youtube.com/playlist?list=PLyyyyyyyy"
cutoff = "none"                          # keep the whole back catalogue
```

Lists like `include`, `exclude` and `hooks` replace the inherited ones. Add
`inherit_rules = true` (or `inherit_hooks = true`) to add to them instead.
`{category}` can be used in paths, e.g. `output_dir = "{media_dir}/{category}/{source}/{title} [{id}]"`.

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

If you edit the file and forget to apply it, `run`, `index`, `download`,
`reset` and `filter test` stop and tell you what changed, so you never run with
settings you didn't mean:

```
error: ~/.config/tidepool/config.toml has changes that aren't applied yet:
  + youtube.api_key = "AIza…x1Qw"
Apply it with `tidepool config import`, or pass --skip-config-check to run with the stored config (revision 3).
```

Changes to comments or formatting don't count. Add `--skip-config-check` to run
with the settings tidepool already has. `tidepool config …` commands are never
stopped, since they're how you sort it out.

### Checking for mistakes

```sh
tidepool config check ~/.config/tidepool/config.toml
```

This shows every problem at once, with the line and column where it is:

```
config.toml:2:1 general.concurency: unknown key "concurency"
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

### Downloading

```sh
tidepool run
```

checks every source for new videos and downloads the ones your rules keep. Run
it as often as you like: videos already downloaded are never downloaded again.
Add `--source <name>` (or `-s`) to run only some sources.

By default, each video gets its own folder:

```
~/.local/share/tidepool/media/      # media_dir
  lofi/                             # the source's name
    2025-03-01 - Café Ñoño [dQw4w9WgXcQ]/
      Cafe Nono.mkv                 # the video
      Cafe Nono-poster.jpg          # the largest cover image available
      Cafe Nono.en.vtt              # subtitles, if you asked yt-dlp for them
      metadata.json                 # title, channel, dates, description, tags, chapters, …
```

Where everything goes is up to you. Each of these settings is a full path:

| Setting | Default | Notes |
| --- | --- | --- |
| `output_dir` | `"{media_dir}/{source}/{upload_date} - {title} [{id}]"` | The video's folder; the others refer to it as `{output_dir}` |
| `output_file` | `"{output_dir}/{normalized_title}.{ext}"` | The video; must contain `{ext}` (the file type yt-dlp downloaded) |
| `cover_file` | `"{output_dir}/{normalized_title}-poster.jpg"` | Ends in `.jpg`, `.png` or `.webp`, which picks the image format |
| `subtitle_file` | `"{output_dir}/{normalized_title}.{lang}.{ext}"` | Each subtitle; must contain `{lang}` and `{ext}` |
| `metadata_file` | `"{output_dir}/metadata.json"` | tidepool's description of the video |

A full path starts with `/`, `~`, an environment variable like `$HOME`,
`{media_dir}`, `{data_dir}` (tidepool's data folder, `~/.local/share/tidepool`)
or, for the files, `{output_dir}`. `{title}` is the title as on YouTube,
minus characters that aren't allowed in file names; `{normalized_title}` is
also turned into plain letters (`Café 東京` becomes `Cafe Dong Jing`). For
example, to keep every video of a source in one folder, with posters
somewhere else:

```toml
[category.default]
output_dir = "/nas/youtube/{source}"
cover_file = "/nas/posters/{id}.webp"
metadata_file = "{output_dir}/{id}.json"
```

Files can share a folder: tidepool only ever replaces or deletes the files it
placed for a video, never anything else in the folder.

Downloads in progress live in `tmp_dir` (default `"{data_dir}/tmp"`). Each file
is moved into place once the whole video is ready, and the metadata file
always comes last, so a video only counts as downloaded once everything is
there. When `tmp_dir` is on a different disk, each file is copied next to its
destination under a hidden name first, so it still appears all at once.

By default, `format` is `"bv*+ba/b/bv*/ba"`: the best video and the best audio
merged into one file; otherwise the best single file that has both; otherwise
video alone, or audio alone. So you get the best quality available, and still
get something when only part of it can be downloaded. To cap the resolution:
`format = "bv*[height<=1080]+ba/b"`.

To run tidepool on a schedule, call it from cron (or launchd, or a systemd
timer). `--config` applies your config file first if you changed it:

```
0 */6 * * *  tidepool run --config ~/.config/tidepool/config.toml
```

**When a download fails.** A video that can't be downloaded right now (network
trouble, YouTube asking to slow down) is tried again on later runs, waiting
longer each time, up to 5 attempts. Videos that can't be downloaded at all
(private, removed, members-only, age-restricted without `cookies_file`) are
marked unavailable and not tried again. Either way, the other videos still
download, and the command ends with exit code 3.

**Stopping.** Ctrl-C stops cleanly: unfinished downloads are discarded and
picked up again on the next run.

**Downloading only what's waiting.** `tidepool download` downloads videos
already found by `tidepool run` or `tidepool index`, without checking the
sources again. `--limit 5` downloads at most five; `--concurrency 4` runs four
downloads at once (default: `general.concurrency`, 2).

**Downloading a video again.** `tidepool reset <video id>` makes tidepool
download it again on the next run; add `--delete-files` to remove the current
copy first. The video id is the part after `v=` in its YouTube address, and
it's also in the folder name.

**The same video in several sources.** Each source gets its own folder. If a
video was already downloaded for another source in the same `format`, tidepool
reuses that file instead of downloading it again (`general.duplicates`):

| `duplicates` | What it does |
| --- | --- |
| `"hardlink"` (default) | Both folders share the same file on disk, using no extra space |
| `"symlink"` | The second folder links to the first one's files |
| `"copy"` | A full copy |
| `"download"` | Download it again |

### Tags, cover and chapters inside the file

tidepool can also write what it knows into the video file itself, so it stays
self-describing when you copy it elsewhere or add it to a media player. Turn on
what you want in the `embed` table, for every source or per category or source:

```toml
[category.default.embed]
metadata = true     # title, artist, date, description, …
cover = true        # the poster
chapters = true     # chapter markers
subtitles = true    # subtitles you asked yt-dlp for (with --write-subs)
```

| Option | What goes into the file |
| --- | --- |
| `metadata` | title, artist and album artist (the channel), genre (YouTube's category), date, description, the video's address, album (the source's title on YouTube) and track (its position in the source) |
| `cover` | the poster, the same image as `cover_file` |
| `chapters` | the video's chapters |
| `subtitles` | the subtitles; the separate subtitle files are kept too |

Embedding needs `ffmpeg` and never re-encodes: the video and audio are copied
as they are. Some file types can't hold everything: webm, opus and ogg files
get no cover, and only mkv, mp4 and webm files get subtitles. tidepool says
what it left out, and the poster and subtitle files are always saved anyway.
If embedding fails, you keep the file as downloaded, with a warning.

When the same video is in two sources and shares one file (see `duplicates`),
the file carries the album and track of the source that downloaded it last.

### Hooks

Hooks run your own commands at each stage of a download: to fix tags, convert
files, or send a notification. Define a hook once, then list it in `hooks` for
every source, a category or a single source:

```toml
[hooks.notify]
command = ["ntfy", "publish", "videos", "New: {title}"]

[category.default]
hooks = ["notify"]
```

`command` is the program and its arguments, run directly (no shell). Any
`{placeholder}` works in it (`{title}`, `{channel}`, `{output_file}`, …), and
the same values are in the environment too (`TIDEPOOL_TITLE`,
`TIDEPOOL_OUTPUT_FILE`, …). What a hook prints to its error output goes to
`~/.local/share/tidepool/logs/hooks/<video id>.log`.

A hook's `type` says when it runs:

| `type` | When | What it gets and returns |
| --- | --- | --- |
| `downloaded` | right after the download, before anything is embedded | the raw files, as JSON on its input; it may print new ones, e.g. after converting the video |
| `embed` | before tags and the cover go into the file (see `embed`) | the tags, cover, chapters and subtitles about to be embedded, as JSON; it prints them back, changed |
| `finished` (the default) | once the video's files are in place | nothing; what it prints is ignored |

Hooks of the same type run in the order you list them, each getting what the
previous one printed. Printing nothing means "no changes".

For example, to turn YouTube's `"Artist - Title"` titles into separate artist
and title tags (this one uses [`jq`](https://jqlang.org)):

```toml
[hooks.split-title]
type = "embed"
command = ["~/bin/split-title"]
```

```sh
#!/bin/sh
# ~/bin/split-title
jq '(.tags.title | capture("^(?<artist>.+?) - (?<title>.+)$")) as $m
    | .tags.artist = $m.artist | .tags.album_artist = $m.artist | .tags.title = $m.title'
```

In what an `embed` hook prints, a missing key keeps its value and `null`
removes it (a tag, the cover, all chapters). The `embed` settings still decide
what goes into the file: a hook can change the tags, but they're only written
with `metadata = true`.

| Option | Default | Meaning |
| --- | --- | --- |
| `type` | `"finished"` | `downloaded`, `embed` or `finished` |
| `timeout` | `"10m"` | stop the hook after this long |
| `on_failure` | `"fail"` | `fail`, `warn` or `retry` |
| `retries` | `0` | extra tries with `on_failure = "retry"` |
| `env` | `{}` | extra environment variables |
| `work_dir` | the video's folder | where the hook runs |

When a `downloaded` or `embed` hook fails, the download fails too and is tried
again on a later run; with `on_failure = "warn"`, tidepool carries on without
the hook's changes. When a `finished` hook fails, the video stays downloaded,
and `tidepool problems` tells you about it.

#### Running hooks again

Added a hook, or fixed one? Run hooks on videos you already have, without
downloading them again:

```sh
tidepool hooks run --all                     # every downloaded video
tidepool hooks run --source lofi             # one source (repeatable)
tidepool hooks run --category music          # every source in a category
tidepool hooks run dQw4w9WgXcQ okvideo01     # these videos
```

Each video runs its source's hooks, as a download would: `downloaded` and
`embed` hooks work on copies of the files, which are then embedded again and
put back in place (so a hook that converts a video replaces the old file);
`finished` hooks run on the files as they are. Add `--hook <name>` (repeatable)
to run only some hooks, and `--failed` to run only the ones that failed last
time:

```sh
tidepool hooks run --all --failed            # after fixing a broken hook
tidepool hooks run --category music --hook split-title
```

Running the same hooks twice is safe: the file gets the new tags, cover and
subtitles instead of a second copy of them.

### Problems

Some failures aren't about one video: an outdated yt-dlp that can't read
YouTube anymore, YouTube asking to confirm you're not a bot, a playlist that
was deleted. When tidepool runs into one of these repeatedly, it keeps a note
and shows it the next time you use tidepool, even if the failure happened in
a cron job:

```
tidepool problem #1 yt-dlp can't extract videos
   seen 4 times, 2026-10-31 12:00 UTC → 2026-10-31 18:00 UTC, yt-dlp 2026.10.01
   sources: lofi
   last error: Signature extraction failed: Some formats may be missing
   hint: yt-dlp 2026.10.01 may be outdated; tidepool checks for a newer yt-dlp right away; failed videos are retried on later runs
   (`tidepool problems` lists them; `tidepool problems ack 1` hides this one)
```

After that, commands only remind you with one line ("1 open problem"). A
problem goes away by itself once the same kind of work succeeds again, for
example after yt-dlp is updated.

| Command | What it does |
| --- | --- |
| `tidepool problems` | List open problems, with hints, and recently resolved ones |
| `tidepool problems ack <id>` | Hide a problem until it happens again |

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

An API key is optional. A future version will use it to check playlists for
new videos faster, with more details per video. Today tidepool uses yt-dlp for
everything. To add a key later, set it in the config file and apply the change:

```toml
[category.default.youtube]
api_key = "AIza..."
```

A category (or a single source) can use its own key, with its own quota
settings, e.g. under `[category.music.youtube]`. Quota is counted per key.

The config file is created so only you can read it. If you create it some
other way and it holds a key or token, `config check` warns you if other users
can read it.

### Tools

tidepool downloads [yt-dlp](https://github.com/yt-dlp/yt-dlp) and
[Deno](https://deno.com) the first time they're needed, and keeps them up to
date. Deno is the JavaScript runtime yt-dlp needs to download from YouTube.

- **yt-dlp** is checked for a new release at most once an hour. Before
  installing one, tidepool checks that it was signed by the yt-dlp project and
  that the file wasn't altered, so a tampered download is never used.
- **Deno** is checked once a day. tidepool installs the newest 2.x release
  (yt-dlp needs at least 2.3.0) and checks it against the checksum published
  with the release. Deno doesn't sign its releases, so this catches broken
  downloads, but not a release that was tampered with on GitHub itself.

| Command | What it does |
| --- | --- |
| `tidepool tools status` | Show each tool's version, where it is, and when tidepool last checked for updates |
| `tidepool tools update [yt-dlp \| deno]` | Check for new releases now and install them (both, or one) |
| `tidepool tools rollback <yt-dlp \| deno>` | Go back to the version you had before the last update. The release you rolled back from is skipped until a newer one comes out |

By default tidepool follows yt-dlp's *nightly* releases, which is what the
yt-dlp project recommends, since YouTube changes often. To follow the regular
releases instead, or check less often:

```toml
[tools.yt-dlp]
channel = "stable"         # stable | nightly | master
update_check = "1d"
```

#### Using your own

If you'd rather install and update them yourself:

```toml
[tools.yt-dlp]
managed = false
path = "/usr/local/bin/yt-dlp"

[tools.deno]
managed = false
path = "/opt/homebrew/bin/deno"   # or leave out path to use the deno on your PATH
```

tidepool then uses those copies as they are, and never updates them. To use
another JavaScript runtime instead of Deno (Node, for example), set
`managed = false` in `[tools.deno]` and pass yt-dlp's own option in a category:
`ytdlp_args = ["--js-runtimes", "node"]`.

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

## Seeing more (or less)

Add `-v` (`--verbose`) to any command to see what tidepool is doing: which
config revision it uses, where videos go, which yt-dlp runs, each source's
settings and filters, why each new video is kept or skipped, and each
download's progress, cover and destination:

```
$ tidepool run -v
DEBUG config revision 2, imported from ~/.config/tidepool/config.toml on 2026-10-31 20:48 UTC
DEBUG media in ~/.local/share/tidepool/media, work area ~/.local/share/tidepool/tmp
DEBUG sample: https://www.youtube.com/playlist?list=PLXLpuA-bwY4s (category default), format bv*[height<=360]+ba/b, output_dir {media_dir}/{source}/{upload_date} - {title} [{id}]
DEBUG yt-dlp 2026.10.29.232701 at ~/.local/share/tidepool/bin/yt-dlp-nightly (managed, nightly channel)
DEBUG sample: listed 1 video in 2.9s
DEBUG sample: new: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)” [dQw4w9WgXcQ]: keep (no include rules)
sample: 1 video (1 new): 1 to download
DEBUG sample: downloading “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)” [dQw4w9WgXcQ]
DEBUG sample: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)”: 36% of 5.7 MB at 38.2 MB/s, 0:00 left
DEBUG sample: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)”: cover 1280×720 from https://i.ytimg.com/vi_webp/dQw4w9WgXcQ/maxresdefault.webp
DEBUG sample: “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)”: saved as ~/.local/share/tidepool/media/sample/2009-10-25 - Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster) [dQw4w9WgXcQ]/Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster).mkv (12.3s)
sample: downloaded “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)” (9.1 MB)
```

`-vv` shows even more: your complete settings with every default filled in (as
`tidepool config check` prints them, with your API key and web interface token
hidden), every video's decision (not just new and changed ones), and the exact
yt-dlp commands and output.
`-q` (`--quiet`) shows errors only. Messages go to the terminal's error output,
so `tidepool run -v > out.txt` keeps the summary lines separate. With
`--log-format json`, messages are JSON lines, one per message.

## Where things are

| What | Where |
| --- | --- |
| Your config file | `~/.config/tidepool/config.toml` |
| Videos (unless a category sets `media_dir` or `output_dir`) | `~/.local/share/tidepool/media` |
| Downloads in progress (unless a category sets `tmp_dir`) | `~/.local/share/tidepool/tmp` |
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
| 3 | Something failed for some sources or videos, but the rest finished |
| 130 / 143 | Stopped by Ctrl-C / `kill` |
