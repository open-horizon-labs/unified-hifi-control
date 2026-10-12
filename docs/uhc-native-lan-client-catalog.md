# UHC native-LAN hardware-control client catalog

Research snapshot: 2026-10-12.

Practical twelve-family implementation shortlist, not a measured market-share ranking; Denon/Marantz included as the known baseline.

**Native LAN only.** No RS232, USB, IR, serial bridges or cloud-provider implementation.

**Evidence boundary:** These are prospective UHC capabilities supported by upstream code/documentation. No devices were tested, and no UHC code or repository was changed. Exact model/firmware/source behavior still needs validation.

## Client inventory

| Family | Primary client | Language / license | Discovery | Control / feedback |
|---|---|---|---|---|
| Denon / Marantz AVR | [denonavr](https://github.com/ol-iver/denonavr) | Python / MIT | SSDP (Denon, DENON, DENON PROFESSIONAL, Marantz) | Telnet-like ASCII over TCP :23; HTTP/XML. TCP push plus HTTP state/configuration queries |
| WiiM | [wiim](https://github.com/Linkplay2020/wiim) | Python / MIT | mDNS: `_linkplay._tcp.local.` | UPnP SOAP and event subscriptions; LinkPlay HTTP API. UPnP push notifications with HTTP polling fallback |
| Yamaha MusicCast / Extended Control | [aiomusiccast](https://github.com/vigonotion/aiomusiccast) | Python / MIT | SSDP (Yamaha Corporation) | Yamaha Extended Control HTTP/JSON; UDP event notifications. UDP change notifications plus HTTP state reads |
| BluOS: Bluesound and BluOS-equipped products | [pyblu](https://github.com/LouisChrist/pyblu) | Python / MIT | mDNS: `_musc._tcp.local.` | BluOS HTTP API / XML :11000. HTTP polling in the inspected HA integration; do not claim push based only on BluOS branding |
| Cambridge Audio StreamMagic | [aiostreammagic](https://github.com/noahhusby/aiostreammagic) | Python / MIT | mDNS: `_stream-magic._tcp.local.`, `_smoip._tcp.local.` | StreamMagic WebSocket / JSON. WebSocket subscriptions and pushed state |
| Hegel legacy native-IP amplifiers | [hegel-ip-client](https://github.com/boazca/hegel-ip-client) | Python / MIT | SSDP (Hegel) | Hegel ASCII control over TCP :50001. Push state, command queue and reconnect handling |
| Lyngdorf / Steinway Lyngdorf network products | [lyngdorf](https://github.com/fishloa/lyngdorf) | Python / MIT | SSDP (Lyngdorf, Steinway Lyngdorf) | Native ASCII control over TCP :84; Streaming-module HTTP :8080; UPnP description / identity. Native TCP push; supplemental streaming-module reads |
| Onkyo / Integra / eISCP-capable Pioneer | [aioonkyo](https://github.com/arturpragacz/aioonkyo) | Python / GPL-3.0-only | SSDP (ONKYO, Onkyo & Pioneer Corporation, Pioneer); eISCP UDP 60128 | eISCP framed commands over TCP :60128. TCP push with receiver interrogation |
| Sonos | [soco](https://github.com/SoCo/SoCo) | Python / MIT | SSDP (ZonePlayer); mDNS: `_sonos._tcp.local.` | UPnP / SOAP over HTTP :1400; Local WebSocket API :1443. UPnP subscription events; modern HA additionally uses a WebSocket client |
| Anthem native-IP MRX / AVM family | [anthemav](https://github.com/nugget/python-anthemav) | Python / MIT | Configured IP; auto-discovery unverified | Anthem ASCII over TCP :14999. TCP status updates plus state queries |
| Arcam and compatible native-IP JBL / AudioControl | [arcam-fmj](https://github.com/elupus/arcam_fmj) | Python / MIT | SSDP (ARCAM) | Arcam binary control over TCP :50000. Reply/poll-based integration; use receiver feedback where available |
| Trinnov Altitude | [trinnov-altitude](https://github.com/binarylogic/py-trinnov-altitude) | Python / Apache-2.0 | Configured IP; auto-discovery unverified | Altitude native text control over TCP :44100; Wake-on-LAN. TCP push, liveness probes and periodic authoritative reconciliation |

## Feature matrix

Cells describe support in the selected native-IP path. Model/source conditions are intentional. Input selection means physical device inputs, not merely changing a UHC music provider.

| Family | Power / standby | Volume / mute | Physical inputs | Playback / track metadata | Zones / grouping | DSP / presets |
|---|---|---|---|---|---|---|
| Denon / Marantz AVR | supported; network standby required for wake | supported | supported | model/source dependent; HEOS is separate | independent receiver zones where equipped | sound modes, tone and Audyssey controls where supported |
| WiiM | not established by this SDK review; do not model pause as power | supported; fixed-output mode is a capability constraint | model dependent: only installed inputs | supported; active-source dependent | WiiM grouping | presets verified; full EQ/DSP coverage not established in selected SDK |
| Yamaha MusicCast / Extended Control | supported with network standby enabled | supported | model dependent inputs | supported for network/USB and other exposed playback sources | receiver zones and MusicCast distribution/grouping | sound programs, EQ/tone, presets; model dependent |
| BluOS: Bluesound and BluOS-equipped products | no universal hardware power control established | supported; fixed-output mode applies | model dependent inputs exposed through BluOS | supported | player grouping | tone, balance, output modes and volume limits; model dependent |
| Cambridge Audio StreamMagic | supported on network-standby-capable models/configuration | model dependent: Evo/receiver/variable-preamp output; not physical Control Bus | model dependent inputs | supported; active-source dependent | not claimed as a general multiroom grouping API | presets, balance, EQ/room settings and volume limit; model dependent |
| Hegel legacy native-IP amplifiers | supported on covered network models | supported | supported | not claimed by selected hardware-control client | single amplifier | not claimed |
| Lyngdorf / Steinway Lyngdorf network products | supported on covered native-IP models | supported | supported | only streaming-module models and supported current sources | Zone B on equipped processors; not TDAI | RoomPerfect focus, voicings, sound processing and trims |
| Onkyo / Integra / eISCP-capable Pioneer | supported with network standby/configuration caveats | supported; range varies | supported | network-source/model dependent; not universal media browsing | independent receiver zones where equipped | listening/sound modes; model dependent |
| Sonos | no generic power off/on | supported | TV/line-in only on equipped models | supported; service content access remains separate | speaker groups, coordinator/topology, per-player and group volume | EQ, loudness and theater options on equipped models |
| Anthem native-IP MRX / AVM family | supported with standby-IP control configured | supported | supported | not song playback/metadata; input-signal diagnostics available | Zone 1 is selected client baseline; extend only after validation | listening modes and some ARC controls; model specific |
| Arcam and compatible native-IP JBL / AudioControl | power-off; LAN wake only on models/settings retaining network in standby | supported | supported | not claimed generally; selected streaming models need capability checks | receiver zones where equipped | decode modes and signal diagnostics; model dependent |
| Trinnov Altitude | acknowledged shutdown; wake through WOL with MAC | supported | supported | not song playback/metadata; processor state instead | not claimed as multiroom grouping | presets, upmixer and processor options |

## Family-level harvest notes

### 1. Denon / Marantz AVR

**Client:** [denonavr](https://github.com/ol-iver/denonavr) (Python, MIT).
**Observed HA requirement:** `denonavr==1.3.3`. This is a snapshot, not a proposed lockfile.

**Harvest:** Discovery manufacturer variants; HTTP generation probing; TCP message parsers and callbacks; Per-model input naming, zones and Audyssey quirks.

**Boundaries:**
- Do not conflate the AVR hardware API with the HEOS playback/grouping API.
- Some models permit only one control/Telnet connection. Share one connection inside UHC.
- Fetch actual volume range/step and source names; generation-specific HTTP behavior matters.

**Discovery record:**
```json
[
  {
    "method": "ssdp",
    "device_types": [
      "urn:schemas-upnp-org:device:MediaRenderer:1",
      "urn:schemas-upnp-org:device:MediaServer:1",
      "urn:schemas-denon-com:device:AiosDevice:1"
    ],
    "manufacturers": [
      "Denon",
      "DENON",
      "DENON PROFESSIONAL",
      "Marantz"
    ],
    "identity": "Fetch LOCATION device description; match device/model/firmware, not IP alone"
  }
]
```

**Sources:**
- https://github.com/ol-iver/denonavr
- https://pypi.org/project/denonavr/
- https://www.home-assistant.io/integrations/denonavr/
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/denonavr/manifest.json

### 2. WiiM

**Client:** [wiim](https://github.com/Linkplay2020/wiim) (Python, MIT).
**Observed HA requirement:** `wiim==0.2.1`. This is a snapshot, not a proposed lockfile.

**Harvest:** Zeroconf discovery and UPnP-device validation; Input/output and capability models; UPnP event handling and polling fallback; Grouping and current-state normalization.

**Boundaries:**
- Vendor-owned Python SDK, but API coverage varies by model, firmware and active source.
- Do not infer support for every other LinkPlay-based brand from WiiM support.
- Controlling a WiiM endpoint does not establish native-IP control of an attached non-network amplifier.

**Discovery record:**
```json
[
  {
    "method": "mdns_dnssd",
    "service_types": [
      "_linkplay._tcp.local."
    ],
    "ports": "Use resolved SRV/advertised endpoint"
  }
]
```

**Sources:**
- https://github.com/Linkplay2020/wiim
- https://pypi.org/project/wiim/
- https://www.home-assistant.io/integrations/wiim/
- https://faq.wiimhome.com/en/support/solutions/articles/72000646690-integrating-wiim-devices-with-home-assistant
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/wiim/manifest.json

### 3. Yamaha MusicCast / Extended Control

**Client:** [aiomusiccast](https://github.com/vigonotion/aiomusiccast) (Python, MIT).
**Observed HA requirement:** `aiomusiccast==0.15.0`. This is a snapshot, not a proposed lockfile.

**Other language candidates:** [yamaha-yxc-nodejs](https://github.com/foxthefox/yamaha-yxc-nodejs) (JavaScript, MIT): Useful second reference: discovery, explicit getFeatures, broad YXC command surface and examples.

**Harvest:** Discovery plus capability negotiation; Request/response schema; UDP notification lifecycle; Zone and distribution control; Volume ranges and source-dependent controls.

**Boundaries:**
- This row is MusicCast/YXC, not all Yamaha legacy receivers.
- SSDP manufacturer matching is only a candidate filter. Probe YXC features before accepting the device.
- Available zones, inputs, EQ and playback controls vary by model/source.

**Discovery record:**
```json
[
  {
    "method": "ssdp",
    "device_types": [],
    "manufacturers": [
      "Yamaha Corporation"
    ],
    "identity": "Fetch LOCATION device description; match device/model/firmware, not IP alone"
  }
]
```

**Sources:**
- https://github.com/vigonotion/aiomusiccast
- https://pypi.org/project/aiomusiccast/
- https://www.home-assistant.io/integrations/yamaha_musiccast/
- https://github.com/foxthefox/yamaha-yxc-nodejs
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/yamaha_musiccast/manifest.json

### 4. BluOS: Bluesound and BluOS-equipped products

**Client:** [pyblu](https://github.com/LouisChrist/pyblu) (Python, MIT).
**Observed HA requirement:** `pyblu==2.2.0`. This is a snapshot, not a proposed lockfile.

**Harvest:** mDNS identity; Endpoint/XML schemas; Playback/queue/group models; Settings availability, ranges and units.

**Boundaries:**
- Only BluOS-equipped devices are covered, not the entire NAD catalog.
- A BluOS streaming module need not expose every host-amplifier input or power function.
- Settings setters do not necessarily enforce the advertised limits; UHC should validate them.

**Discovery record:**
```json
[
  {
    "method": "mdns_dnssd",
    "service_types": [
      "_musc._tcp.local."
    ],
    "ports": "Use resolved SRV/advertised endpoint"
  }
]
```

**Sources:**
- https://github.com/LouisChrist/pyblu
- https://pypi.org/project/pyblu/
- https://www.home-assistant.io/integrations/bluesound/
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/bluesound/manifest.json

### 5. Cambridge Audio StreamMagic

**Client:** [aiostreammagic](https://github.com/noahhusby/aiostreammagic) (Python, MIT).
**Observed HA requirement:** `aiostreammagic==2.13.2`. This is a snapshot, not a proposed lockfile.

**Harvest:** mDNS variants; WebSocket subscriptions and reducers; Source-dependent capabilities; Variable output/preamp state; Network standby handling.

**Boundaries:**
- Limit scope to native-network streamers/all-in-ones, such as Evo, CXN, Edge NQ and compatible CXR units.
- Physical Control Bus volume control of a separate amplifier is excluded.
- ECO standby can disable networking. Network Standby is needed for LAN wake.
- Fixed-output models do not become amplifier-volume controllers just because they expose playback.

**Discovery record:**
```json
[
  {
    "method": "mdns_dnssd",
    "service_types": [
      "_stream-magic._tcp.local.",
      "_smoip._tcp.local."
    ],
    "ports": "Use resolved SRV/advertised endpoint"
  }
]
```

**Sources:**
- https://github.com/noahhusby/aiostreammagic
- https://pypi.org/project/aiostreammagic/
- https://www.home-assistant.io/integrations/cambridge_audio/
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/cambridge_audio/manifest.json

### 6. Hegel legacy native-IP amplifiers

**Client:** [hegel-ip-client](https://github.com/boazca/hegel-ip-client) (Python, MIT).
**Observed HA requirement:** `hegel-ip-client==0.1.4`. This is a snapshot, not a proposed lockfile.

**Harvest:** SSDP model recognition; Small ASCII command/response grammar; Command serialization; Reconnect and state callbacks.

**Boundaries:**
- HA explicitly lists Rost, H95, H120, H190, H190V, H390 and H590.
- Do not extrapolate this protocol/client to newer Hegel platforms without separate verification.

**Discovery record:**
```json
[
  {
    "method": "ssdp",
    "device_types": [
      "urn:schemas-upnp-org:device:MediaRenderer:1"
    ],
    "manufacturers": [
      "Hegel"
    ],
    "identity": "Fetch LOCATION device description; match device/model/firmware, not IP alone"
  }
]
```

**Sources:**
- https://github.com/boazca/hegel-ip-client
- https://pypi.org/project/hegel-ip-client/
- https://www.home-assistant.io/integrations/hegel/
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/hegel/manifest.json

### 7. Lyngdorf / Steinway Lyngdorf network products

**Client:** [lyngdorf](https://github.com/fishloa/lyngdorf) (Python, MIT).
**Observed HA requirement:** `lyngdorf==2.2.1`. This is a snapshot, not a proposed lockfile.

**Harvest:** Discovery and model/firmware capability tables; Protocol documentation/oracle evidence labels; Volume and trim units/ranges; Command pacing and state reconciliation; RoomPerfect and source catalogs.

**Boundaries:**
- Select native-IP TDAI/MP/P models; exclude a model that requires serial even if the Python package lists it.
- HA reports hardware testing for MP-60, TDAI-1120, TDAI-3400 and P200; other coverage can be specification-derived.
- Active sources have different transport semantics. For example, pause/seek availability is not universal.
- Pace commands and preserve relative-command ordering; do not turn a slider drag into an unbounded queue.

**Discovery record:**
```json
[
  {
    "method": "ssdp",
    "device_types": [
      "urn:schemas-upnp-org:device:MediaRenderer:2"
    ],
    "manufacturers": [
      "Lyngdorf",
      "Steinway Lyngdorf"
    ],
    "identity": "Fetch LOCATION device description; match device/model/firmware, not IP alone"
  }
]
```

**Sources:**
- https://github.com/fishloa/lyngdorf
- https://pypi.org/project/lyngdorf/
- https://www.home-assistant.io/integrations/lyngdorf/
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/lyngdorf/manifest.json

### 8. Onkyo / Integra / eISCP-capable Pioneer

**Client:** [aioonkyo](https://github.com/arturpragacz/aioonkyo) (Python, GPL-3.0-only).
**Observed HA requirement:** `aioonkyo==0.4.0`. This is a snapshot, not a proposed lockfile.

**Harvest:** SSDP variants plus native interview behavior; Framing and disconnect/recovery tests; Capability and model quirks; Command/reply semantics, subject to licensing review.

**Boundaries:**
- Only eISCP-capable Pioneer models. Older Pioneer protocols are a separate family.
- Current HA reference is aioonkyo, not the older onkyo-eiscp dependency.
- GPL-3.0-only: review reuse/licensing before copying or translating implementation into UHC.
- Do not assume all eISCP commands are available on every model.

**Discovery record:**
```json
[
  {
    "method": "ssdp",
    "device_types": [
      "urn:schemas-upnp-org:device:MediaRenderer:1",
      "urn:schemas-upnp-org:device:MediaRenderer:2",
      "urn:schemas-upnp-org:device:MediaRenderer:3"
    ],
    "manufacturers": [
      "ONKYO",
      "Onkyo & Pioneer Corporation",
      "Pioneer"
    ],
    "identity": "Fetch LOCATION device description; match device/model/firmware, not IP alone"
  },
  {
    "method": "eiscp_udp_discovery",
    "port": 60128,
    "notes": "Native receiver interview/discovery; do not assume SSDP alone proves usable eISCP control"
  }
]
```

**Sources:**
- https://github.com/arturpragacz/aioonkyo
- https://pypi.org/project/aioonkyo/
- https://www.home-assistant.io/integrations/onkyo/
- https://github.com/home-assistant/core/issues/162176
- https://www.gnu.org/licenses/gpl-faq.en.html
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/onkyo/manifest.json

### 9. Sonos

**Client:** [soco](https://github.com/SoCo/SoCo) (Python, MIT).
**Observed HA requirement:** `soco==0.31.5; sonos-websocket==0.2.0`. This is a snapshot, not a proposed lockfile.

**Other language candidates:** [sonos](https://github.com/bencevans/node-sonos) (JavaScript, MIT): Established alternate client and behavior reference.; [sonos-sdk / sonos-api](https://github.com/tatimblin/sonos-sdk) (Rust, MIT OR Apache-2.0): Evaluate as a native Rust option, not an HA-backed maturity claim. High-level SDK is sync/blocking; isolate from Tokio worker threads.

**Harvest:** Discovery and stable device identity; SCPD/action schemas; GENA event subscription lifecycle; Group topology and queue models; Fixed/variable outputs and model-specific controls.

**Boundaries:**
- UPnP must be enabled where current Sonos privacy/security settings require it.
- Device-to-controller event callbacks must be reachable.
- Local playback control does not guarantee music-service catalog/search/authentication support.
- Do not treat stopped playback as a powered-off amplifier.

**Discovery record:**
```json
[
  {
    "method": "ssdp",
    "device_types": [
      "urn:schemas-upnp-org:device:ZonePlayer:1"
    ],
    "manufacturers": [],
    "identity": "Fetch LOCATION device description; match device/model/firmware, not IP alone"
  },
  {
    "method": "mdns_dnssd",
    "service_types": [
      "_sonos._tcp.local."
    ],
    "ports": "Use resolved SRV/advertised endpoint"
  }
]
```

**Sources:**
- https://github.com/SoCo/SoCo
- https://www.home-assistant.io/integrations/sonos/
- https://github.com/bencevans/node-sonos
- https://github.com/tatimblin/sonos-sdk
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/sonos/manifest.json

### 10. Anthem native-IP MRX / AVM family

**Client:** [anthemav](https://github.com/nugget/python-anthemav) (Python, MIT).
**Observed HA requirement:** `anthemav==1.4.2`. This is a snapshot, not a proposed lockfile.

**Harvest:** Model identification and x20/x40 command tables; Input-name and listening-mode parsing; Standby-IP behavior; Connection ownership and reconnect; Zone 1 tests before generalizing zones.

**Boundaries:**
- README is older than parts of current source: source contains x40 and MDX handling. Test the actual generation rather than relying only on README model list.
- Inspected client initializes Zone 1; do not promise all receiver/distribution zones.
- A persistent client may compete with vendor remote/ARC software on limited-connection models.
- No auto-discovery mechanism verified in the selected HA integration.

**Discovery record:**
```json
[
  {
    "method": "configured_host",
    "automatic_discovery": "Not verified in the selected client/integration; not a claim that no discovery mechanism exists"
  }
]
```

**Sources:**
- https://github.com/nugget/python-anthemav
- https://pypi.org/project/anthemav/
- https://www.home-assistant.io/integrations/anthemav/
- https://github.com/nugget/python-anthemav/blob/master/anthemav/connection.py
- https://github.com/nugget/python-anthemav/blob/master/anthemav/protocol.py
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/anthemav/manifest.json

### 11. Arcam and compatible native-IP JBL / AudioControl

**Client:** [arcam-fmj](https://github.com/elupus/arcam_fmj) (Python, MIT).
**Observed HA requirement:** `arcam-fmj==3.0.1`. This is a snapshot, not a proposed lockfile.

**Harvest:** Binary frame parser and response errors; Model-specific command enums; Zone capability discovery; Network-standby boundary; Input/sound-mode mappings.

**Boundaries:**
- Restrict to native-IP models such as compatible AVR/AV units, SA30 and ST60. Exclude serial-only variants.
- Arcam SSDP manufacturer matcher is not proof of discovery support on every rebadged JBL/AudioControl unit.
- Older models disable the network in standby. IR/serial workarounds are excluded; expose wake as unsupported.
- Some newer models allow HDMI & IP On to retain LAN power control.

**Discovery record:**
```json
[
  {
    "method": "ssdp",
    "device_types": [
      "urn:schemas-upnp-org:device:MediaRenderer:1"
    ],
    "manufacturers": [
      "ARCAM"
    ],
    "identity": "Fetch LOCATION device description; match device/model/firmware, not IP alone"
  }
]
```

**Sources:**
- https://github.com/elupus/arcam_fmj
- https://pypi.org/project/arcam-fmj/
- https://www.home-assistant.io/integrations/arcam_fmj/
- https://github.com/home-assistant/core/blob/dev/homeassistant/components/arcam_fmj/manifest.json

### 12. Trinnov Altitude

**Client:** [trinnov-altitude](https://github.com/binarylogic/py-trinnov-altitude) (Python, Apache-2.0).

**Harvest:** Protocol parser and normalized event model; Catalog/identity semantics; ACK versus observed-state tests; Liveness and reconnection; Shutdown/WOL sequencing.

**Boundaries:**
- Configured-IP baseline; no automatic discovery established by this review.
- Source profiles and processing presets are separate identities.
- Sending bytes, receiving an ACK and observing the target state are different completion levels.
- Never blindly retry relative commands or toggles. Wake and shutdown require lifecycle handling.

**Discovery record:**
```json
[
  {
    "method": "configured_host",
    "automatic_discovery": "Not verified in the selected client/integration; not a claim that no discovery mechanism exists"
  }
]
```

**Sources:**
- https://github.com/binarylogic/py-trinnov-altitude
- https://pypi.org/project/trinnov-altitude/
- https://github.com/binarylogic/py-trinnov-altitude/blob/master/README.md


## Rust implementation leverage

- [mdns-sd](https://docs.rs/mdns-sd/latest/mdns_sd/): One DNS-SD browser/cache across mDNS families; sync and async receive interfaces.
- [ssdp-client](https://docs.rs/ssdp-client/latest/ssdp_client/): Async SSDP search; retain LOCATION/USN and resolve advertised descriptions.
- [rupnp](https://docs.rs/rupnp/latest/rupnp/): UPnP discovery, service descriptions and SOAP actions. Do not assume this alone supplies the complete event-subscription lifecycle.
- [sonos-sdk / sonos-api](https://github.com/tatimblin/sonos-sdk): Sonos-specific Rust candidate; evaluate maturity and isolate blocking APIs.

For a Rust UHC backend, Python clients can supply compatibility behavior, fixtures and schema references without embedding Home Assistant. Preserve provenance when adapting permissively licensed code. `aioonkyo` is GPL-3.0-only: settle the licensing approach before incorporating or translating it; translation into Rust does not by itself remove the original license obligations. See the [GNU license FAQ](https://www.gnu.org/licenses/gpl-faq.en.html).

## Proposed shared capability model

```json
{
  "identity": [
    "vendor",
    "model",
    "firmware",
    "stable_device_id",
    "discovery_provenance"
  ],
  "volume": [
    "absolute_supported",
    "relative_supported",
    "mute_supported",
    "fixed_or_variable",
    "native_units",
    "min",
    "max",
    "step",
    "user_ceiling"
  ],
  "power": [
    "standby_supported",
    "wake_supported",
    "wake_method",
    "network_standby_requirement",
    "observed_state"
  ],
  "inputs": [
    "stable_input_id",
    "display_name",
    "available",
    "physical_or_virtual"
  ],
  "state": [
    "observed_at",
    "feedback_transport",
    "connected",
    "confirmed_or_pending"
  ],
  "features": [
    "independent_zones",
    "grouping",
    "transport_by_current_source",
    "dsp_controls_by_model"
  ]
}
```

This model is a design proposal, not a claim about the current UHC schema.

## Suggested implementation order

1. Use Denon/Marantz as baseline; exploit existing knowledge rather than replacing it.
2. First additions: WiiM, MusicCast, BluOS and StreamMagic for broader local control coverage.
3. Focused amplifier additions: Hegel and Lyngdorf.
4. Add Onkyo after settling the GPL reuse route; add Sonos when grouping/topology is in scope.
5. Model-specific follow-ons: Anthem, Arcam and Trinnov, with real-device transcripts.

## Validation before advertising compatibility

- Create fixture transcripts for discovery, identity, capability negotiation, commands, notifications, errors and reconnect.
- Tag evidence as implementation-derived, vendor-spec-verified or hardware-verified; do not turn source-derived support into a hardware-tested claim.
- Test packet fragmentation, combined replies, stale and out-of-order events, interrupted connections and reboot.
- Test volume min/max/step, fixed output, mute, source changes and zone separation without sending unsafe default levels.
- One device connection owner inside UHC; frontends consume UHC state rather than opening competing device sessions.
- Keep UHC music providers separate from hardware input/volume ownership.

Keep SSDP and mDNS as separate discovery mechanisms. Discovery finds candidates; successful model identification and capability negotiation authorize an adapter. Resolve advertised ports rather than guessing them. Manual IP should remain available when multicast cannot cross network segments.
