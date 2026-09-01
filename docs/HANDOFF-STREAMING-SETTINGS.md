# Handoff: Streaming Adapters and the Settings Recovery

## Mission

Finish the Apple Music/Spotify streaming-adapter initiative without asking the
user to test speculative builds. The immediate priority is to make UHC Settings
reliable again: every feature switch must persist, reload from server-confirmed
state, and remove disabled provider surfaces and tabs.

## Current branch and useful commits

Work is on `feat/issue-462-streaming-adapters`.

- `1be7dca` — adds `make web-run`; use it instead of `cargo run` for web work.
- `b973578`, `1970a73` — deterministic contracts for the fullstack runner and
  provider-card visibility.
- `8fb8d09` — server/client navigation visibility bootstrap. Optional tabs now
  use one persisted settings snapshot rather than client defaults.

Do **not** reset or discard unrelated untracked companion/Xcode files.

## The current Settings failure

The user reported that feature switches do not toggle at:

`http://192.168.1.176:18088/settings`

The reproducible browser error is a Dioxus hydration failure at startup:

```text
hydrate_node … Cannot read properties of undefined (reading 'toString')
```

When that occurs, Dioxus event handlers never attach. Do not interpret a
server-side `/api/settings` implementation or a source-only test as proof that
the user can toggle a switch.

The attempted uncommitted Settings rewrite currently includes:

- feature values initialized from the SSR marker rather than hard-coded WASM
  defaults;
- server-confirmed `POST /api/settings` plus reload semantics;
- accessible switch-button styling;
- a `public/settings-feature-toggles.js` fallback controller.

It is **not accepted and must not be committed as a fix** until live acceptance
passes. It may be salvaged, replaced, or reverted deliberately.

## Required debugging path

1. Start from a known running instance. Do not leave `:18088` down while
   experimenting. The currently restored listener is the legacy binary.
2. Build the actual fullstack artifact with:

   ```sh
   UHC_PORT=18088 make web-run
   ```

   This is mandatory. `cargo run` produces a server that can disagree with the
   WASM bundle.
3. Open a brand-new browser tab to `/settings` and capture console errors
   before clicking anything.
4. If hydration still fails, minimize the SSR/client tree mismatch. Fix the
   mismatch rather than adding more reactive `hidden`, conditional siblings, or
   controlled checkbox patches.
5. Only after zero hydration errors, test the UI behavior below.

If the fullstack release process is killed locally, use the debug fullstack
artifact only for diagnosis, determine why it exits, and restore the known
working listener first. Do not tell the user a build is live unless `lsof` and
an HTTP request prove it is listening.

## Acceptance test: feature switches

Run this against a freshly built, listening fullstack server:

1. Load `/settings`; browser console has no hydration error.
2. Disable Apple Music.
   - `POST /api/settings` contains `adapters.applemusic: false`.
   - After reload, the Apple Music provider card and Streaming providers area
     are absent when Spotify is also disabled.
   - The Apple Music switch renders off.
3. Enable Apple Music again; the card appears only after confirmed reload.
4. Repeat one non-provider switch, such as UPnP/DLNA, and confirm its server
   setting changes and survives a refresh.
5. Force a server error if practical; the visible switch must return to the
   persisted setting and show a plain-language error.

## Acceptance test: navigation visibility

With HQPlayer, LMS, and Spotify disabled, a fresh request to every page must
render those nav links with `hidden` before JavaScript runs. They must remain
hidden after hydration.

The deterministic guard is:

```sh
cargo test --test navigation_visibility_contract
```

## Existing deterministic guards

Run all of these before commit:

```sh
cargo test --test web_fullstack_runner_contract \
  --test settings_provider_visibility_contract \
  --test navigation_visibility_contract
```

`tests/settings_feature_transaction_contract.rs` is currently uncommitted with
the experimental Settings rewrite. Retain or rewrite it so it verifies the
final design, not a transient implementation detail.

## Product constraints

- UHC is the bus and aggregator. Provider/companion access goes through it.
- Spotify is controller-only; UHC is not a Spotify Connect receiver.
- Apple Music pairing is native companion ownership (iPhone/iPad/Mac) with
  Bluetooth-style matching-code confirmation—no developer App-ID/MusicKit
  setup shown to end users.
- Settings should show a provider area only when Spotify or Apple Music is
  enabled. Music Assistant remains suppressed from the feature list for now.
- Disabled feature means disabled: no stale provider card, tab, polling loop,
  or page visibility leak.

## Shipping gate

Commit only focused, tested changes. Push normally; never force-push. Do not
merge. Before telling the user it is fixed, report the exact running artifact,
URL, browser console result, and both toggle directions exercised.
