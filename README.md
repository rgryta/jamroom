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

Library placement: `<Artist>/<Album>/<Title>.m4a` when an album is known, otherwise
`Non-Album/<Artist>/<Title>.m4a`. A file that already exists in the library (same title, matching
artist, length within 3 s) is played from there and never downloaded again.

Downloads are about 128 kbps AAC, which is what YouTube serves.

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
| `JAM_LIBRARY` | `/music` | music library root (read and write) |
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
