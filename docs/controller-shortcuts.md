# Controller shortcuts

A shortcut starts a Library item with one press on a controller that has no screen to browse with: a BLE
media remote paired to a Frame or Dial, or a button on the controller itself.

Two layers, kept apart on purpose:

- **A shortcut** is a pinned Library item: an album, playlist, station or favorite. You pin it once, in the
  Library (**⋯ → Pin as shortcut** on a row, or the pin button on a card).
- **A controller's map** says which button starts which shortcut. You edit it under
  **Controllers → Config → Buttons and shortcuts**, and it saves as you change it.

One pin can serve every controller, and a controller that is re-flashed loses nothing: maps live on the bridge,
not in the device's settings.

## What a press does

1. The controller reports the key and gesture (`POST /knob/input`, identified by `X-Knob-Id`).
2. The bridge looks up the controller's map, finds the shortcut and plays it on **the controller's own zone**:
   its assigned zone if it has one, otherwise the zone it last reported. The controller store normalizes
   legacy bare Roon IDs. Playback is typed intent admitted through the reliable command runtime; a missing,
   expired or stopped endpoint cannot issue a native write. Provider resolution and retries stay inside
   the adapter. Spotify and Music Assistant use coordinator-owned library command companions.
3. The reply carries a stable `code` so a controller with no screen can show something. A press that is not
   bound to anything replies `unmapped`, which is normal and not an error.

| `code` | Meaning |
| --- | --- |
| `accepted` | The provider took the command. This is not a claim that audio started: nothing observes that. |
| `unmapped` | That key and gesture have no shortcut on this controller. |
| `no_zone` | The controller has not chosen a zone yet. |
| `zone_missing` | The controller's zone is not currently discovered. |
| `provider_mismatch` | The shortcut is for a different provider than the controller's zone. |
| `item_unavailable` | The provider could no longer find the item. |
| `refused` | The provider or bridge declined; the `message` says why. |
| `no_shortcut` | The button points at a shortcut that was deleted. |

**It never plays on another zone instead.** If the controller's zone is gone, the press fails and says so:
music starting in a room nobody chose is worse than silence.

A zone hidden from lists (globally in Settings, or for this controller) **still plays**. Hiding declutters; it
does not deauthorise. The editor says so next to "Plays on".

## Which keys can carry a shortcut

| Key | Gestures |
| --- | --- |
| Volume up/down, mute | none. Volume always means volume. |
| Play/pause, next, previous | long press only. The key's normal action already ran on the press. |
| Any other key | press, long press, double press |

A key the bridge has no name for arrives as `usage_XXXX` (the HID usage in hex), so any key a remote sends can
be mapped. The editor lists keys a controller has recently reported, so you can press a button on the remote and
pick it instead of typing its name. It re-reads the controller every few seconds while open, so a press made on the
remote shows up, with its outcome, without reloading.

## What a shortcut stores

Not a ref token and not a browse session. The MCP ref table is deliberately short-lived, and a Roon `item_key`
only means something inside the session that minted it. A shortcut keeps the part that outlives both:

| Provider | Stored | At press time |
| --- | --- | --- |
| Roon | the semantic browse walk from the root, each row's title and subtitle | resolved uniquely in a fresh session inside the provider endpoint; every retry retains that full identity. List positions never identify a shortcut, and indistinguishable rows refuse rather than select a recording by position |
| LMS | database id, saved-playlist id or URL | validated against the live library before the queue is touched |
| Spotify, Music Assistant | the provider URI | played as is |

Items without a durable handle are refused at pin time, with the reason, instead of pinned and left to fail on a
button nobody can see:

- a Roon **search** hit (it knows only its own title, which is not an identity and names no root-level row): pin it from Browse;
- an LMS search result that is only a positional breadcrumb (it can silently play the wrong item);
- a folder, which does not play;
- Apple Music items, until the companion's content handles are verified to outlive a session.

Titles alone are never used to find a Roon item: two albums can share one, and a title match takes the first. Two
"Greatest Hits" by different artists are two pins, and a press opens the one that was pinned. If it has gone and only a
lookalike remains, the press reports `item_unavailable` rather than playing the lookalike. Renaming or moving an item
in Roon or LMS has the same effect. Reordering the list does not create a new pin when the same item is pinned again.

The pinned list shows the pin's latest aggregate result. Each button's history is separately scoped to its
controller, event, pin and target zone; another controller's failure cannot become that button's history.
“Accepted last time” reports native acceptance, not confirmed audio playback.

## API

| Method | Path | |
| --- | --- | --- |
| `GET` | `/api/shortcuts` | Every pin, with the last press result |
| `POST` | `/api/shortcuts` | `{ "ref": "<Library ref>", "name": "<optional>" }`; pinning the same item twice returns the existing pin |
| `PATCH` | `/api/shortcuts/{id}` | `{ "name": "…" }` (the API contract check now reads `PATCH` routes) |
| `DELETE` | `/api/shortcuts/{id}` | Also unmaps it from every controller |
| `GET` | `/api/controllers/{id}/shortcuts` | The map, where it plays, per-button status, recently heard keys |
| `PUT` | `/api/controllers/{id}/shortcuts` | `{ "bindings": { "<key>:<gesture>": "<shortcut id>" } }`; replaces the whole map, all or nothing |
| `POST` | `/api/controllers/{id}/shortcuts/test` | `{ "key", "gesture" }`; presses a button from the UI, through the same path |
| `POST` | `/knob/input` (also `/input`) | `{ "key", "gesture" }` from a controller |

Errors are `{ "error": "<sentence for a person>", "error_code": "<stable value>" }`.

The mutating routes, and `/knob/input`, are treated like the other playback controls when
`UHC_REQUIRE_CONTROLLER_AUTH` is on (see [controller-auth.md](controller-auth.md)).

Shortcuts and maps are stored in `shortcuts.json` next to `knobs.json`. An unreadable file is never overwritten:
the store refuses writes until it is restored.

## Per-controller zone hiding

Each controller can also keep zones out of its own zone list, on top of the global hide list in Settings
(**Controllers → Config → Zones on this controller**). It is a list filter only, like the global one: a zone hidden
for a controller still plays when addressed, so an assigned zone or a shortcut that targets it keeps working. The
list is omitted from the controller's synced config while empty, so controllers that never use it are not resynced
by an upgrade.

## Not covered yet

- Remotes that are not HID-over-GATT (for example Shelly BLU buttons, which broadcast BTHome advertisements).
- Action macros (volume, grouping, zone switching) as part of a press.
- A controller-side screen for shortcuts; maps are edited on the bridge.
