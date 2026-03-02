#!/usr/bin/env python3
"""Diagnose knob ↔ bridge connectivity issues.

Usage: python3 diagnose_knob.py <bridge_ip> [zone_id]
"""
import socket, struct, sys, json, urllib.request, urllib.error
from urllib.parse import quote

BRIDGE_IP = sys.argv[1] if len(sys.argv) > 1 else "192.168.50.225"
ZONE_ID = sys.argv[2] if len(sys.argv) > 2 else ""
BRIDGE_PORT = 8088
UDP_PORT = BRIDGE_PORT + 1
MAGIC = 0x524B

def fetch_json(path):
    url = f"http://{BRIDGE_IP}:{BRIDGE_PORT}{path}"
    try:
        with urllib.request.urlopen(url, timeout=5) as r:
            return json.loads(r.read())
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError) as e:
        return {"_error": str(e)}

# 1. Check zones
print("=" * 60)
print("1. ZONES IN AGGREGATOR")
print("=" * 60)
zones = fetch_json("/api/zones")
if isinstance(zones, dict) and "_error" in zones:
    print(f"  ❌ Bridge unreachable: {zones['_error']}")
    sys.exit(1)

if not zones:
    print("  ❌ No zones found! Check adapter connections.")
else:
    for z in zones:
        zid = z.get("zone_id", "?")
        name = z.get("display_name") or z.get("zone_name", "?")
        state = z.get("state", "?")
        np = z.get("now_playing")
        vc = z.get("volume_control")
        title = np.get("title", "?") if np else "<none>"
        vol = vc.get("value", "?") if vc else "<none>"
        vol_out = vc.get("output_id", "?") if vc else "<none>"
        prefix = "roon" if zid.startswith("roon:") else zid.split(":")[0]
        print(f"  [{prefix}] {zid}")
        print(f"    name={name}, state={state}")
        print(f"    now_playing: {title}")
        print(f"    volume: {vol} (output_id={vol_out})")
        print()

# 2. Check specific zone via manifest
if ZONE_ID:
    test_zone = ZONE_ID
elif zones:
    # Pick first roon zone, or first zone
    roon_zones = [z for z in zones if z.get("zone_id", "").startswith("roon:")]
    test_zone = (roon_zones or zones)[0]["zone_id"]
else:
    test_zone = None

if test_zone:
    print("=" * 60)
    print(f"2. MANIFEST FOR: {test_zone}")
    print("=" * 60)
    manifest = fetch_json(f"/knob/manifest?zone_id={quote(test_zone)}")
    if "_error" in manifest:
        print(f"  ❌ Manifest error: {manifest['_error']}")
    else:
        fast = manifest.get("fast", {})
        print(f"  playing: {fast.get('is_playing')}")
        print(f"  volume: {fast.get('volume')} (min={fast.get('volume_min')}, max={fast.get('volume_max')}, step={fast.get('volume_step')})")
        print(f"  volume_type: {fast.get('volume_type')}")
        print(f"  seek: {fast.get('seek_position')}, length: {fast.get('length')}")
        screens = manifest.get("screens", [])
        for s in screens:
            stype = s.get("type", "?")
            if stype == "media":
                lines = s.get("lines", [])
                print(f"  media screen: {[l.get('text','') for l in lines]}")
        print()

    # 3. UDP test
    print("=" * 60)
    print(f"3. UDP FAST-PATH FOR: {test_zone}")
    print("=" * 60)
    req = struct.pack("<H", MAGIC)
    req += b"\x00" * 20
    req += test_zone.encode("utf-8").ljust(32, b"\x00")[:32]

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(2.0)
    sock.sendto(req, (BRIDGE_IP, UDP_PORT))
    try:
        data, addr = sock.recvfrom(128)
        if len(data) >= 48:
            magic, ver, flags = struct.unpack_from("<HBB", data, 0)
            sha = data[4:24]
            vol, vol_min, vol_max, vol_step = struct.unpack_from("<ffff", data, 24)
            seek, length = struct.unpack_from("<iI", data, 40)
            playing = bool(flags & 1)
            print(f"  ✅ Got response ({len(data)} bytes)")
            print(f"  playing={playing}, flags=0x{flags:02x}")
            print(f"  volume={vol:.1f} (min={vol_min:.1f}, max={vol_max:.1f}, step={vol_step:.1f})")
            print(f"  seek={seek}, length={length}")
            if vol == 0 and vol_min == 0 and vol_max == 0:
                print("  ⚠️  Volume all zeros — volume_control is None in aggregator!")
            if seek == -1 and length == 0:
                print("  ⚠️  No seek/duration data")
        else:
            print(f"  ⚠️  Short response: {len(data)} bytes")
    except socket.timeout:
        print("  ❌ No UDP response (timeout)")
    finally:
        sock.close()
else:
    print("\nNo zones to test.")
