# Daemon HTTP API

Server on `0.0.0.0:8888` (any device on the same Wi-Fi can control it).
HTTP/1.1, `Connection: close`, UTF-8 JSON. Errors: 4xx/5xx status with `{"error": "..."}`.

## Types

```json
// Item (track)
{ "uri": "spotify:track:...", "name": "...", "artist": "A, B", "dur": 213560 }
```

## Library

| Method and route | Response |
|---|---|
| `GET /api/playlists` | `{"items": [Node]}`. Node = `{"t":"p","id","name","sub"}` (playlist) or `{"t":"f","id","name","n","items":[Node]}` (folder) |
| `GET /api/playlist?id=<base62>` | `{"name", "tracks": [Item]}` |
| `GET /api/liked` | `{"name": "Liked Songs", "tracks": [Item]}` |
| `GET /api/album?id=<base62>` | `{"name", "tracks": [Item]}` |
| `GET /api/search?q=<text>` | `{"tracks": [Item]}` (up to 50; search returns tracks only) |

## Playback

| Method and route | Effect |
|---|---|
| `POST /api/play` | body `{"ctx": "liked" \| "playlist:<id>" \| "album:<id>" \| "search:<q>", "uri": "<first track, optional>", "shuffle": true \| false}` (omit `shuffle` to keep the current mode). Also accepts `{"items": [Item], "index": n}` |
| `POST /api/pause` | pause |
| `POST /api/resume` | resume |
| `POST /api/next` | next track (instant when preloaded) |
| `POST /api/prev` | back to the start if past 4 s, otherwise previous track |
| `POST /api/seek?ms=<n>` | position |
| `POST /api/jump?i=<n>` | play queue item n |
| `POST /api/shuffle?on=0\|1` | shuffle the queue keeping the current track first; off restores the original order. Without `on` it toggles |
| `POST /api/repeat?on=0\|1` | at the end of the queue wrap to the first track instead of stopping. Without `on` it toggles |
| `GET /api/queue` | `{"idx", "tracks": [Item]}` |
| `GET /api/status` | `{"playing","loading","idx","count","item","pos_ms","error","ctx","shuffle","repeat","connected","underruns","buffered_ms"}` |

Every playback route answers with the same JSON as `/api/status`.

## Example

```bash
curl -s http://<ipad-ip>:8888/api/playlists
curl -s -X POST http://<ipad-ip>:8888/api/play \
  -H 'Content-Type: application/json' -d '{"ctx":"liked","shuffle":true}'
curl -s -X POST http://<ipad-ip>:8888/api/next
```
