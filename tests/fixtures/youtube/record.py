#!/usr/bin/env python3
"""Record real YouTube Data API responses as test fixtures.

Makes the calls tidepool makes, against public examples, and saves each
response (status, ETag, body) under tests/fixtures/youtube/recorded/, plus an
index.json that maps each request (without the key) to its file. The key is
only ever sent to Google: it's read from YOUTUBE_API_KEY, never written, and
any copy of it in a response is replaced with REDACTED.

Costs about 30 quota units. Run from the repository root:

    YOUTUBE_API_KEY=... python3 tests/fixtures/youtube/record.py
"""

import json
import os
import pathlib
import sys
import urllib.error
import urllib.parse
import urllib.request

API = "https://www.googleapis.com/youtube/v3"
OUT = pathlib.Path("tests/fixtures/youtube/recorded")

# Public examples.
PLAYLIST = "PLfSl2rOZ6Zto"  # Scatolove: 10 videos, one page
HANDLE = "@supercombooficial"  # Supercombo: ~590 uploads, ~12 pages
CHANNEL_ID = "UC5eeDHYjTWF9pPWYDKpVMYQ"  # the same channel, by id

PLAYLIST_PARTS = "snippet,contentDetails,status"
VIDEO_PARTS = "snippet,contentDetails,status,liveStreamingDetails"
CHANNEL_PARTS = "snippet,contentDetails"

key = os.environ.get("YOUTUBE_API_KEY", "")
if not key:
    sys.exit("set YOUTUBE_API_KEY (an API key with the YouTube Data API v3 enabled)")

index = []
units = 0


def call(name, endpoint, params, etag=None, use_key=None):
    """GET one API endpoint; save the response as <name>.json."""
    global units
    query = dict(params, key=use_key if use_key is not None else key)
    url = f"{API}/{endpoint}?{urllib.parse.urlencode(query)}"
    request = urllib.request.Request(url, headers={"Accept": "application/json"})
    if etag:
        request.add_header("If-None-Match", etag)
    try:
        with urllib.request.urlopen(request) as response:
            status, headers, body = response.status, response.headers, response.read()
    except urllib.error.HTTPError as error:
        status, headers, body = error.code, error.headers, error.read()
    units += 1
    text = body.decode("utf-8", "replace").replace(key, "REDACTED")
    saved = {
        "request": {
            "endpoint": endpoint,
            "params": params,  # without the key
            "if_none_match": etag,
        },
        "status": status,
        "etag": headers.get("ETag"),
        "body": json.loads(text) if text.strip().startswith("{") else text,
    }
    (OUT / f"{name}.json").write_text(json.dumps(saved, indent=2, ensure_ascii=False) + "\n")
    index.append({"name": name, **saved["request"], "status": status})
    print(f"{name}: HTTP {status}", file=sys.stderr)
    return saved


def playlist_pages(prefix, playlist_id):
    """Every page of a playlist; returns all video ids."""
    ids, token, page = [], None, 1
    while True:
        params = {"part": PLAYLIST_PARTS, "maxResults": "50", "playlistId": playlist_id}
        if token:
            params["pageToken"] = token
        saved = call(f"{prefix}-page-{page}", "playlistItems", params)
        body = saved["body"]
        ids += [item["contentDetails"]["videoId"] for item in body.get("items", [])]
        token = body.get("nextPageToken")
        if not token:
            return ids
        page += 1


def video_batches(prefix, ids):
    for i in range(0, len(ids), 50):
        batch = ids[i : i + 50]
        call(f"{prefix}-videos-{i // 50 + 1}", "videos", {"part": VIDEO_PARTS, "id": ",".join(batch)})


OUT.mkdir(parents=True, exist_ok=True)

# A one-page playlist, its details, and the same page again with its ETag
# (does YouTube answer 304, and does that cost quota?).
ids = playlist_pages("playlist", PLAYLIST)
video_batches("playlist", ids)
first = json.loads((OUT / "playlist-page-1.json").read_text())
call(
    "playlist-page-1-unchanged",
    "playlistItems",
    {"part": PLAYLIST_PARTS, "maxResults": "50", "playlistId": PLAYLIST},
    etag=first["etag"] or first["body"].get("etag"),
)

# A channel: by handle and by id, then every page of its uploads.
handle = call("channel-by-handle", "channels", {"part": CHANNEL_PARTS, "forHandle": HANDLE})
call("channel-by-id", "channels", {"part": CHANNEL_PARTS, "id": CHANNEL_ID})
uploads = handle["body"]["items"][0]["contentDetails"]["relatedPlaylists"]["uploads"]
ids = playlist_pages("uploads", uploads)
video_batches("uploads", ids)

# Errors: a playlist that doesn't exist, and a key YouTube rejects.
call("playlist-not-found", "playlistItems", {"part": PLAYLIST_PARTS, "maxResults": "50", "playlistId": "PLdoesnotexist000"})
call("key-invalid", "playlistItems", {"part": PLAYLIST_PARTS, "maxResults": "50", "playlistId": PLAYLIST}, use_key="AIzaNotARealKey000000000000000000000000")

(OUT / "index.json").write_text(json.dumps(index, indent=2) + "\n")
for path in OUT.glob("*.json"):
    if key in path.read_text():
        sys.exit(f"the key leaked into {path}; not keeping it")
print(f"recorded {len(index)} responses (~{units} quota units) in {OUT}", file=sys.stderr)
