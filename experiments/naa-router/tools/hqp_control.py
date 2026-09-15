#!/usr/bin/env python3
"""Small, bounded HQPlayer Desktop control client.

The element names and attributes below were independently implemented from
the locally inspected HQPlayer ControlInterface source.  Source attribution:
HQPlayer Control, Copyright (C) 2011-2026 Jussi Laako, MIT license; the
reviewed source snapshot has SHA-256
31d6621d693e7c973d1015d81e4264fe236df99030a670817dd7a786b3fa9d77.
This is HQPlayer's control API, not NAA.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, subject to the MIT license conditions and disclaimer.
"""

import argparse
import json
import socket
import sys
import time
import xml.etree.ElementTree as ET

DEFAULT_HOST = "127.0.0.1"
DEFAULT_PORT = 4321
MAX_DOCUMENT = 1024 * 1024
LOOPBACK = {"127.0.0.1", "localhost", "::1"}


class HQPlayerError(RuntimeError):
    pass


def _json_element(element):
    value = {"tag": element.tag, "attributes": dict(element.attrib)}
    text = (element.text or "").strip()
    if text:
        value["text"] = text
    children = [_json_element(child) for child in element]
    if children:
        value["children"] = children
    return value


class _DocumentReader:
    """Incrementally split concatenated XML documents without recv framing."""

    def __init__(self, limit=MAX_DOCUMENT):
        self.limit = limit
        self.buffer = bytearray()
        self.depth = 0
        self.complete = False
        self.token = bytearray()
        self.mode = "text"
        self.quote = None

    def feed(self, data):
        documents = []
        for byte in data:
            # Ignore inter-document whitespace, while retaining declarations
            # and all bytes belonging to the next document.
            if not self.buffer and byte in b" \t\r\n":
                continue
            self.buffer.append(byte)
            if len(self.buffer) > self.limit:
                raise HQPlayerError("HQPlayer XML document exceeds size limit")
            self._scan(byte)
            if self.complete:
                raw = bytes(self.buffer)
                try:
                    documents.append(ET.fromstring(raw))
                except ET.ParseError as exc:
                    raise HQPlayerError(f"invalid HQPlayer XML: {exc}") from exc
                self.buffer.clear()
                self.depth = 0
                self.complete = False
                self.token.clear()
                self.mode = "text"
                self.quote = None
        return documents

    def _scan(self, byte):
        if self.mode == "comment":
            self.token.append(byte)
            if self.token.endswith(b"-->"):
                self.mode, self.token = "text", bytearray()
            return
        if self.mode == "cdata":
            self.token.append(byte)
            if self.token.endswith(b"]]>"):
                self.mode, self.token = "text", bytearray()
            return
        if self.mode == "quote":
            self.token.append(byte)
            if byte == self.quote:
                self.mode, self.quote = "tag", None
            return
        if self.mode == "tag":
            self.token.append(byte)
            if self.token.upper().startswith((b"<!DOCTYPE", b"<!ENTITY")):
                raise HQPlayerError("DOCTYPE and ENTITY declarations are not allowed")
            if byte in (ord('"'), ord("'")):
                self.mode, self.quote = "quote", byte
            elif byte == ord('>'):
                token = bytes(self.token).strip()
                if token.startswith(b"<!--"):
                    self.mode = "text" if token.endswith(b"-->") else "comment"
                elif token.startswith(b"<![CDATA["):
                    self.mode = "text" if token.endswith(b"]]>" ) else "cdata"
                else:
                    if token.startswith(b"</"):
                        self.depth -= 1
                    elif not token.startswith((b"<?", b"<!")) and not token.endswith(b"/>"):
                        self.depth += 1
                    elif token.endswith(b"/>") and not token.startswith((b"<?", b"<!")):
                        if self.depth == 0:
                            self.complete = True
                    if self.depth == 0 and token.startswith(b"</"):
                        self.complete = True
                if self.mode == "tag":
                    self.mode, self.token = "text", bytearray()
            return
        if byte == ord('<'):
            self.mode, self.token = "tag", bytearray(b"<")

    def finish(self):
        if self.buffer:
            raise HQPlayerError("HQPlayer closed with an incomplete XML document")


class HQPlayerControl:
    def __init__(self, host=DEFAULT_HOST, port=DEFAULT_PORT, timeout=3.0,
                 max_document=MAX_DOCUMENT):
        if host not in LOOPBACK:
            raise ValueError("only loopback HQPlayer hosts are allowed")
        if not (1 <= int(port) <= 65535):
            raise ValueError("port must be between 1 and 65535")
        if timeout <= 0 or timeout > 30:
            raise ValueError("timeout must be > 0 and <= 30 seconds")
        self.host, self.port, self.timeout = host, int(port), float(timeout)
        self.max_document = max_document

    def request(self, command, **attrs):
        root = ET.Element(command, {key: str(value) for key, value in attrs.items()})
        payload = b'<?xml version="1.0" encoding="UTF-8"?>\n' + ET.tostring(root, encoding="utf-8") + b"\n"
        reader = _DocumentReader(self.max_document)
        deadline = time.monotonic() + self.timeout
        try:
            remaining = max(0.001, deadline - time.monotonic())
            with socket.create_connection((self.host, self.port), remaining) as sock:
                sock.sendall(payload)
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        raise HQPlayerError("timed out waiting for HQPlayer response")
                    sock.settimeout(remaining)
                    try:
                        chunk = sock.recv(4096)
                    except socket.timeout as exc:
                        raise HQPlayerError("timed out waiting for HQPlayer response") from exc
                    if not chunk:
                        reader.finish()
                        break
                    docs = reader.feed(chunk)
                    for document in docs:
                        result = document.attrib.get("result")
                        if document.tag.lower() in {"error", "failure"} or (result is not None and result != "OK"):
                            detail = document.attrib.get("message", document.attrib.get("error", "HQPlayer rejected command"))
                            raise HQPlayerError(detail)
                        if document.tag == command:
                            return document
        except OSError as exc:
            raise HQPlayerError(f"HQPlayer connection failed: {exc}") from exc
        raise HQPlayerError("HQPlayer returned no XML response")

    def info(self): return self.request("GetInfo")
    def state(self): return self.request("State")
    def status(self): return self.request("Status", subscribe=0)
    def rates(self): return self.request("GetRates")
    def volume_range(self): return self.request("VolumeRange")
    def playlist_get(self): return self.request("PlaylistGet", picture=0)
    def play(self): return self.request("Play", last=0)
    def pause(self): return self.request("Pause")
    def stop(self): return self.request("Stop")
    def volume(self, value): return self.request("Volume", value=value)
    def set_rate(self, index): return self.request("SetRate", value=index)
    def select_track(self, index): return self.request("SelectTrack", index=index)
    def playlist_add(self, uri): return self.request("PlaylistAdd", uri=uri, queued=0, clear=0, start=0, freewheel=0)
    def playlist_clear(self): return self.request("PlaylistClear")


def _parser():
    parser = argparse.ArgumentParser(description="Bounded loopback HQPlayer control client")
    parser.add_argument("--host", default=DEFAULT_HOST)
    parser.add_argument("--port", type=int, default=DEFAULT_PORT)
    parser.add_argument("--timeout", type=float, default=3.0)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in ("info", "state", "status", "rates", "volume-range", "playlist-get", "play", "pause", "stop", "playlist-clear"):
        sub.add_parser(name)
    p = sub.add_parser("volume"); p.add_argument("value", type=float)
    p = sub.add_parser("set-rate"); p.add_argument("index", type=int)
    p = sub.add_parser("select-track"); p.add_argument("index", type=int)
    p = sub.add_parser("playlist-add"); p.add_argument("uri")
    return parser


def main(argv=None):
    args = _parser().parse_args(argv)
    try:
        client = HQPlayerControl(args.host, args.port, args.timeout)
        method = {"volume-range": "volume_range", "playlist-get": "playlist_get", "set-rate": "set_rate", "select-track": "select_track", "playlist-add": "playlist_add", "playlist-clear": "playlist_clear"}.get(args.command, args.command.replace("-", "_"))
        result = getattr(client, method)(*([getattr(args, "value")] if args.command == "volume" else [getattr(args, "index")] if args.command in ("set-rate", "select-track") else [args.uri] if args.command == "playlist-add" else []))
        print(json.dumps(_json_element(result), sort_keys=True))
        return 0
    except (HQPlayerError, ValueError) as exc:
        print(f"hqp-control: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
