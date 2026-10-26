#!/usr/bin/env python3
"""Read-only audit of final browser assets, after wasm-bindgen/wasm-opt and hashing.

Do not strip the compiler's intermediate module: wasm-bindgen needs its metadata.
Dioxus owns transformation and content hashes; this gate rejects leaked debug data,
including poisoned/stale cache restores, without rewriting immutable asset URLs.
"""
import argparse
import json
from pathlib import Path
import re

MAGIC = b"\0asm\x01\0\0\0"
DEBUG_NAMES = {"sourceMappingURL", "external_debug_info"}


def uleb(data, offset):
    value = 0
    for index in range(5):
        if offset >= len(data):
            raise ValueError("truncated section length")
        byte = data[offset]
        offset += 1
        if index == 4 and byte > 15:
            raise ValueError("section length exceeds u32")
        value |= (byte & 127) << (index * 7)
        if byte < 128:
            return value, offset
    raise ValueError("invalid section length")


def custom_sections(data):
    if data[:8] != MAGIC:
        raise ValueError("not a WebAssembly 1 module")
    offset = 8
    sections = []
    while offset < len(data):
        section_id = data[offset]
        if section_id > 13:
            raise ValueError(f"unknown section id {section_id}")
        length, payload = uleb(data, offset + 1)
        end = payload + length
        if end > len(data):
            raise ValueError("truncated section payload")
        if section_id == 0:
            name_length, name_start = uleb(data, payload)
            if name_start + name_length > end:
                raise ValueError("custom section name exceeds payload")
            name = data[name_start:name_start + name_length].decode("utf-8")
            sections.append({"name": name, "bytes": end - offset,
                             "content_bytes": end - name_start - name_length})
        offset = end
    return sections


def audit(root):
    root = Path(root)
    modules = [root] if root.is_file() else sorted(root.rglob("*.wasm"))
    if not modules:
        raise ValueError(f"no final WASM modules under {root}")
    result = []
    for path in modules:
        data = path.read_bytes()
        sections = custom_sections(data)
        # Dioxus's asset pass can emit an empty name-section envelope (7 bytes).
        # It contains no names/debug data. Reject any actual name subsection.
        debug = [s for s in sections if (s["name"] == "name" and s["content_bytes"] > 0)
                 or s["name"] in DEBUG_NAMES
                 or s["name"].startswith((".debug_", ".zdebug_"))]
        if debug:
            raise ValueError(f"{path}: shipped debug sections: " + ", ".join(s["name"] for s in debug))
        result.append({"path": str(path), "bytes": len(data), "custom_sections": sections})
    if root.is_dir():
        for path in root.rglob("*"):
            if path.name.endswith((".js.map", ".wasm.map", ".css.map")):
                raise ValueError(f"{path}: shipped browser source map")
            if path.suffix in {".js", ".css"} and path.is_file():
                if re.search(rb"(?m)(?:^\s*//[#@]|/\*[#@])\s*sourceMappingURL\s*=", path.read_bytes()):
                    raise ValueError(f"{path}: shipped source-map reference")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    args = parser.parse_args()
    try:
        print(json.dumps(audit(args.path), indent=2))
    except (ValueError, OSError, UnicodeError) as error:
        parser.exit(1, f"Release WASM audit failed: {error}\n")


if __name__ == "__main__":
    main()
