# Optional music details through HiPhi Cloud

The music-details client uses the installation key and owner-only pairing configuration already created by HiPhi pairing. It sends a dedicated signed metadata read to the production HiPhi authority. No provider token, owner cookie or Garmin grant is shared, and no subscription check is required. The cloud capability is independently deployable; old UHC builds continue listening normally when it is absent or disabled.

Build this optional CLI with `cargo build --bin uhc-music-details --no-default-features --features server`. The existing release installer does not yet bundle it. For an explicit playing zone on this UHC host (default local HTTP port 8088):

```sh
uhc-music-details --zone 'roon:EXPLICIT_ZONE_ID' en
```

The command reads existing local now-playing state before the request, checks it again afterward, and refuses to print a result when the music identity changed. The before/after comparison cannot detect A→B→A or distinct queue entries with identical metadata; the returned context is still about that same music identity. A live UI must use its own item/zone/session epoch token with `read`, rather than treating this convenience check as a queue revision. Seek and volume values are excluded from identity. It does not start playback, change zones or alter the existing local HTTP API. For a direct canonical identity lookup:

```sh
printf '%s\n' '{"artist_mbid":"7944ed53-2a58-4035-9b93-140a71e41c34"}' | uhc-music-details en
```

Rust callers can use `cloud_connector::music::MusicDetailsClient::from_runtime(config_dir)` and `read_now_playing(&aggregator, zone_id, language)`. The helper reads the aggregator before and after enrichment. Surfaces should request details on demand, preserve all source attribution and render source text as text. They must honor partial/ambiguous/unavailable status, and must never turn a source outage into a playback failure. Direct `read` callers own the opaque item token and must invalidate it on item, zone or session changes; the returned `applies_to` method is a final race check, not an automatic UI binding.

This change provides the client and CLI. It does not add a details UI or a local HTTP endpoint. Those require the repository's separate public API review. Custom relay hosts/ports do not implicitly become metadata recipients; this client is restricted to the production `relay.hiphi.audio` pairing endpoint.

## Cloud contract version 1

The signed JSON body contains exactly `purpose`, `version`, `installation_id`, `request_id`, `issued_at`, `endpoint`, `item_token`, `identity`, `language`, and `signature`. The purpose is `hiphi.music-details.v1`; the endpoint is `https://relay.hiphi.audio/v1/music/details`. Ed25519 signs canonical JSON with `signature` omitted. Request IDs are new UUIDs, timestamps are Unix milliseconds within 60 seconds, and the opaque token is at most 128 ASCII letters, digits, underscores or hyphens. No zone identifier is sent to the shared source cache.

Identity may contain `artist`, `title`, `album`, `duration_ms`, `artist_mbid`, `recording_mbid`, `release_mbid`, and `wikidata_id`; text fields are bounded to 512 UTF-8 bytes and the whole request to 8 KiB. Metadata names are candidate evidence, not proof of a recording. A supplied Wikidata ID alone is explicitly unverified as a music match. Language is an explicit supported wiki language code.

The response is bounded to 512 KiB and echoes `version: 1` and `item_token`, with `status` (`complete`, `partial`, `ambiguous`, or `unavailable`), `catalog`, `sources`, `entities`, `genres`, `unavailable`, `language` and `stale`. Current enrichment returns partial whenever content is available because source coverage is not assumed complete. Per-source facts include provenance, source revision, attribution and license; retained raw source responses are not sent to clients. Absence, outage, ambiguity and stale data must remain distinguishable in a consuming surface.

Unknown/removed installations and invalid/expired signatures return 401; replay and rate limits return 429; disabled/unavailable service returns 503. Treat every such outcome as optional context unavailable, never as a playback or pairing failure.
