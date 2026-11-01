# tidepool

tidepool keeps local copies of YouTube playlists in sync. For each video it
saves the video file, its details (title, channel, description, …), the
largest available cover image and any subtitles you want, and can write tags,
the cover and chapters into the video file itself. Rules decide which videos to
keep, and your own scripts can tidy up tags, convert files or send
notifications along the way.

> **Early days.** tidepool can keep your sources downloaded, run your own
> scripts and clean up when you ask it to. Running in the background (with
> automatic cleanup) and the web interface aren't available yet.

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
installed Deno 2.9.8
sample: 1 video (1 new): 1 to download
sample: downloaded “Rick Astley - Never Gonna Give You Up (Official Video) (4K Remaster)” (243.8 MB)
downloaded 1 video (243.8 MB)
```

The first run also downloads the two tools tidepool uses (see [Tools](#tools)).
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

A *source* is something to keep in sync: one or more YouTube playlists or
whole channels, saved together. A minimal one looks like this:

```toml
[sources.lofi]
urls = ["https://www.youtube.com/playlist?list=PLxxxxxxxx"]

[sources.supercombo]
urls = ["https://www.youtube.com/@supercombooficial"]
```

`urls` lists the addresses as you'd copy them from your browser. It's always
a list, even with a single address:

- **A playlist:** its page, or a link to a video inside it (as long as it
  contains `list=`).
- **A channel:** `youtube.com/@name`, `youtube.com/channel/UC…`, or the older
  `youtube.com/c/name` and `youtube.com/user/name`. A channel source has
  everything the channel uploads: videos, Shorts and past live streams. Use
  your rules to keep only what you want.

With several addresses, the source keeps everything they have together: the
same folder, the same rules, and a video found in more than one is downloaded
once. The source takes its title from the first address. If one address can't
be read (a deleted playlist, a network error), the others are still checked,
and the videos only known from that address are left as they are until it
works again:

```toml
[sources.supercombo]
urls = [
  "https://www.youtube.com/@supercombooficial",
  "https://www.youtube.com/playlist?list=PLzzzzzzzz",   # a fan's playlist of live shows
]
```

An address can only be in one source.

The name (`lofi` here) is up to you: lowercase letters, digits, `-` and `_`. By
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
urls = ["https://www.youtube.com/playlist?list=PLxxxxxxxx"]
category = "music"
```

Here `lofi` saves audio in `~/Music/youtube/lofi/`, and still skips `#shorts`
from `default`. A source's settings come from `[category.default]`, then its
category, then anything set on the source itself, so a single source can still
change any setting:

```toml
[sources.talks]
urls = ["https://www.youtube.com/playlist?list=PLyyyyyyyy"]
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
tidepool config check
```

This checks your config file (`~/.config/tidepool/config.toml`, or the path you
give it) and the settings tidepool has stored, and tells you whether the two
differ, so you know when your edits still need `tidepool config import`. If
the stored settings no longer work with your version of tidepool (after an
update renamed a setting, for example), it says so: fix the file and import it.

It shows every problem at once, with the line and column where it is:

```
config.toml:2:1 general.concurency: unknown key "concurency"
config.toml:20:12 sources.lofi.cleanup.max_size: "20 gigs" is not a valid byte size (e.g. "20GiB", "500MB")
```

It also checks that `ffmpeg`, yt-dlp and Deno are available and that the video
folder is writable. When everything is fine it prints nothing (add `-v` to see
what it checked); it only speaks up about mistakes and warnings. A file with
mistakes is never applied.

Editors that understand the `#:schema` line at the top of the file (for
example VS Code with the *Even Better TOML* extension) suggest setting names
and flag mistakes as you type.

### History and undo

Every applied change is kept, so you can always go back:

| Command | What it does |
| --- | --- |
| `tidepool config` | Show the current settings, as you wrote them |
| `tidepool config --include-defaults` | The same, with every setting you left out filled in with its default |
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

Videos can share a folder: tidepool only ever replaces or deletes its own files
for a video, never anything else you keep there.

You never see half-finished downloads: while a video downloads, its files wait
in a work folder (`~/.local/share/tidepool/tmp`, or `tmp_dir`), and they only
show up in your folders once everything is ready, even when that's on another
disk.

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

**Downloading, then finishing.** Each video is downloaded into a work folder
first, then finished: its hooks run, tags and the cover are written into the
file, and everything is saved into your library. Finishing happens alongside
the downloads, so a slow hook (converting a video, say) never holds up the
next download. `general.process_concurrency` says how many videos are finished
at once (default 1, since converting videos keeps the computer busy); your
library only ever gets finished videos.

**Stopping.** Ctrl-C stops cleanly: no new downloads start, and the ones in
progress get a moment to wind down (10 seconds, or `general.shutdown_timeout`)
before they're stopped. Videos that finished in the meantime are kept;
unfinished downloads are discarded and downloaded again on the next run. A
video that was downloaded but not finished yet is finished on the next run,
without downloading it again. Press Ctrl-C a second time to stop right away.
Hooks and anything else tidepool started stop with it, so nothing keeps running
in the background.

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
| `tags` | your own tags, added to (or replacing) the ones above; needs `metadata = true` |

`tags` takes fixed text or `{placeholders}` (not file paths), and an empty
value leaves a tag out, also taking it off the video and audio tracks inside the
file (e.g. `HANDLER_NAME = ""`; mp4 files always keep that one). Set it for a category, or per source, for example to
mark whose music videos a source holds:

```toml
[sources.afi.embed]
tags = { MUSICBRAINZ_ARTISTID = "…", album_artist = "{channel}", genre = "" }
```

Tags add up: a source's `tags` are added to its category's and
`[category.default]`'s, and a tag set again replaces the earlier one, so you can
set shared tags once and one id per source. Not every file type keeps every
tag: mp4 files only keep the usual ones (title, artist, album, date, …), so a
tag like `MUSICBRAINZ_ARTISTID` needs mkv. tidepool tells you when a tag
couldn't be saved.

Embedding needs `ffmpeg` and never re-encodes: the video and audio are copied
as they are. Some file types can't hold everything: webm, opus and ogg files
get no cover, and only mkv, mp4 and webm files get subtitles. tidepool says
what it left out, and the poster and subtitle files are always saved anyway.
If embedding fails, you keep the file as downloaded, with a warning.

When the same video is in two sources and shares one file (see `duplicates`),
the file carries the album and track of the source that downloaded it last.

#### Embedding into videos you already have

Changing `embed` only affects new downloads. To bring videos you already have
up to date, for example after turning `cover` on or adding an `embed` hook, use
`tidepool embed`. It picks videos the same way as `tidepool hooks run`:

```sh
tidepool embed --all                     # every downloaded video
tidepool embed --source lofi             # one source (repeatable)
tidepool embed --category music          # every source in a category
tidepool embed dQw4w9WgXcQ okvideo01     # these videos
```

It runs the source's `embed` hooks, then writes what your `embed` settings
turn on. Turning an option off doesn't take anything out of the files you
already have; add `--remove-disabled` for that, e.g. with `cover = false`,
`tidepool embed --all --remove-disabled` takes the covers out:

```text
lofi: “Some title”: embedded tags, chapters; removed cover
embedded 1 video
```

### Missing files

Each run checks that every file of the videos you've downloaded is still
there: the video, the poster, the subtitles and the metadata file. By default
it only tells you:

```
1 video has missing files; set redownload_missing = true to restore them
```

With `redownload_missing = true` (for every source, a category or one source),
tidepool restores only what's missing: a deleted poster is fetched again,
subtitles are fetched again without the video, the metadata file is written
again, and only a missing video is downloaded again. Files a hook deleted on
purpose (see below) never count as missing.

### Hooks

Hooks run your own commands while tidepool works: to send a notification, fix
up tags, convert videos, or move them somewhere else. Define a hook once, then
list it in `hooks` for every source, a category or a single source:

```toml
[hooks.notify]
command = ["ntfy", "publish", "videos", "New: {title}"]

[category.default]
hooks = ["notify"]
```

`command` is the program and its arguments. Any `{placeholder}` works in it
(`{title}`, `{channel}`, `{output_file}` for the video file, …), and the same
values are available to scripts as environment variables (`TIDEPOOL_TITLE`,
`TIDEPOOL_OUTPUT_FILE`, …). Hooks run in the order you list them. What a hook
prints as errors is kept in `~/.local/share/tidepool/logs/hooks/<video id>.log`
(started over past 1 MiB, keeping the previous one as `.log.1`), and shown as it
happens with `-v`.

A hook's `type` says when it runs:

| `type` | When | Good for |
| --- | --- | --- |
| `prepare` | right after the download, before anything else | fixing the title, description or channel; choosing where the files go |
| `downloaded` | after `prepare` hooks, on the downloaded files | converting the video to another format |
| `embed` | just before tags and the cover are written into the file (see [Tags, cover and chapters](#tags-cover-and-chapters-inside-the-file)) | fixing titles, artists and other tags |
| `finished` (the default) | once the video is saved | notifications, copying the video elsewhere |
| `before_cleanup` | before [cleanup](#cleaning-up) removes a video | keeping videos you still want, telling a media server |

| Option | Default | Meaning |
| --- | --- | --- |
| `type` | `"finished"` | `prepare`, `downloaded`, `embed`, `finished` or `before_cleanup` |
| `timeout` | `"10m"` | stop the hook after this long |
| `on_failure` | `"fail"` | `fail`, `warn` or `retry` |
| `retries` | `0` | extra tries with `on_failure = "retry"` |
| `env` | `{}` | extra environment variables |
| `work_dir` | the video's folder | where the hook runs (`prepare`, `downloaded` and `embed` hooks start in a work folder, since the video isn't saved yet) |

**When a hook fails.** If a `prepare`, `downloaded` or `embed` hook fails, the video
isn't saved; it's tried again on a later run (with `on_failure = "warn"`,
tidepool carries on without the hook's changes instead). If a `finished` hook
fails, the video stays saved and the hook is tried again on later runs of
`tidepool run` and `tidepool download`, waiting a little longer each time: 15
seconds at first, about 40 minutes by the last try. After 8 more tries (about an
hour and a half) it stops trying, and `tidepool problems` tells you about it, so
you can run the hook again once it's fixed (see below). Every try is
remembered, so you can see what went wrong each time.

#### Hooks that change things

A hook receives a description of the video (its details and where its files
go for `prepare` hooks, its files for `downloaded` hooks, the tags about to be
written for `embed` hooks) as JSON on its input, and can print a changed version
on its output. Printing nothing means "no changes", and a hook only
needs to print what it changes. When several hooks of the same type run, each
one sees what the previous one printed.

**Fixing details and choosing where files go** (`prepare`). A `prepare` hook
gets the video's `details` (`title`, `description`, `channel`, `upload_date`,
`tags` and `categories`) and its `paths` (`output_dir`, `output_file`,
`cover_file`, `subtitle_file` and `metadata_file`, as full paths with `{ext}`
and `{lang}` still in them), and prints what it changes:

```json
{"details": {"title": "17 Crimes", "channel": "AFI"},
 "paths": {"output_dir": "/videos/music/AFI"}}
```

Everything after it uses the new details: file names (`{title}`), the tags
written into the file, `metadata.json` and other hooks. Paths it doesn't change
follow the new details, so a new title renames the files. Changed paths must
stay inside the media folder, and keep `{ext}` (and `{lang}` for subtitles).
`metadata.json` keeps YouTube's version of whatever changed under `original`,
and your include and exclude rules keep seeing YouTube's details. `prepare`
hooks only run for new downloads, not with `tidepool hooks run`.

**Fixing tags** (`embed`). For example, to turn YouTube's `"Artist - Title"`
titles into separate artist and title tags (this one uses
[`jq`](https://jqlang.org)):

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

Besides `tags`, an `embed` hook can change the `cover`, `chapters` and
`subtitles` that go into the file; setting one to `null` leaves it out. A tag
that ends up empty (`""`) is left out too. Your
`embed` settings still decide what's written: changed tags only go into the
file with `metadata = true`.

**Converting** (`downloaded`). A hook that converts the video prints where the
new file is, and tidepool saves that one instead:

```json
{"files": {"media": "/path/it/was/given/media.mp4"}}
```

`finished` hooks run on the saved files, and what they print is ignored. To
change where files go, use a `prepare` hook.

#### Running hooks again

Added a hook, or fixed one? Run hooks on videos you already have, without
downloading them again:

```sh
tidepool hooks run --all                     # every downloaded video
tidepool hooks run --source lofi             # one source (repeatable)
tidepool hooks run --category music          # every source in a category
tidepool hooks run dQw4w9WgXcQ okvideo01     # these videos
```

Each video runs its source's hooks (except `prepare`), just like when it was downloaded: tags
are fixed and written again, a converted video replaces the old one, and
`finished` hooks run on the saved files. Your files stay as they are if
something fails. Add `--hook <name>` (repeatable) to run only some hooks, and
`--failed` to run only the ones that failed last time, including those that
ran out of tries (they get a fresh set of tries if they fail again):

```sh
tidepool hooks run --all --failed            # after fixing a broken hook
tidepool hooks run --category music --hook split-title
```

Running the same hooks twice is safe: the file gets the new tags, cover and
subtitles instead of a second copy of them. Videos you pick that don't use any
of those hooks are skipped.

### Cleaning up

To keep a source from growing forever, set limits in `cleanup`, for every
source, a category or one source, then run `tidepool cleanup`:

```toml
[category.default.cleanup]
max_items = 50        # keep the 50 most recent videos
max_age = "30d"       # remove videos downloaded more than 30 days ago
```

| Option | Default | Removes |
| --- | --- | --- |
| `max_items` | `0` (no limit) | the oldest videos beyond this many |
| `max_size` | `""` (no limit) | the oldest videos until the source fits, e.g. `"20GiB"` |
| `max_age` | `""` (no limit) | videos downloaded longer ago than this, e.g. `"30d"` |
| `unmatched` | `"keep"` | with `"remove"`: videos your `include`/`exclude` rules (or duration limits, `cutoff`) no longer match |
| `marker_file` | `""` | videos whose folder has this file, e.g. `".watched"` (made by you or a media server) |
| `keep_latest` | `0` | nothing: it protects the newest N videos from all of the above |
| `order` | `"oldest_downloaded"` | who's oldest for `max_items` and `max_size`: `oldest_downloaded`, `oldest_published` or `source_position` (furthest down the playlist or channel first) |

A video goes when any limit says so, unless `keep_latest` protects it or a
`before_cleanup` hook keeps it. With `max_items`, tidepool also only downloads
the first that many videos of the source, in the playlist's order (newest first
for a channel), instead of downloading everything and removing the rest; the
others show up as skipped, "beyond max_items".

What's removed:

| Option | Default | |
| --- | --- | --- |
| `keep` | `"metadata"` | `metadata`: the video and its subtitles go, `metadata.json` and the poster stay; `nothing`: everything goes |
| `delete` | `"trash"` | `trash`: files go to the trash, and you can bring them back; `now`: they're deleted right away |
| `trash_for` | `"7d"` | how long files stay in the trash before they're deleted for good |

Only the files tidepool saved for the video are removed, never other files in
its folder, and the folder goes once it's empty. A cleaned-up video isn't
downloaded again (`tidepool reset <video id>` makes it downloadable again).

```sh
tidepool cleanup --dry-run     # what would go (and how many stay, per source), without removing anything
tidepool cleanup               # every source (or --source <name>)
tidepool trash list            # what's in the trash, and until when
tidepool trash restore dQw4w9WgXcQ
tidepool trash empty           # delete everything in the trash now
```

The trash is in `~/.local/share/tidepool/trash` unless a category sets
`trash_dir`. If your videos are on another disk (a NAS, say), moving them there
means copying them; `trash_dir = "{media_dir}/.trash"` keeps the trash on the
same disk, where moving is instant. What's been in the trash for `trash_for` is
deleted for good the next time you run `tidepool run`, `download` or
`cleanup`.

**Keeping videos with a hook.** A `before_cleanup` hook runs before each
video goes, with its files and the reason as JSON on its input. Printing
`{"keep": true}` keeps the video this time; printing nothing lets it go. Every
`before_cleanup` hook gets asked, and if one fails or times out the video is
kept too, and `tidepool problems` tells you. A video whose `finished` hooks
haven't run yet (or are waiting to be tried again) is also kept for now.

### Problems

Some failures aren't about one video: an outdated yt-dlp that can't read
YouTube anymore, YouTube asking to confirm you're not a bot, a playlist that
was deleted, a hook that keeps failing. When tidepool runs into one of these repeatedly, it keeps a note
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
urls = ["https://www.youtube.com/playlist?list=PLxxxxxxxx"]
include = ['(?i)\blive\b']                 # only videos with "live" in the title
exclude = ['(?i)\b(trailer|teaser)\b']     # but never trailers or teasers
```

A video is kept when it matches at least one `include` rule (or there are
none) and no `exclude` rule.

A rule can also look at one field, by pattern or by exact value:

```toml
include = [{ field = "channel", pattern = '(?i)^lofi girl$' }]
exclude = [{ field = "kind", value = "short" }, { field = "kind", value = "live" }]
```

The fields are `title`, `description`, `channel`, `tags` and `id`, plus `kind`:
whether a video is a regular `video`, a `short` or a `live` stream (past ones;
a scheduled stream is decided once it has aired). A Premiere, a video
YouTube airs at a set time, is a `video`. `kind` only works with `value`. It's
handy for channels, which include everything they upload: the example above
keeps only regular videos. tidepool asks YouTube which videos are Shorts, so
it's exact, not a guess from their length; it only asks for videos of 3
minutes or less, and only once per video. Telling a past stream from a
Premiere takes a closer look at the video, so those are decided just before
download.

To see why a video was skipped (or kept), test it by its id: tidepool shows
what it knows about the video and each rule's result, for every source it's
in (or the one you give with `--source`):

```sh
tidepool filter test dJkbfKpZdWM
```

```
dJkbfKpZdWM “Supercombo, @kamaitachi - Infame (Clipe Oficial)”
  channel: Supercombo
  kind: video
  duration: 4m26s
  published: 2023-03-31
  ...
source supercombo (pending)
  exclude[0] "kind = \"short\"" on kind: no match
  exclude[1] "kind = \"live\"" on kind: no match
decision: keep (no include rules)
```

Or try a title against a source's rules:

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

An API key is optional, but worth it: with one, tidepool checks your sources
through YouTube's official API instead of yt-dlp. It's faster, and it gets the
full details of each video (exact dates, full descriptions, tags, every
thumbnail size), so rules on descriptions or tags don't have to wait for the
download.

```toml
[category.default.youtube]
api_key = "AIza..."
```

Get a free key from the [Google Cloud console](https://console.cloud.google.com/apis/credentials)
with the *YouTube Data API v3* enabled, put it in your config file and apply
the change with `tidepool config import`.

**Quota.** Google gives each project 10,000 units a day. Checking a source
costs 1 unit for every 50 videos in it, plus 1 unit for every 50 videos tidepool
hasn't seen before; a 1,000-video playlist costs about 20 units once nothing is
new. tidepool keeps count, warns you at 80% (`quota_warn_at`), and stops using
the key before Google would refuse it. The count resets at midnight Pacific
time, like Google's. If Google granted your project more, raise `daily_quota`.

**When the key can't be used** (the quota is used up, YouTube rejects the key,
or YouTube's API is having trouble), tidepool checks your sources with yt-dlp
instead and tells you about it in `tidepool problems`. To stop instead, set
`fallback_to_ytdlp = false`: tidepool then ends with exit code 4.

| Setting | Default | Meaning |
| --- | --- | --- |
| `api_key` | none | Your key; without one, tidepool uses yt-dlp |
| `daily_quota` | `10000` | Your project's daily quota |
| `quota_warn_at` | `0.8` | Warn once this share of the quota is used |
| `fallback_to_ytdlp` | `true` | Use yt-dlp when the key can't be used |
| `requests_per_second` | `5` | How fast tidepool may call the API |

A category (or a single source) can use its own key, with its own quota
settings, e.g. under `[category.music.youtube]`. Quota is counted per key.

The config file is created so only you can read it. If you create it some
other way and it holds a key or token, `config check` warns you if other users
can read it.

### Tools

tidepool uses two tools, and takes care of both: it downloads them the first
time they're needed and keeps them up to date.

- **[yt-dlp](https://github.com/yt-dlp/yt-dlp)** does the downloading. tidepool
  looks for a new release at most once an hour, and only installs one that the
  yt-dlp project signed, so a tampered download is never used.
- **[Deno](https://deno.com)** is a helper yt-dlp needs to download from
  YouTube. tidepool looks for a new release once a day, and checks each
  download against the fingerprint Deno publishes with it. That catches broken
  downloads; since Deno doesn't sign its releases, it can't catch a release
  that was tampered with at the source.

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

`-vv` shows even more: every source's complete settings, with every default
filled in (your API key and web interface token hidden), every video's decision (not just new and changed ones), and the exact
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
| Cleaned-up files, until they're deleted for good (unless a category sets `trash_dir`) | `~/.local/share/tidepool/trash` |
| What your hooks printed as errors | `~/.local/share/tidepool/logs/hooks/` |
| The yt-dlp and Deno tidepool manages | `~/.local/share/tidepool/bin/` |
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
| 3 | Something failed for some sources, videos or hooks, but the rest finished |
| 4 | The YouTube API couldn't be used and `fallback_to_ytdlp = false` |
| 130 / 143 | Stopped by Ctrl-C / `kill` |
