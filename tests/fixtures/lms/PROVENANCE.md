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
| `status_squeezelite_muted.json` | `status` on a **muted** real squeezelite player still reports a plain positive `"mixer volume": 42` and carries **no mute key at all**. This is why `status` alone cannot report mute, and why the volume value is not secretly negated. |
| `status_tags_aAdltKc.json` | **Defect 4.** The adapter's exact `tags:aAdltKc` string. `l` yields `"album": "Ember Light"` (album **title**, not `album_id`); `A` yields per-role `albumartist`/`trackartist`; `a` yields `artist`; `d` `duration`; `t` `tracknum`. Also shows `artwork_track_id` is **absent**, because that field is tag `J` which this string does not request. |

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
