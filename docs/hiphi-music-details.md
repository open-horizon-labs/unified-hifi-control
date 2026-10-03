# Optional music details through HiPhi Cloud

The music-details client uses the installation key and owner-only pairing configuration already created by HiPhi pairing. It sends a dedicated signed metadata read to the production HiPhi authority. No provider token, owner cookie or Garmin grant is shared, and no subscription check is required. The cloud capability is independently deployable; old UHC builds continue listening normally when it is absent or disabled.

Build this optional CLI with `cargo build --bin uhc-music-details --no-default-features --features server`. Release builds publish standalone `uhc-music-details-*` binaries for Linux, macOS, and Windows; Linux deb/rpm packages install it as `/usr/bin/uhc-music-details`. Other package formats do not yet bundle the CLI. For an explicit playing zone on this UHC host (default local HTTP port 8088):

```sh
uhc-music-details --zone 'roon:EXPLICIT_ZONE_ID' en
```

The command reads existing local now-playing state before the request, checks it again afterward, and refuses to print a result when the music identity changed. The before/after comparison cannot detect A→B→A or distinct queue entries with identical metadata; the returned context is still about that same music identity. A live UI must use its own item/zone/session epoch token with `read`, rather than treating this convenience check as a queue revision. Seek and volume values are excluded from identity. It does not start playback, change zones or alter the existing local HTTP API. For a direct canonical identity lookup:

```sh
printf '%s\n' '{"artist_mbid":"7944ed53-2a58-4035-9b93-140a71e41c34"}' | uhc-music-details en
```

Rust callers can use `cloud_connector::music::MusicDetailsClient::from_runtime(config_dir)` and `read_now_playing(&aggregator, zone_id, language)`. The helper reads the aggregator before and after enrichment. Surfaces should request details on demand, preserve all source attribution and render source text as text. They must honor partial/ambiguous/unavailable status, and must never turn a source outage into a playback failure. Direct `read` callers own the opaque item token and must invalidate it on item, zone or session changes; the returned `applies_to` method is a final race check, not an automatic UI binding.

The client, CLI, local HTTP endpoint, and MCP tool provide explicit on-demand reads. Custom relay hosts/ports do not implicitly become metadata recipients; this client is restricted to the production `relay.hiphi.audio` pairing endpoint.

## Cloud contract version 1

The signed JSON body contains exactly `purpose`, `version`, `installation_id`, `request_id`, `issued_at`, `endpoint`, `item_token`, `identity`, `language`, and `signature`. The purpose is `hiphi.music-details.v1`; the endpoint is `https://relay.hiphi.audio/v1/music/details`. Ed25519 signs canonical JSON with `signature` omitted. Request IDs are new UUIDs, timestamps are Unix milliseconds within 60 seconds, and the opaque token is at most 128 ASCII letters, digits, underscores or hyphens. No zone identifier is sent to the shared source cache.

Identity may contain `artist`, `title`, `album`, `duration_ms`, `artist_mbid`, `recording_mbid`, `release_mbid`, and `wikidata_id`; text fields are bounded to 512 UTF-8 bytes and the whole request to 8 KiB. Metadata names are candidate evidence, not proof of a recording. A supplied Wikidata ID alone is explicitly unverified as a music match. Language is an explicit supported wiki language code.

The response is bounded to 512 KiB and echoes `version: 1` and `item_token`, with `status` (`complete`, `partial`, `ambiguous`, or `unavailable`), `catalog`, `sources`, `entities`, `genres`, `unavailable`, `language` and `stale`. Cloud reports complete, partial, ambiguous, or unavailable results based on its current source responses; source coverage is not assumed complete. Per-source facts include provenance, source revision, attribution and license; retained raw source responses are not sent to clients. Absence, outage, ambiguity and stale data must remain distinguishable in a consuming surface.

Unknown/removed installations and invalid/expired signatures return 401; replay and rate limits return 429; disabled/unavailable service returns 503. Treat every such outcome as optional context unavailable, never as a playback or pairing failure.

## Local HTTP and MCP reads

The same on-demand music-context read is available through UHC's local HTTP API and MCP. Both require an explicit prefixed zone ID and a language, and use the existing HiPhi Cloud pairing. No enrichment is triggered by ordinary now-playing reads, playback commands, seeks or volume changes.

```sh
curl 'http://localhost:8088/zones/roon:EXPLICIT_ZONE_ID/music-details?language=en'
```

When controller authentication is enabled, use the same authenticated controller session as other protected UHC reads. The LAN compatibility policy is unchanged. Responses use `Cache-Control: private, no-store`. Missing language, unknown query parameters and malformed inputs are rejected; there is no default-zone fallback.

MCP clients call `hifi_music_details` with:

```json
{"zone_id":"roon:EXPLICIT_ZONE_ID","language":"en"}
```

HTTP returns `version`, `zone_id`, `identity`, `language` and `details`; MCP returns that same payload inside the existing structured result envelope. The identity says which music the context describes, not which queue occurrence is current. Both surfaces compare identity before and after the Cloud request and refuse changed music. An A→B→A transition or two entries with identical metadata cannot be distinguished; consumers must compare the returned identity with their current selection and live UIs must also own their occurrence token.

`details` retains the Cloud response's status, sources, attribution/license information, catalog, entities, genres, unavailable sections, language and stale flag. A catalog answer marked partial, ambiguous or unavailable is a successful read, distinct from failing to reach the service. Consumers must preserve these distinctions and render source text as text rather than executable markup.

HTTP errors use `{ "error": "safe explanation", "code": "MACHINE_CODE" }`: `INVALID_REQUEST` (400), `ZONE_NOT_FOUND` (404), `NO_MUSIC` or `MUSIC_CHANGED` (409), and `CLOUD_NOT_PAIRED` or `MUSIC_DETAILS_UNAVAILABLE` (503). Existing controller-auth errors are unchanged. MCP carries the same music-context code in a structured refusal. Cloud failures never affect local playback. Requests have bounded concurrency, response size and deadlines; overload is reported as unavailable.

## Agent guidance and response time

The read-through implementation fetches source context on demand. The first request for a track can take longer while upstream sources respond. Agents should call this tool only when music context is useful, then treat `details.status` (`complete`, `partial`, `ambiguous`, or `unavailable`), `stale`, and each source's provenance/attribution as part of the answer. Do not combine facts from competing catalog candidates or present an ambiguous candidate as a confirmed match. Compare the returned `identity` with the user's current selection before describing it. Cloud or source delays affect this optional read only; playback remains local.
