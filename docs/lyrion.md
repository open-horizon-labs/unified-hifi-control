# Lyrion (LMS) Integration Notes

Implementation learnings for the Lyrion Music Server adapter.

## API Reference

- [CLI Documentation](https://lyrion.org/reference/cli/introduction/)
- [Players Command](https://lyrion.org/reference/cli/players/)
- Local docs: `http://HOST:9000/html/docs/cli-api.html`

## Key Fields

### `connected` vs `isplaying`

The `players` query returns both fields with different meanings:

| Field | Meaning | Values |
|-------|---------|--------|
| `connected` | TCP connection to player | 0/1 |
| `isplaying` | Playback state | 0/1 |

**Important:** Mobile apps (iPeng, Squeezer) often show `connected: 0` when:
- App is backgrounded (iOS aggressively suspends)
- Device is sleeping
- Network hiccup

Do NOT filter players by `connected` status - users expect to see paused/idle players.

### `artwork_url` for Streaming Services

For streaming content (Qobuz, Tidal, etc.), LMS returns `artwork_url` as a **relative path**:

```
/imageproxy/https%3A%2F%2Fstatic.qobuz.com%2F.../image.jpg
```

Must prepend `baseUrl` to make it absolute:

```javascript
if (artworkUrl && artworkUrl.startsWith('/')) {
  artworkUrl = `${this.baseUrl}${artworkUrl}`;
}
```

The `coverid` field often returns placeholder icons for streaming content - prefer `artwork_url`.

## Polling Behavior

LMS uses polling (no WebSocket push). Default interval: 2 seconds.

**Zone change notifications:** Only notify bus when player set changes (added/removed), not on every poll. Otherwise zones will flicker in the UI.

```javascript
const setChanged = previousIds.size !== currentIds.size ||
  [...previousIds].some(id => !currentIds.has(id));

if (setChanged && this.onZonesChanged) {
  this.onZonesChanged();
}
```

## JSON-RPC Format

Endpoint: `POST http://HOST:9000/jsonrpc.js`

```json
{
  "id": 1,
  "method": "slim.request",
  "params": ["PLAYER_ID", ["command", "arg1", "arg2"]]
}
```

Use empty string `""` for server-level commands (like `players`).

## Status Tags

Request specific fields with the `tags` parameter:

```javascript
['status', '-', 1, 'tags:aAdltKc']
```

This adapter's string requests: `a` artist, `A` per-role contributors, `d` duration,
`l` album title, `t` tracknum, `K` artwork_url, `c` coverid.

**An earlier version of this section was wrong**: it described `l` as the album id and
`A` as the album. Both are false — `l` is the album title and `e` is the album id —
and anyone extending the string from that legend would request the wrong fields. The
table below is read from LMS's own `%tagMap` in
`Slim/Control/Queries.pm` at 9.1.2 and confirmed against a live server; the recorded
response is `tests/fixtures/lms/status_tags_aAdltKc.json`.

**A `tags:` parameter replaces the query's default tag set, it does not extend it.**
Every field a caller reads has to be named in the string, including ones the same
query returns by default when no `tags:` is sent at all. This bit
`hifi_collections`: `albums` sends `album` by default, and adding `tags:cJ` for
artwork alone removed it, so every album row arrived with no title and the whole
Albums level came back empty. Recorded both ways in
`tests/fixtures/lms/collections_albums_artwork_only.json` and
`collections_albums_with_display_tags.json`; the adapter's collection tag strings
are the `ALBUM_DISPLAY_TAGS` / `TRACK_DISPLAY_TAGS` constants in
`src/adapters/lms.rs`, and `tests/lms_collection_tags.rs` fails if either drops a
field it reads.

| Tag | Key in the response | Value |
|-----|--------------------|-------|
| `a` | `artist` | Track artist name |
| `A` | `albumartist`, `trackartist`, `composer`, … | Per-**role** contributor names — one key per role present, not a single field |
| `s` | `artist_id` | Artist (contributor) id |
| `l` | `album` | Album **title** |
| `e` | `album_id` | Album id |
| `g` | `genre` | Genre name |
| `p` | `genre_id` | Genre id |
| `d` | `duration` | Seconds |
| `t` | `tracknum` | Track number |
| `y` | `year` | Year |
| `u` | `url` | Track URL |
| `c` | `coverid` | Cover id, for `/music/<coverid>/cover.jpg` |
| `K` | `artwork_url` | Artwork URL, relative for streaming content (see above) |
| `J` | `artwork_track_id` | The album's artwork track id |

Two things to know before extending the string:

- **`A` returns several keys, not one.** Which keys depends on the roles present on
  the track, so treat it as a set.
- **`artwork_track_id` requires `J`, which this adapter does not request.** The
  artwork fallback chain in `get_player_status` reads `coverid` → `artwork_track_id`
  → `id`, so its middle arm is currently unreachable and artwork falls through to
  the track's own `id`. Adding `J` costs nothing on the wire but changes
  `image_key` for tracks with no `coverid`, and `image_key` is client-visible — so
  it is deliberately left alone rather than changed as a side effect. See #407.

### `mixer volume` is negative while muted

`status` has **no mute key**. Mute lives in the per-player `mute` pref, and LMS
*negates* the volume pref while muted, so `status` reports e.g. `"mixer volume": -42`.
The negation lags the mute by roughly 0.8 s, because `mixer muting` schedules a fade
and only writes the negated value when it finishes.

The adapter therefore reports `player.volume` as the magnitude, and takes mute from
two signals OR-ed together:

1. `players 0 100 playerprefs:mute` — the pref, correct immediately. Rides on the
   `players` call the poll already makes, so mute costs **no extra round-trip**;
   `mixer muting ?` would cost one per player per poll.
2. A negative `mixer volume` from `status` — needs no tagged parameter, but lags,
   so it can only ever produce a false negative.

`mixer muting <0|1>` sets mute, and `mixer volume <n>` clears it as a side effect.

## Authentication

LMS supports HTTP Basic Auth. Include credentials in requests:

```javascript
const auth = Buffer.from(`${username}:${password}`).toString('base64');
headers['Authorization'] = `Basic ${auth}`;
```
