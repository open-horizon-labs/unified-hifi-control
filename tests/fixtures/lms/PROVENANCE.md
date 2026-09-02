# LMS fixture provenance

Every `.json` file in this directory is the **verbatim HTTP response body** from a
live Lyrion Music Server, pretty-printed and key-sorted, with nothing added or
removed. Each file's `params` member is LMS's own echo of the request that
produced it, so every fixture documents its own request.

These are recorded, not written. Issue #407 exists because
`LmsAdapter::search_library()` was tested against a hand-written guess at LMS's
response shape and that guess was wrong for three years — the loop items were
assumed to carry `id` when LMS emits `<type>_id`. **Do not hand-edit these files
and do not add hand-authored fixtures to this directory.** Re-record instead.

## Recording environment

| | |
|---|---|
| Server | Lyrion Music Server **9.1.2**, build `1781881406` (Sat Jun 20 04:01:40 UTC 2026) |
| Image | `lmscommunity/lyrionmusicserver:stable`, digest `sha256:e7865195d91df554760df3957bed91044f650b80c3fc2783d425c487b68c7185`, `arch=arm64` |
| Transport | `POST /jsonrpc.js`, `{"id":217,"method":"slim.request","params":[<playerid\|"">,[…]]}` — byte-for-byte the request `LmsRpc::execute` builds (`src/adapters/lms.rs`), including the `id: 217` this repo uses |
| Library | 12 tagged FLACs, 4 artists, 5 albums, 4 genres, generated with `ffmpeg`; artist/album/track names chosen so one `search term:` hits `contributors_loop`, `albums_loop` and `tracks_loop` at once |
| Network | container published **`127.0.0.1:9000` and `127.0.0.1:9090` only**. Port **3483 was never published** — see the warning below |
| Players | `02:00:00:00:00:01` "Kitchen" and `02:00:00:00:00:02` (`model: http`, LMS Web Clients via `/stream.mp3?player=<mac>`); `02:00:00:00:00:11` "Study" (`model: squeezelite`, `isplayer: 1`) via a minimal SlimProto `HELO` client run **inside the container's own network namespace** (`docker run --network container:…`) so 3483 never left the Docker bridge |

### Do not publish port 3483

Publishing 3483 lets every SlimProto player on the LAN auto-discover the throwaway
server and attach to it. This already happened once during the #402/#403 survey and
pulled roughly ten of the operator's real players onto a test container. Bind 9000
(and 9090 if you need the CLI) to `127.0.0.1`, and reach 3483 from inside the Docker
network if you need a player.

## What each fixture proves

| File | Proves |
|---|---|
| `search_term_ember.json` | **Defect 1.** `search <s> <n> term:` returns `albums_loop[].album_id`, `contributors_loop[].contributor_id`, `tracks_loop[].track_id`. There is **no `id` key anywhere** — which is what `search_library()` read. One term hits all three loops. |
| `search_term_aurora.json` | Same shape for a term that matches an artist name; `albums_loop` ordering is not stable across terms. |
| `search_term_jazz.json` | `genres_loop[].genre_id` / `genre`, and that loops with no matches are **omitted entirely** rather than returned empty. |
| `search_no_results.json` | Zero matches is `{"count": 0}` with no loops at all. |
| `players_mute_mixed.json` | **Defect 3.** `players 0 100 playerprefs:mute` carries per-player mute in the response the adapter *already* makes, so mute costs **zero extra round-trips**. Also captures all three shapes LMS uses in one response: `"mute": "0"` (string), key **absent** (pref never written), `"mute": "1"` (string). |
| `players_mute_all_unmuted.json` | Same query with nothing muted. |
| `players_no_playerprefs.json` | The same `players` query **without** `playerprefs:` — no `mute` key on any player. Proves the tag is what adds it, and that the old call could not have known mute. |
| `mixer_muting_query_muted.json` / `..._unmuted.json` | `mixer muting ?` → `{"_muting": 1}` / `{"_muting": 0}`. The per-player query, kept as the documented alternative to the batched one. |
| `status_squeezelite_muted.json` | `status` on a **muted** real squeezelite player carries **no mute key at all**, and reports `"mixer volume": -42` — LMS **negates** the volume once its mute fade completes. Both halves matter: `status` cannot report mute directly, and the raw value it does report goes negative, which the adapter used to hand straight to a `VolumeControl` declared `min: 0.0`. See the timing table below. |
| `collections_albums_artwork_only.json` | **The collection tag defect.** `albums 0 3 tags:cJ` — the artwork-only string #549 shipped. Rows carry `id`, `performance`, `favorites_url`, and a **null** `favorites_title`, and **no `album` key at all**, so `query_albums`' title guard dropped every row and the Albums level (plus every artist's album list) was empty against a real server, at any library size. |
| `collections_albums_with_display_tags.json` | The same query as `tags:lacJ`: `album` and `artist` are back alongside the artwork tags. Proves the fields are absent because they were not requested, not because the album lacks them. |
| `collections_titles_artwork_only.json` | `titles 0 3 album_id:2 tags:cJ` — `title` survives (it is not tag-gated) but `artist` is gone, so track rows lost their subtitle. |
| `collections_titles_with_display_tags.json` | The same query as `tags:acJ`; `artist` restored. |
| `collections_playlisttracks_artwork_only.json` | `playlists tracks 0 3 playlist_id:<id> tags:cJ` — same subtitle loss as `titles`, and shows the loop is named `playlisttracks_loop`. |
| `collections_playlisttracks_with_display_tags.json` | The same query as `tags:acJ`; `artist` restored. |
| `collections_favorites_without_want_url.json` | **The favorites defect.** `favorites items 0 20` — the query the adapter shipped. Rows carry `id`, `name`, `type`, `image`, `isaudio`, `hasitems` and **no `url`**. A favorite has no durable entity id, so `list_favorites`' url guard dropped every row and the Favorites and Radio tabs were empty. Also shows the artwork field is `image` (a server-relative icon path like `html/images/radio.png`), **not** the `icon` absolute URL the adapter reads — so favorites carry no artwork either way. |
| `collections_favorites_with_want_url.json` | The same query plus `want_url:1`: every non-folder row gains `url` (`http://…` for a stream, `db:track.titlesearch=…` for a library favorite). The folder row (`hasitems: 1`) still has none, which is why it is dropped rather than listed as a dead end. |
| `status_tags_aAdltKc.json` | **Defect 4.** The adapter's exact `tags:aAdltKc` string. `l` yields `"album": "Ember Light"` (album **title**, not `album_id`); `A` yields per-role `albumartist`/`trackartist`; `a` yields `artist`; `d` `duration`; `t` `tracknum`. Also shows `artwork_track_id` is **absent**, because that field is tag `J` which this string does not request. |

## Mute is immediate; the negated volume lags it (no fixture file — this is timing)

`mixer muting` schedules a volume fade and only writes the negated volume when the
fade finishes, so `status`'s `mixer volume` is a **lagging** mute signal while the
`mute` pref is an immediate one. Measured on the real squeezelite player at volume
42, polling `status`, `mixer muting ?` and `players … playerprefs:mute` together:

| Time after `mixer muting 1` | `status`'s `mixer volume` | `mixer muting ?` | `playerprefs:mute` |
|---|---|---|---|
| 0.04 s | `42` | `"1"` | `"1"` |
| 0.78 s | `-42` | `"1"` | `"1"` |
| 0.78 s – 8.6 s | `-42` | `"1"` | `"1"` |

| Time after `mixer muting 0` | `status`'s `mixer volume` | `mixer muting ?` | `playerprefs:mute` |
|---|---|---|---|
| 0.07 s | `-42` | `"0"` | `"0"` |
| 0.84 s onward | `42` | `"0"` | `"0"` |

Two consequences the adapter relies on:

1. The **pref** is correct in every window, so `playerprefs:mute` is the primary
   signal. A sign-based reading alone would report unmuted for ~0.8 s after a mute.
2. A negative `mixer volume` is nonetheless a *sufficient* condition for muted, so
   the adapter also treats it as one. It can only ever produce a false **negative**,
   never a false positive, which makes it safe to OR with the pref — and it needs no
   tagged parameter, so it still works if `playerprefs:` is unavailable.

An earlier reading of this recorded a positive `42` while muted and concluded LMS
does not negate the volume. That reading was taken inside the sub-second fade window
and was wrong; `tests/lms_adapter_defects.rs::muted_player_volume_is_not_negated`
caught it against `status_squeezelite_muted.json`.

## Failure mode (no fixture file — there is no response to record)

LMS never returns a JSON-RPC `error` object. On any request error it closes the
socket having written **zero bytes**, with no HTTP status line at all. Recorded by
hand-writing HTTP/1.1 to the socket and counting bytes:

| Request | LMS status | Bytes received |
|---|---|---|
| `search 0 3 term:Ember` | 0 (ok) | 675 (`HTTP/1.1 200 OK`, 496-byte body) |
| `totallybogus items 0 10` | 104 unknown in dispatch table | **0 — socket closed** |
| `status 0 1` on an unknown player id | 103 missing client | **0 — socket closed** |
| `playlist save X` with `playlistdir` unset | 105 bad config | **0 — socket closed** |
| `playlistcontrol cmd:load artist_id:999999` | Perl exception | **0 — socket closed** |
| `browselibrary items 0 20` (no `mode:`) | 102 bad params | 9 bytes then **hung** until client timeout |

Source: `Slim::Web::JSONRPC::requestMethod` calls `closeHTTPSocket()` on any
`$request->isStatusError()`. There is no branch that emits an `error` member.

Because there is nothing to record, `tests/lms_transport.rs` reproduces this
behaviour with a local TCP listener that accepts, reads the request, and closes
without writing — the exact wire behaviour in the table above.
