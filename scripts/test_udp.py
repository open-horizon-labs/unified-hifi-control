#!/usr/bin/env python3
"""Test the UDP fast-path protocol against a running bridge."""

import socket
import struct
import sys

MAGIC = 0x524B
REQUEST_SIZE = 54
RESPONSE_SIZE = 48

def main():
    host = sys.argv[1] if len(sys.argv) > 1 else "127.0.0.1"
    port = int(sys.argv[2]) if len(sys.argv) > 2 else 8089
    zone_id = sys.argv[3] if len(sys.argv) > 3 else ""

    # Build request: magic(2) + sha(20) + zone_id(32) = 54 bytes
    req = bytearray(REQUEST_SIZE)
    struct.pack_into("<H", req, 0, MAGIC)
    # sha[20] = all zeros (no cached SHA)
    # zone_id
    zone_bytes = zone_id.encode("utf-8")[:31]
    req[22:22 + len(zone_bytes)] = zone_bytes

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.settimeout(2.0)

    print(f"Sending {REQUEST_SIZE}-byte request to {host}:{port}")
    if zone_id:
        print(f"  zone_id: {zone_id}")
    else:
        print("  zone_id: (empty — bridge picks first zone)")

    sock.sendto(req, (host, port))

    try:
        data, addr = sock.recvfrom(256)
    except socket.timeout:
        print("ERROR: No response (timeout 2s)")
        sys.exit(1)

    print(f"Received {len(data)} bytes from {addr[0]}:{addr[1]}")

    if len(data) < RESPONSE_SIZE:
        print(f"ERROR: Response too short ({len(data)} < {RESPONSE_SIZE})")
        sys.exit(1)

    magic, version, flags = struct.unpack_from("<HBB", data, 0)
    sha_raw = data[4:24]
    sha = sha_raw.split(b"\x00")[0].decode("utf-8", errors="replace")
    volume, vol_min, vol_max, vol_step = struct.unpack_from("<ffff", data, 24)
    seek_pos, length = struct.unpack_from("<iI", data, 40)

    print("\n--- Response ---")
    print(f"  magic:    0x{magic:04X} {'✓' if magic == MAGIC else '✗ WRONG'}")
    print(f"  version:  {version}")
    print(f"  flags:    0b{flags:08b}")
    print(f"    playing:       {bool(flags & 0x01)}")
    print(f"    play_allowed:  {bool(flags & 0x02)}")
    print(f"    pause_allowed: {bool(flags & 0x04)}")
    print(f"    next_allowed:  {bool(flags & 0x08)}")
    print(f"    prev_allowed:  {bool(flags & 0x10)}")
    print(f"  sha:      {sha}")
    print(f"  volume:   {volume:.1f} (min={vol_min:.1f}, max={vol_max:.1f}, step={vol_step:.1f})")
    print(f"  seek:     {seek_pos}s / {length}s")

    if vol_min < 0:
        print(f"  display:  {volume:.0f} dB")
    else:
        print(f"  display:  {volume:.0f}%")

    print("\n✓ UDP fast-path working")

if __name__ == "__main__":
    main()
