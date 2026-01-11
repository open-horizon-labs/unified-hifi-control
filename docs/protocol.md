# API Protocol

**Note:** This API is internal and changes frequently as needed. It is not considered stable or reliable for third-party integrations. If you're building a client, expect breaking changes.

## Zone Object

Zones are returned by `/zones`, `/admin/status.json`, and other endpoints.

```json
{
  "zone_id": "roon:1234",
  "zone_name": "Living Room",
  "output_name": "USB DAC",
  "device_name": "Raspberry Pi",
  "dsp": {
    "type": "hqplayer",
    "instance": "HQP-Main",
    "pipeline": "/hqp/pipeline?zone_id=roon%3A1234",
    "matrix": "/hqp/matrix/profiles",
    "profiles": "/hqp/profiles"
  }
}
```

### DSP Field

The `dsp` field is **only present** when the zone is linked to a DSP processor (currently only HQPlayer).

| Field | Type | Description |
|-------|------|-------------|
| `type` | string | DSP type, currently always `"hqplayer"` |
| `instance` | string | Name of the HQPlayer instance |
| `pipeline` | string | URL to fetch/set pipeline settings |
| `matrix` | string | URL to fetch/set matrix profiles (convolution/PEQ for room correction) |
| `profiles` | string? | URL to fetch configuration profiles. **Only present** for instances that support profile switching. |

### Checking for DSP

```javascript
// JavaScript
const hasDsp = !!zone.dsp;
const hasProfiles = !!zone.dsp?.profiles;
const hasMatrix = !!zone.dsp?.matrix;
```

```swift
// Swift
let hasDsp = zone.dsp != nil
let hasProfiles = zone.dsp?.profiles != nil
let hasMatrix = zone.dsp?.matrix != nil
```

## Changelog

- **2026-01-10**: Added `dsp` field to zone objects. Replaces client-side `/hqp/zones/links` fetching.
- **2026-01-10**: `dsp.profiles` is now conditional based on instance capabilities.
