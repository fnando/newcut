# newcut

newcut looks at your music libraries and tells you, for every artist in
them, which albums you have, which you don't, and what's new or coming. It
reads the MusicBrainz ids that [MusicBrainz Picard](https://picard.musicbrainz.org)
writes into your files, checks each artist's discography on MusicBrainz, and
watches ListenBrainz's fresh releases for anything new. A web page
shows it all, and an Atom feed tells your feed reader when something comes
out.

> **Early days.** newcut scans, checks, and keeps doing it in the
> background, with a web page to keep an eye on it and edit your config. It
> doesn't download anything.

Works on Linux and macOS (Apple Silicon).

## Requirements

Music tagged with MusicBrainz ids (`MUSICBRAINZ_RELEASEGROUPID` or
`MUSICBRAINZ_ALBUMID`, and `MUSICBRAINZ_ALBUMARTISTID`), as Picard writes
them. FLAC, MP3, M4A/ALAC/AAC, Ogg Vorbis, Opus, WavPack, APE, WAV, AIFF, DSF
and Musepack all work. Files without them are listed as untagged.

## Installation

TBD.

## Getting started

```sh
newcut init --library ~/Music --library /Volumes/nas/flac
newcut scan       # read your libraries and look up their artists
newcut sync       # check discographies and fresh releases
newcut missing    # what you don't have
```

Or leave it running, with the web interface on <http://127.0.0.1:55460>:

```sh
newcut run
```

## How it works

1. **Scanning.** Every audio file in your libraries is read once; after that,
   only files whose size or modification time changed are read again, and
   files that are gone are forgotten. A library that isn't there (an
   unmounted disk) keeps what was found in it before.
2. **Artists.** An album's artists are its album artists
   (`MUSICBRAINZ_ALBUMARTISTID`), or else its track artists. MusicBrainz's
   special artists (Various Artists, [unknown], …) are never followed. Each
   artist is looked up once on MusicBrainz, for their name and their
   members: a group named after its members ("K-Salaam &
   Beatnick", whose members are K-Salaam and Beatnick) is a joined artist.
   It's hidden, and its albums and releases are its members'.
3. **What you have.** An album is yours when a file has its release group
   id, or the id of one of its releases (which newcut looks up once).
4. **Discographies.** Each artist's release groups are checked on MusicBrainz
   every `schedule.artist`. Which ones count is up to `[discography]`:
   albums and EPs by default, without compilations, live albums, remixes and
   the like.
5. **What's new.** An artist's first check is the baseline: nothing it finds
   counts as new. After that, a release group is new when it's found later
   and came out within `schedule.new_window` (60 days), or is still to come.
   ListenBrainz's fresh releases (checked every `schedule.fresh`, in one
   request for everyone) often see releases before the discography check
   does.
6. **Ignoring.** Don't want something? Ignore it, and it leaves Missing, New
   and the feed. Unfollow an artist to leave out all of theirs.

musicbrainz.org gets at most one request a second, as it asks, so the first
checks of a big library take a while (about a second per artist and per
release looked up). They're spread out after that.

## Configuration

The config file is `$NEWCUT_CONFIG_DIR/config.toml`, or
`$XDG_CONFIG_HOME/newcut/config.toml` (`~/.config/newcut/config.toml`) when
`NEWCUT_CONFIG_DIR` isn't set; `--config` (or `NEWCUT_CONFIG`) points
elsewhere. `newcut init` writes one with every setting explained; see
[examples/config.toml](examples/config.toml). Every key but `version` is
optional.

```toml
version = 1
libraries = ["~/Music", "/Volumes/nas/flac"]

[musicbrainz]
contact = "you@example.com"   # for the User-Agent, as MusicBrainz asks

[discography]
primary_types = ["Album", "EP", "Single"]
secondary_types = ["Live"]
exclude = ['(?i)\bao vivo\b']   # titles to leave out (regular expressions)

[schedule]
artist = "3d"
```

Covers come from the Cover Art Archive. To show the ones in your album
folders instead (`cover.jpg` or `folder.jpg`, in any case), turn on
`covers.local`; albums without one, and those you don't have, still come from
the Cover Art Archive. With `covers.local`, artists' pictures come from
their folder (the one their album folders are in) when it has an
`artist.jpg` or `folder.jpg`; otherwise, and without it, they get their
initial:

```toml
[covers]
local = true
files = ["cover.jpg", "folder.jpg", "cover.png"]
artist_files = ["artist.jpg", "folder.jpg"]
```

Pages get small copies of these (250 pixels on the shorter side), made the
first time each is shown and kept in newcut's cache folder
(`~/.local/share/newcut/cache/thumbs`); a picture that changes gets a new
one.

To download what you're missing with
[slskd](https://github.com/slskd/slskd), give newcut its address and an API
key (from `web.authentication.api_keys` in slskd's config). A missing album's
title then leads to a page that searches Soulseek for it: pick a folder (the
best ones come first: free upload slots, then lossless, then the fastest) and
slskd downloads it.

```toml
[slskd]
enabled = true
server = "http://localhost:5030"
api_key = "$SLSKD_API_KEY"
downloads_dir = "/srv/slskd/downloads"                 # slskd's, as newcut sees it
output_dir = "~/Music/%{artist}/%{album} (%{year})"    # where albums go
```

Search results are matched against the album's releases on MusicBrainz:
the same number of tracks, each within 3 seconds of the release's (or, when
lengths are missing, matching titles). Matches come first, and the file list
under each folder shows which track each file is.

With `downloads_dir` and `output_dir`, newcut has slskd put each download
in a folder of its own, and the `importer` service files the finished ones:
those that match a release exactly are tagged as MusicBrainz Picard tags
them, named like `1. Title.flac` (`1-01. Title.flac` on multi-disc
releases), moved under `output_dir` with the release's `cover.jpg`, and
scanned. The rest wait on the Downloads page: tag one as a release you pick
(its files in track order), or deal with it yourself and say so.
`output_dir`'s placeholders: `%{artist}`, `%{artist_sort}`, `%{album}`,
`%{year}` (the first release's), `%{date}`, `%{release_year}`, `%{country}`,
`%{media}`, `%{label}`, `%{catalog_number}`, `%{type}`, `%{releaseid}` and
`%{releasegroupid}`.

Editors that understand `#:schema` (VS Code with Even Better TOML, Zed,
Helix, …) check and complete it as you type; `newcut config schema` prints
the schema.

### Checking for mistakes

```sh
newcut config check
```

Mistakes say where they are (`config.toml:12:9: unknown field …`) and exit
with 2.

### Applying changes

The running daemon notices a changed config file within a few seconds and
uses it as a new revision once it checks out; with mistakes, it keeps the
previous one and says why in its log. No restart needed (a new
`server.listen` or `token` restarts the web interface). The web interface's
Config page edits the file too: Check says what your edits change, setting
by setting, with a text diff. It keeps every revision, so you can see what
rolling back to one would change, and roll back. The token is masked there
(`my-l…oken`); leave it as it is to keep it. `newcut config show` prints the
revision in use (`--show-secrets` with the token), and `newcut config diff`
what the file changes from it.

## The web interface

`newcut run` serves it on `server.listen` (`127.0.0.1:55460`):

- **Dashboard**: what's running now, new and upcoming releases, your
  libraries, and each service (run it now, pause it, resume it).
- **Latest releases**: everything the artists you follow released (or will),
  newest first, a page at a time; filter by new, upcoming, owned or
  missing, or search.
- **Artists**: everyone in your libraries with what you have, what's missing
  and what's coming; each artist's page lists their discography with what
  you have and where. To follow an artist you
  don't have yet, search MusicBrainz from there (a mirror without a search
  server works too).
- **Missing**: everything you don't have, across artists.
- **Untagged**: folders with files Picard hasn't tagged.
- **Config**: the editor and its history.

To reach it from other machines, listen on another address and set a token
(newcut won't listen beyond this machine without one):

```toml
[server]
listen = "0.0.0.0:55460"
token = "a long random string"
```

Pages then ask for the token once (or take `Authorization: Bearer <token>`);
the feeds stay open. To keep it out of the file, name an environment
variable instead: `token = "$NEWCUT_TOKEN"` (or `"${NEWCUT_TOKEN}"`), read
when the daemon starts. `newcut config check` warns when that variable isn't
set, or when the file has the token itself and other users can read it.

### Secrets in an env file

Variables don't have to be in your shell: newcut first reads
`~/.config/newcut/env` (in `$NEWCUT_CONFIG_DIR`), `NAME=value` lines, when
it's there; `--env-file <FILE>` reads another one (which must be there).
Variables already set win over the file. `newcut init` writes a commented
one, readable only by you; `newcut config check` warns when others can read
it. The daemon reads it only when it starts: after changing it, stop
`newcut run` and start it again (`newcut daemon restart` restarts its
services, not the process, so it doesn't). Unlike `config.toml`, it isn't
picked up while running: its variables go into the process environment, which
can't safely change once the daemon's threads are running.

```sh
# ~/.config/newcut/env
NEWCUT_TOKEN=a-long-random-string
PUSHOVER_APP_TOKEN=…
PUSHOVER_USER_KEY=…
```

### The feed

`/feed.atom` (Feed, in the menu) lists new releases you don't have, newest
found first; each artist has their own, `/artists/<mbid>/feed.atom` (Feed, on
their page). Give the address to your feed reader. Feeds don't need the token:
feed readers can't sign in, and feeds only say what came out.

### The API

`GET /api/v1/status`, `/api/v1/new` and `/api/v1/missing` answer JSON;
`POST /api/v1/run/<service>` runs a service now; `/api/v1/events` streams
what happens as server-sent events.

## Notifications

Every `schedule.notification` (`1d`), newcut tells you what became new since
it last told you: one release by name ("New release: Radiohead", "OK Computer
(Album) is out today."), more in general ("3 new releases"). What's new is
what the New list has (out within `schedule.new_window`, or coming), and with
`[notification] strategy = "all"`, the ones you already have too.

A download that needs you (it isn't an exact match, its files aren't
where slskd should have put them, its place in your library is taken, …) is
told right away, once: "A download needs you", with the album and why, and
the Downloads page as its link. The command gets those with `"kind":
"download"` (and a `download` object: `album`, `artist`,
`release_group_mbid`, `folder`, `username`, `reason`); release
notifications say `"kind": "releases"`.

Each channel keeps when it last heard from newcut. Its first run only starts
that clock, so turning notifications on doesn't send what's already new; one
that fails (Pushover down, your script exiting with an error) hears the same
next time.

```toml
[notification.pushover]
app_api_token = "$PUSHOVER_APP_TOKEN"   # from the env file, or the token itself
user_key = "$PUSHOVER_USER_KEY"

[notification.command]
bin = "~/.bin/newcut-notify"
```

The command runs with no arguments and gets the notification as JSON on
stdin; exiting with an error counts as not sent:

```json
{
  "kind": "releases",
  "title": "New release: Radiohead",
  "message": "OK Computer (Album) is out today.",
  "url": "https://musicbrainz.org/release-group/b1392450-e666-3926-a536-22c65f834433",
  "url_title": "See it on MusicBrainz",
  "count": 1,
  "releases": [
    {
      "title": "OK Computer", "artist": "Radiohead",
      "artist_mbid": "a74b1b7f-71a5-4011-9441-d0b5e4122711",
      "release_group_mbid": "b1392450-e666-3926-a536-22c65f834433",
      "kind": "Album", "date": "1997-05-21",
      "status": "missing", "upcoming": false,
      "link": "https://musicbrainz.org/release-group/b1392450-e666-3926-a536-22c65f834433",
      "cover": "https://coverartarchive.org/release-group/b1392450-e666-3926-a536-22c65f834433/front-250",
      "found_at": "2026-11-10T17:40:21Z"
    }
  ]
}
```

```sh
newcut notify          # now (the daemon does it, if it's running)
newcut notify --test   # a test notification through each channel
```

Pushover's keys are masked in the web UI's config editor, like
`server.token`.

## Keeping it running

`newcut run` starts seven services, each on its own: `scanner` (every
`schedule.scan`), `resolver` (after each scan), `discography`, `fresh`,
`notifier` (every `schedule.notification`, when a channel is set up),
`importer` (finished slskd downloads, when `slskd.downloads_dir` and
`output_dir` are set) and `server`. One that stops unexpectedly is started again (after
1 second, then 2, 4, … up to 5 minutes).

```sh
newcut daemon status
newcut daemon pause fresh
newcut daemon resume fresh
newcut daemon restart
```

`newcut scan` and `newcut sync` ask the running daemon to do it when
there is one. Only one daemon runs at a time. Ctrl-C or `kill` stops it
cleanly.

## Other commands

```sh
newcut missing --artist "Radiohead"     # or a MusicBrainz id; --json
newcut new
newcut artists
newcut sync --artist "Radiohead"        # check one artist now
newcut ignore <release-group-mbid>…
newcut unignore <release-group-mbid>…
```

## Seeing more (or less)

`-v` shows more of what newcut does (each request it makes), `-vv`
everything, `-q` only errors. `--log-format json` logs JSON lines. `NEWCUT_LOG` takes a
[filter](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html)
(`NEWCUT_LOG=newcut=debug`).

## Development

```sh
make fmt check test
```

## Where things are

| What | Where |
| --- | --- |
| Your config file | `$NEWCUT_CONFIG_DIR/config.toml`, else `~/.config/newcut/config.toml` |
| Your env file (secrets) | `$NEWCUT_CONFIG_DIR/env`, else `~/.config/newcut/env`; or `--env-file` |
| newcut's own data | `~/.local/share/newcut/` |

If you set `XDG_CONFIG_HOME` or `XDG_DATA_HOME`, newcut uses those folders
instead, on macOS too.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success (also when `newcut run` is stopped) |
| 1 | Something went wrong; the message says what |
| 2 | The command or the config is wrong (unknown option, unknown artist, a config mistake) |
