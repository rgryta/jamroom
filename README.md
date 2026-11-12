# jamroom

A shared play queue for an office or a room. Anyone with a room code pastes a link or types a
song name; one browser tab plays the queue through whatever speakers the computer is connected
to. Songs that are not in the music library yet are downloaded, tagged and added to it.

A single Rust binary (axum + SQLite) plus `yt-dlp` and `ffmpeg`. It idles at a few MB of memory.

## How it works

```
phones ──► /r/<code>        add page: paste a link or a song name, see the queue
laptop ──► /r/<code>/player  plays the queue (open once, press Start)
                │
                ▼
  resolve ─► already in the library? ─► yes: stream that file (keeps its quality)
                │ no
                ▼
  yt-dlp download ─► tag (artist, title, album, cover) ─► Artist/Album/Title.ext in the library
```

| Pasted | What happens |
|---|---|
| YouTube / YouTube Music / SoundCloud link or playlist | yt-dlp reads the metadata and downloads the audio |
| Spotify track / album / playlist link | The public embed page gives title, artists and length (no account or API key). The same recording is matched on YouTube Music by title, artist and length (within 2.5 s) and downloaded from there. Nothing is taken from Spotify's own streams |
| Tidal link | The page title is used as a YouTube Music search |
| Plain text | YouTube Music song search, first plausible hit |

Downloads go to a private cache (`/data/songs`, `<Artist>/<Album>/<Title>.m4a` or
`Non-Album/<Artist>/<Title>.m4a`), not into the music library: YouTube audio is 128 kbps AAC, and the
library is for better sources. A song that already exists in the library (same title and matching
artist; same length within 3 s, or within 30 s unless a version word like live or remix differs)
is played from there and never downloaded again. Your library comes first, then the cache. The
library index is refreshed every five minutes. Cached songs unused for `JAM_CACHE_DAYS` (60) days are deleted.

## Soulseek upgrade

`jamroom upgrade` is a second process (its own container in `deploy/compose.yaml`). For every cached
song it searches Soulseek through slskd, accepts only a close match (FLAC, or MP3 at 256 kbps or
more; title and artist in the file name; length within 3 s; no remixes or live versions), waits for
the download, tags it and files it in the library, then points waiting queue entries at it. Nothing
else ever writes to the library, and the public service has no route to slskd. Songs with no good
copy stay in the cache and are tried again after 14 days.

Downloads are about 128 kbps AAC, which is what YouTube serves.

## Autoplay

Each room has an **Autoplay** switch, on the add page and on the player (off to start). When the
queue is empty, it queues a song from the music library, so the room keeps playing like a radio.
Library songs start at once at full quality and nothing is downloaded. Someone's own songs always
go first. None of the room's last 60 plays comes round again, and the same artist is not picked
within three songs. With nothing else switched on, the pick is random.

**Keep the mood** (shown while Autoplay is on) makes the pick follow what the last five songs were
like. Choose what to follow, any number of them:

* **Genre**, **Era** (the same decade, within about five years), **Language**, and **Artist** (more
  of the same artists), from the file tags. A YouTube song borrows the tags of the same artist's
  library songs.
* **Tempo**: the same pace. Library files rarely have a BPM tag, so the Soulseek upgrader's
  container measures each song in the background (30 seconds of audio through ffmpeg, then beat
  detection), slowly and one song at a time. The panel shows how many songs are done; songs not
  measured yet are skipped by this option.

**How close** is Loose (a third of the picks ignore the mood), Balanced, or Tight (stay near the
best few fits). A line under the options says what the radio is following right now, for example
"Rock · 1990s · ~118 BPM".

## Playing in sync

The server keeps one playback clock per room (the playing song's start time, or the spot where it
was paused). Every player page follows that clock: it joins a song at the right second, rejoins
after a stall or a browser pause, and pause, resume and seek from any page or media key apply to
all players. Restarting the server does not stop the song. The pages share `web/jam.css` and
`web/jam.js`, served at `/assets/`, and follow the system light or dark setting.

## Run

```sh
docker build -t jamroom .
docker run -p 8080:8080 -v ./data:/data -v ~/Music:/music \
  -e JAM_ADMIN_TOKEN=change-me -e JAM_DEFAULT_ROOM=Office jamroom
```

The first start creates a room named by `JAM_DEFAULT_ROOM` and logs its code. More rooms come
from `/admin` (needs `JAM_ADMIN_TOKEN`). `deploy/compose.yaml` is the homelab stack.

## Settings

| Variable | Default | Meaning |
|---|---|---|
| `JAM_ADDR` | `0.0.0.0:8080` | listen address |
| `JAM_DATA` | `/data` | database, yt-dlp copy, scratch space |
| `JAM_LIBRARY` | `/music` | music library root (the public service only reads it) |
| `JAM_CACHE` | `<data>/songs` | where downloaded songs are kept |
| `JAM_CACHE_DAYS` | `60` | cached songs older than this are deleted unless queued |
| `JAM_SLSKD_URL` | empty | slskd address; only the upgrader uses it |
| `JAM_SLSK_DOWNLOADS` | `/slsk` | slskd's finished-downloads folder, as the upgrader sees it |
| `JAM_ADMIN_TOKEN` | empty | enables the admin API when set |
| `JAM_PYTHON` | `python3` | interpreter that runs the yt-dlp script (the data volume need not allow execution) |
| `JAM_MAX_SECONDS` | `900` | longest track that is downloaded |
| `JAM_CONCURRENCY` | `2` | parallel downloads |
| `JAM_MAX_QUEUE` | `50` | tracks waiting per room |
| `JAM_MAX_PLAYLIST` | `50` | tracks taken from one playlist or album link |
| `JAM_MAX_NEW_PER_DAY` | `150` | new files written to the library per day |
| `JAM_ADDS_PER_MINUTE` | `20` | adds per client address per minute |

## Safety

The service downloads on behalf of anyone who has a room code, so:

- only YouTube, SoundCloud, Spotify and Tidal hosts are accepted (checked on the parsed host, so
  `youtube.com.evil.example` and local addresses are refused);
- yt-dlp is started without a shell and the URL always follows `--`;
- room codes are 10 random characters, wrong guesses are rate limited per client, and a room can be
  revoked in `/admin`;
- per-room queue limit, per-client add limit, track length and file size limits, and a daily cap
  on new library files;
- the container runs as an unprivileged user with a read-only root and no capabilities.

Cached tracks are copied from services whose terms do not allow downloading. Use this for your own
library and your own office.

## Development

```sh
cargo test          # 34 tests, no network needed
cargo clippy --all-targets -- -D warnings
```

Fixtures in `tests/fixtures` are trimmed copies of real Spotify embed pages and a YouTube Music
search response, so the parsers are tested against what the services really send. If a parser
breaks, refresh the fixture from the live response first.
