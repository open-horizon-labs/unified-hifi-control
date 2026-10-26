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
   its assigned zone if it has one, otherwise the zone it last reported.
3. The reply carries a stable `code` so a controller with no screen can show something. A press that is not
   bound to anything replies `unmapped`, which is normal and not an error.

| `code` | Meaning |
| --- | --- |
| `played` | The provider accepted it. |
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
pick it instead of typing its name.

## What a shortcut stores

Not a ref token and not a browse session. The MCP ref table is deliberately short-lived, and a Roon `item_key`
only means something inside the session that minted it. A shortcut keeps the part that outlives both:

| Provider | Stored | At press time |
| --- | --- | --- |
| Roon | the title trail from the browse root | re-walked in a fresh session (`resolve_trail`, #616) |
| LMS | database id, saved-playlist id or URL | validated against the live library before the queue is touched |
| Spotify, Music Assistant | the provider URI | played as is |

Items without a durable handle are refused at pin time, with the reason, instead of pinned and left to fail on a
button nobody can see:

- a Roon **search** hit (its trail is only its own title, so it can never be found again): pin it from Browse;
- an LMS search result that is only a positional breadcrumb (it can silently play the wrong item);
- a folder, which does not play;
- Apple Music items, until the companion's content handles are verified to outlive a session.

Renaming a pin in Roon or LMS, or moving it, makes the next press report `item_unavailable`. The Controllers page
shows that on the shortcut and on each button that uses it.

## API

| Method | Path | |
| --- | --- | --- |
| `GET` | `/api/shortcuts` | Every pin, with the last press result |
| `POST` | `/api/shortcuts` | `{ "ref": "<Library ref>", "name": "<optional>" }`; pinning the same item twice returns the existing pin |
| `PATCH` | `/api/shortcuts/{id}` | `{ "name": "…" }` |
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
