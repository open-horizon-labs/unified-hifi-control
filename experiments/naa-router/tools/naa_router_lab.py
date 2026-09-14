#!/usr/bin/env python3
"""Adversarial NAA router lab. Explicit loopback fixtures; no vendor or real DAC.

Run --binary native/naa-router/target/debug/naa-router. --demo emits the measured
A→B→A route/payload transcript. These checks prove software routing properties,
not real Embedded reconnect, authentication validity or hardware payload delivery.
"""
import argparse
import contextlib
import hashlib
import http.client
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import xml.etree.ElementTree as ET

import hqp_control

BINARY = None
VIRTUAL = "hiphi:router"
TIMEOUT = 5


def exact(sock, n):
    result = bytearray()
    while len(result) < n:
        chunk = sock.recv(n - len(result))
        if not chunk:
            raise EOFError(f"socket closed after {len(result)}/{n} bytes")
        result.extend(chunk)
    return bytes(result)


def line(sock):
    result = bytearray()
    while not result.endswith(b"\n"):
        result.extend(exact(sock, 1))
        if len(result) > 65536:
            raise ValueError("fixture control line too long")
    return bytes(result)


def fragmented(sock, data):
    # Fragment at deliberately inconvenient boundaries, including XML tokens.
    offsets = (1, 2, 5, 13, 31)
    cursor = 0
    for size in offsets:
        sock.sendall(data[cursor:cursor + size])
        cursor += size
        if cursor >= len(data):
            return
    sock.sendall(data[cursor:])


def raw_request(method, path, host, extra_headers=(), body=b""):
    headers = [f"{method} {path} HTTP/1.1", f"Host: {host}"]
    headers.extend(f"{k}: {v}" for k, v in extra_headers)
    if body and not any(k.lower() == "content-type" for k, _ in extra_headers):
        headers.append("Content-Type: application/json")
    if not any(k.lower() == "content-length" for k, _ in extra_headers):
        headers.append(f"Content-Length: {len(body)}")
    return ("\r\n".join(headers) + "\r\n\r\n").encode() + body


def status_of(raw_response):
    return raw_response.split(b"\r\n", 1)[0].decode(errors="replace")


def control(kind, **attrs):
    root = ET.Element("networkaudio")
    ET.SubElement(root, "operation", {"type": kind, **{k: str(v) for k, v in attrs.items()}})
    return ET.tostring(root) + b"\n"


def response(kind, attrs=None, children=()):
    root = ET.Element("networkaudio")
    op = ET.SubElement(root, "operation", {**(attrs or {}), "type": kind, "result": "1"})
    op.extend(children)
    return ET.tostring(root) + b"\n"


def operation(raw):
    root = ET.fromstring(raw)
    assert root.tag == "networkaudio", raw
    return root.find("operation")


def audio_record(payload, dsd=False, metadata=b"", picture=b"", position=b""):
    assert dsd or len(payload) % 4 == 0
    samples = len(payload) if dsd else len(payload) // 4
    mask = 2 | (4 if position else 0) | (8 if metadata else 0) | (16 if picture else 0)
    return struct.pack("<8I", mask, samples, len(position), len(metadata), len(picture), 0, 0, 0) + payload + position + metadata + picture


def reserved_port():
    """A loopback port nobody is listening on (bind then release): a
    deterministic connection-refused target, no real network device."""
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def wait_until(predicate, timeout=TIMEOUT):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.01)
    raise AssertionError("condition did not become true before deadline")


class FakeNaa:
    """Independent protocol fixture with endpoint-specific identity and offers."""
    def __init__(self, name, device_id, rate, devices=None, refuse_start=False, stall=False, feedback_override=None, result_overrides=None):
        self.name, self.device_id, self.rate = name, device_id, rate
        self.devices = devices or [(device_id, name)]
        self.refuse_start, self.stall = refuse_start, stall
        self.feedback_override = feedback_override
        self.result_overrides = result_overrides or {}
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.listener.settimeout(0.1)
        self.port = self.listener.getsockname()[1]
        self.stop = threading.Event()
        self.stall_started = threading.Event()
        self.events = []
        self.connections = []
        self.workers = []
        self.errors = []
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def reply(self, kind, attrs=None, children=()):
        raw = response(kind, attrs, children)
        if kind not in self.result_overrides:
            return raw
        root = ET.fromstring(raw)
        op = root.find("operation")
        value = self.result_overrides[kind]
        if value is None:
            op.attrib.pop("result", None)
        else:
            op.set("result", str(value))
        return ET.tostring(root) + b"\n"

    def accept(self):
        while not self.stop.is_set():
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            conn.settimeout(TIMEOUT)
            self.connections.append(conn)
            worker = threading.Thread(target=self.session, args=(conn,), daemon=True)
            self.workers.append(worker)
            worker.start()

    def session(self, conn):
        session = len(self.connections)
        try:
            auth = line(conn)
            self.events.append((session, "auth", auth))
            nonce = ET.fromstring(auth).attrib["nonce"]
            # Deliberately preserve exact quotes/spacing; proxy must keep auth opaque.
            reply = f"<authenticate  endpoint='{self.name}' endpoint_id='{self.name}-distinct-identity' nonce='{nonce}' opaque='&amp;'/>\n".encode()
            self.events.append((session, "auth_reply", reply))
            fragmented(conn, reply)
            dsd = False
            while not self.stop.is_set():
                first = exact(conn, 1)
                if first == b"<":
                    raw = first + line(conn)
                    if ET.fromstring(raw).tag == "authenticate":
                        # A subsequent authenticate (not wrapped in <networkaudio>) is
                        # a mid-session re-handshake, relayed opaquely like the first.
                        nonce = ET.fromstring(raw).attrib["nonce"]
                        self.events.append((session, "reauth", raw))
                        reply = f"<authenticate  endpoint='{self.name}' endpoint_id='{self.name}-distinct-identity' nonce='{nonce}' opaque='&amp;'/>\n".encode()
                        self.events.append((session, "reauth_reply", reply))
                        fragmented(conn, reply)
                        continue
                    op = operation(raw)
                    kind = op.attrib["type"]
                    self.events.append((session, kind, raw))
                    if kind == "getdevices":
                        children = [ET.Element("device", id=i, description=n) for i, n in self.devices]
                        fragmented(conn, self.reply(kind, dict(op.attrib), children))
                    elif kind == "initialize":
                        if op.attrib.get("device") not in [i for i, _ in self.devices]:
                            raise AssertionError(f"{self.name} received wrong DAC id: {raw!r}")
                        fragmented(conn, self.reply(kind, {**op.attrib, "version": "6", "position": "1"}))
                    elif kind == "getformats":
                        children = [ET.Element("format", bits="32", channels="2", dsd="0", pcm="1", sdm="0", rate=str(self.rate)),
                                    ET.Element("format", bits="1", channels="2", dsd="1", pcm="1", sdm="1", rate=str(self.rate * 64))]
                        wire = self.reply(kind, children=children)
                        self.events.append((session, "formats_reply", wire))
                        fragmented(conn, wire)
                    elif kind == "start":
                        dsd = op.attrib.get("stream") == "dsd"
                        if self.refuse_start:
                            conn.sendall(b'<networkaudio><operation type="start" result="0" reason="fixture refuses format"/></networkaudio>\n')
                            continue
                        fragmented(conn, self.reply(kind, dict(op.attrib)))
                        fragmented(conn, bytes(16))
                        if self.stall:
                            self.stall_started.set()
                            self.stop.wait(15)
                            return
                    else:
                        fragmented(conn, self.reply(kind, dict(op.attrib)))
                else:
                    header = first + exact(conn, 31)
                    fields = struct.unpack("<8I", header)
                    length = fields[1] * (1 if dsd else 4) + sum(fields[2:5])
                    if length > 8 * 1024 * 1024:
                        raise AssertionError("fixture received unbounded record")
                    raw = header + exact(conn, length)
                    self.events.append((session, "audio", raw))
                    if fields == (1, 0, 0, 0, 0, 0, 0, 0):
                        continue  # Observed end marker has no synthetic feedback.
                    # Different endpoint feedback and arbitrary nonzero binary fields,
                    # unless a test supplies a raw override (e.g. framing-ambiguity checks).
                    if self.feedback_override is not None:
                        feedback = self.feedback_override
                    else:
                        feedback = struct.pack("<4I", 0, self.rate, 0x0A3E273C, len(raw))
                    self.events.append((session, "feedback", feedback))
                    fragmented(conn, feedback)
        except (EOFError, ConnectionError, OSError):
            pass
        except Exception as exc:
            self.errors.append(repr(exc))
        finally:
            self.events.append((session, "closed", None))
            conn.close()

    def values(self, kind):
        return [value for _, event, value in self.events if event == kind]

    def close(self):
        self.stop.set()
        self.listener.close()
        for conn in self.connections:
            with contextlib.suppress(OSError):
                conn.shutdown(socket.SHUT_RDWR)
            conn.close()
        self.thread.join(1)
        for worker in self.workers:
            worker.join(1)


class FakeHqpControl:
    """Independent fixture for the optional --hqp-control orchestration
    endpoint. tools/hqp_control.py is the reference client and grammar: one
    TCP connection per command (it opens/writes/reads/closes per call), and
    replies are exact XML documents with NO mandatory trailing newline
    (framing is by document structure, not a delimiter). Incoming requests
    are parsed with hqp_control._DocumentReader, the same incremental parser
    the reference client uses for reading responses, so this fixture is not
    a reinvented approximation of the grammar.

    Confirmed commands (control-contract.md): State, Status(subscribe=0)
    (same state/track/position as State), Stop, Play(last=0), Seek(position=
    integer seconds). Only Stop/Play/Seek ever mutate state/position; a bare
    State/Status query never does. Per settled live evidence, Play is issued
    only once the embedded device has already reconnected on its own (not the
    other way around), so Play here always succeeds unconditionally unless
    explicitly refused — there is no retry-gate. `on_play`/`on_seek`, if set,
    fire after a non-refused command commits, letting a test causally notify
    a paired FakeHqpClient instead of coordinating through a sleep.
    """

    def __init__(self, state=2, refuse=(), stall=False, track="", position="0", seekable=True, seek_confirms=True):
        self.state = state
        self.refuse = set(refuse)
        self.stall = stall
        self.track, self.position = track, str(position)
        self.seekable = seekable
        # When False, Seek answers OK but does not actually move position,
        # modeling a no-op acceptance a caller must not mistake for success.
        self.seek_confirms = seek_confirms
        self.on_play = None
        self.on_seek = None
        self.commands = []
        self.hold_next = {}
        self.cancelled_holds = []
        self.ignore_stop = False
        self.lock = threading.Lock()
        self.errors = []
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen()
        self.listener.settimeout(0.1)
        self.port = self.listener.getsockname()[1]
        self.stop_event = threading.Event()
        self.thread = threading.Thread(target=self._accept, daemon=True)
        self.thread.start()

    def _accept(self):
        while not self.stop_event.is_set():
            try:
                conn, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            # Generous and independent of any particular client timeout under
            # test: the real client's own deadline must be what fires, not ours.
            conn.settimeout(30)
            worker = threading.Thread(target=self._session, args=(conn,), daemon=True)
            worker.start()

    def _session(self, conn):
        try:
            if self.stall:
                self.stop_event.wait(30)
                return
            reader = hqp_control._DocumentReader()
            request = None
            while request is None:
                chunk = conn.recv(4096)
                if not chunk:
                    return
                docs = reader.feed(chunk)
                if docs:
                    request = docs[0]
            with self.lock:
                self.commands.append((request.tag, dict(request.attrib)))
                hold = self.hold_next.pop(request.tag, None)
            if hold is not None:
                # This request is an explicit cancellation probe. After the
                # test releases it, prove the client closed the socket instead
                # of treating the expected BrokenPipe from a late send as a
                # fixture implementation failure.
                hold.wait(5)
                conn.settimeout(1)
                try:
                    remaining = conn.recv(1)
                except ConnectionResetError:
                    remaining = b""
                if remaining:
                    self.errors.append("cancelled control request received extra data")
                else:
                    self.cancelled_holds.append(request.tag)
                return
            fragmented(conn, self._reply_for(request.tag, request.attrib))
        except (OSError, ET.ParseError) as exc:
            self.errors.append(repr(exc))
        finally:
            conn.close()

    def _status_attrs(self):
        return f'state="{self.state}" track="{self.track}" position="{self.position}"'

    def _reply_for(self, tag, attrib):
        if tag in self.refuse:
            return f'<Error message="refused {tag}"/>'.encode()
        if tag == "State":
            return f'<State {self._status_attrs()}/>'.encode()
        if tag == "Status":
            return f'<Status {self._status_attrs()}/>'.encode()
        if tag == "Stop":
            if not self.ignore_stop:
                self.state = 0
            self.position = "0"  # observed Embedded Stop resets the source position
            return b'<Stop result="OK"/>'
        if tag == "Play":
            self.state = 2
            if self.on_play is not None:
                self.on_play()
            return b'<Play result="OK"/>'
        if tag == "Seek":
            if not self.seekable:
                return b'<Error message="source is not seekable"/>'
            try:
                requested = str(int(attrib.get("position", "")))
            except ValueError:
                return b'<Error message="Seek requires an integer seconds position"/>'
            if self.seek_confirms:
                self.position = requested
            if self.on_seek is not None:
                self.on_seek()
            return b'<Seek result="OK"/>'
        return f'<Error message="unsupported command {tag}"/>'.encode()

    def values(self, tag):
        with self.lock:
            return [attrib for t, attrib in self.commands if t == tag]

    def tags(self):
        with self.lock:
            return [t for t, _ in self.commands]

    def close(self):
        self.stop_event.set()
        self.listener.close()
        self.thread.join(1)


class FakeHqpClient:
    """Drives the NAA client role HQPlayer/Embedded itself plays, with the
    settled autoreconnect behavior from control-contract.md: whenever its
    session is closed (e.g. a route switch), it reconnects and performs a
    fresh auth/getdevices/initialize/getformats handshake ON ITS OWN —
    independent of any Play command — then waits. It only sends NAA "start"
    (and, if enabled, a real tiny PCM payload) once told to externally via
    `permit_start()`, modeling that a control-plane Play is what causes
    HQPlayer's engine to begin streaming, not the reverse.
    """

    def __init__(self, naa_port, rate=44100, send_audio_after_start=True):
        self.naa_port = naa_port
        self.rate = rate
        self.send_audio_after_start = send_audio_after_start
        self.play_event = threading.Event()
        self.events = []
        self.lock = threading.Lock()
        self.errors = []
        self.stop_event = threading.Event()
        self.generation = 0
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()

    def permit_start(self):
        self.play_event.set()

    def _run(self):
        while not self.stop_event.is_set():
            try:
                self._session()
            except (OSError, ConnectionError, EOFError):
                pass
            except Exception as exc:  # noqa: BLE001 - surfaced via .errors, never hidden
                self.errors.append(repr(exc))
            if self.stop_event.is_set():
                break
            self.stop_event.wait(0.05)

    def _session(self):
        with self.lock:
            self.generation += 1
            gen = self.generation
        conn = socket.create_connection(("127.0.0.1", self.naa_port), TIMEOUT)
        try:
            conn.settimeout(TIMEOUT)
            nonce = f"auto-reconnect-{gen}"
            conn.sendall(f"<authenticate nonce='{nonce}'/>\n".encode())
            line(conn)
            self._record("connected", gen)
            conn.sendall(control("getdevices", direction="output"))
            line(conn)
            conn.sendall(control("initialize", device=VIRTUAL, direction="output", channels=2,
                                 channel_offset=0, pack_sdm=0, periodtime=250, low_delay=0, version_req=5))
            init_reply = operation(line(conn))
            if init_reply.attrib.get("result") != "1":
                self._record("initialize_failed", init_reply.attrib)
                return
            conn.sendall(control("getformats"))
            line(conn)
            self._record("initialized", gen)
            # Wait for an external Play signal without blocking detection of a
            # router-initiated close (a fresh route switch must still reconnect).
            while not self.play_event.is_set() and not self.stop_event.is_set():
                conn.settimeout(0.2)
                try:
                    if conn.recv(1, socket.MSG_PEEK) == b"":
                        return
                except socket.timeout:
                    continue
            if self.stop_event.is_set():
                return
            conn.settimeout(TIMEOUT)
            conn.sendall(control("start", bits=32, channels=2, netbuftime=1, rate=self.rate, stream="pcm"))
            start_reply = operation(line(conn))
            self._record("start", start_reply.attrib)
            if start_reply.attrib.get("result") == "1":
                exact(conn, 16)
                if self.send_audio_after_start:
                    record = audio_record(bytes([0x11]) * 256)
                    conn.sendall(record)
                    exact(conn, 16)
                    self._record("audio_sent", 256)
            self.stop_event.wait(30)
        finally:
            conn.close()
            self.play_event.clear()

    def _record(self, kind, value):
        with self.lock:
            self.events.append((kind, value))

    def values(self, kind):
        with self.lock:
            return [v for k, v in self.events if k == kind]

    def close(self):
        self.stop_event.set()
        self.play_event.set()
        self.thread.join(2)


class Router:
    def __init__(self, hqp_control=None, discovery=False):
        self.temp = tempfile.TemporaryDirectory(prefix="hiphi-router-lab-")
        self.config_path = Path(self.temp.name) / "routes.json"
        self.log = open(Path(self.temp.name) / "router.log", "w+")
        self.hqp_control = hqp_control
        self.discovery = discovery
        self._spawn()

    def _spawn(self):
        # Reserve independent ephemeral loopback ports before starting the binary.
        reservations = [socket.socket(), socket.socket()]
        for sock in reservations:
            sock.bind(("127.0.0.1", 0))
        self.naa_port, self.http_port = [sock.getsockname()[1] for sock in reservations]
        for sock in reservations:
            sock.close()
        args = [BINARY, "--naa-bind", f"127.0.0.1:{self.naa_port}",
                "--control-bind", f"127.0.0.1:{self.http_port}",
                "--name", "HiPhi Router", "--config", str(self.config_path)]
        if self.discovery:
            args += ["--discovery-interface", "127.0.0.1", "--discovery-port", str(self.naa_port)]
        if self.hqp_control:
            args += ["--hqp-control", self.hqp_control]
        self.proc = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT)
        try:
            wait_until(self.ready)
        except Exception:
            self.log.seek(0)
            diagnostic = self.log.read()
            self._stop_process()
            raise AssertionError(f"router did not start: {diagnostic}")

    def restart(self):
        """Terminate the process but keep the same --config file, proving persistence."""
        self._stop_process()
        self.log.seek(0, os.SEEK_END)
        self._spawn()

    def ready(self):
        if self.proc.poll() is not None:
            return False
        try:
            return self.api("GET", "/api/state")[0] == 200
        except (OSError, http.client.HTTPException):
            return False

    def api(self, method, path, body=None):
        conn = http.client.HTTPConnection("127.0.0.1", self.http_port, timeout=TIMEOUT)
        try:
            headers = {"Content-Type": "application/json"} if body is not None else {}
            conn.request(method, path, json.dumps(body) if body is not None else None, headers)
            response = conn.getresponse()
            raw = response.read()
            return response.status, json.loads(raw) if raw else None
        finally:
            conn.close()

    def add(self, fixture, device_id=True):
        body = {"name": fixture.name, "host": "127.0.0.1", "port": fixture.port}
        if device_id:
            body["device_id"] = fixture.device_id
        status, route = self.api("POST", "/api/routes", body)
        assert status in (200, 201), (status, route)
        return route["id"]

    def update(self, route_id, fixture, device_id=True):
        body = {"route_id": route_id, "name": fixture.name, "host": "127.0.0.1", "port": fixture.port}
        body["device_id"] = fixture.device_id if device_id else ""
        return self.api("POST", "/api/routes/update", body)

    def remove(self, route_id):
        return self.api("POST", "/api/routes/remove", {"route_id": route_id})

    def choose(self, route_id):
        status, state = self.api("POST", "/api/select", {"route_id": route_id})
        assert status == 200, (status, state)
        return state

    def connect(self, nonce):
        conn = socket.create_connection(("127.0.0.1", self.naa_port), TIMEOUT)
        conn.settimeout(TIMEOUT)
        request = f"<authenticate nonce='{nonce}' opaque='do not modify &amp; me' />\n".encode()
        fragmented(conn, request)
        reply = line(conn)
        return conn, request, reply

    def raw_http(self, request_bytes, timeout=TIMEOUT):
        """Send exact bytes to the control port for HTTP-boundary tests that need
        headers (Host/Origin/Sec-Fetch-Site/duplicates) the stdlib client won't let us forge."""
        conn = socket.create_connection(("127.0.0.1", self.http_port), timeout)
        conn.settimeout(timeout)
        conn.sendall(request_bytes)
        data = b""
        with contextlib.suppress(socket.timeout, ConnectionError):
            while True:
                chunk = conn.recv(4096)
                if not chunk:
                    break
                data += chunk
        conn.close()
        return data

    def _stop_process(self):
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(2)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(2)

    def close(self):
        self._stop_process()
        self.log.close()
        self.temp.cleanup()


class RouterTests(unittest.TestCase):
    def setUp(self):
        self.fixtures = []
        self.clients = []
        self.controls = []
        self.hqp_clients = []
        self.router = Router()

    def tearDown(self):
        for conn in self.clients:
            conn.close()
        for fixture in self.fixtures:
            fixture.close()
        for client in self.hqp_clients:
            client.close()
        self.router.close()
        for control in self.controls:
            control.close()
        for fixture in self.fixtures:
            self.assertEqual(fixture.errors, [], "fake NAA failed independently")
        for control in self.controls:
            self.assertEqual(control.errors, [], "fake HQP control endpoint failed independently")
        for client in self.hqp_clients:
            self.assertEqual(client.errors, [], "fake HQP NAA-side client failed independently")

    def fixture(self, name="DAC A", device="hw:CARD=A,DEV=0", rate=44100, **kwargs):
        fixture = FakeNaa(name, device, rate, **kwargs)
        self.fixtures.append(fixture)
        return fixture

    def control_fixture(self, **kwargs):
        control = FakeHqpControl(**kwargs)
        self.controls.append(control)
        return control

    def hqp_client(self, **kwargs):
        client = FakeHqpClient(self.router.naa_port, **kwargs)
        self.hqp_clients.append(client)
        return client

    def hqp_error(self, state):
        """Tolerant lookup: the exact --hqp-control state schema is still
        settling (control-contract.md's "Proposed public state" is
        provisional), so check the established top-level field first and a
        plausible nested one, rather than overcoupling to an incidental name."""
        if state.get("last_error"):
            return state["last_error"]
        nested = state.get("hqp_control") or {}
        return nested.get("last_error")

    def with_hqp_control(self, control):
        """Replace the plain router from setUp with one started against this
        control fixture: the flag is only known at process startup."""
        self.router.close()
        self.router = Router(hqp_control=f"127.0.0.1:{control.port}")
        return self.router

    def connect(self, nonce):
        conn, request, reply = self.router.connect(nonce)
        self.clients.append(conn)
        return conn, request, reply

    def initialize(self, conn):
        fragmented(conn, control("getdevices", direction="output"))
        devices = operation(line(conn)).findall("device")
        self.assertEqual(len(devices), 1)
        self.assertEqual(devices[0].attrib["id"], VIRTUAL)
        self.assertEqual(devices[0].attrib["description"], "HiPhi Router")
        fragmented(conn, control("initialize", device=VIRTUAL, direction="output", channels=2,
                                 channel_offset=0, pack_sdm=0, periodtime=250, low_delay=0, version_req=5))
        reply = operation(line(conn))
        self.assertEqual(reply.attrib["device"], VIRTUAL)
        self.assertEqual(reply.attrib["result"], "1")
        conn.sendall(control("getformats"))
        return line(conn)

    def start(self, conn, rate, dsd=False):
        conn.sendall(control("start", bits=1 if dsd else 32, channels=2, netbuftime=1,
                             rate=rate * 64 if dsd else rate, stream="dsd" if dsd else "pcm"))
        reply = operation(line(conn))
        if reply.attrib.get("result") == "1":
            self.assertEqual(exact(conn, 16), bytes(16), "startup binary record must remain exact")
        return reply

    def assert_closed(self, conn):
        conn.settimeout(2)
        try:
            self.assertEqual(conn.recv(1), b"", "old session survived route change")
        except (ConnectionResetError, BrokenPipeError):
            pass

    def test_discovery_during_stream_preserves_session_and_saved_routes(self):
        self.router.close()
        self.router = Router(discovery=True)
        fixture = self.fixture()
        route = self.router.add(fixture)
        self.router.choose(route)
        conn, _, _ = self.connect("scan-while-playing")
        self.initialize(conn)
        self.start(conn, fixture.rate)
        before = self.router.api("GET", "/api/state")[1]
        results = []
        worker = threading.Thread(target=lambda: results.append(self.router.api("POST", "/api/discover", {})))
        worker.start()
        try:
            for _ in range(4):
                conn.sendall(audio_record(bytes(range(256)), False))
                time.sleep(.1)
            during = self.router.api("GET", "/api/state")[1]
            self.assertEqual(during["generation"], before["generation"])
            self.assertEqual(during["session"]["route_id"], before["session"]["route_id"])
            self.assertGreater(during["session"]["audio_bytes"], before["session"]["audio_bytes"])
        finally:
            worker.join(4)
        self.assertFalse(worker.is_alive())
        self.assertEqual(results, [(200, [])])
        self.assertEqual(self.router.api("GET", "/api/routes")[1][0]["id"], route)
        self.assertEqual(len(fixture.values("auth")), 1)

    def test_a_b_a_fresh_auth_identity_formats_pcm_dsd_and_feedback(self):
        fixtures = [self.fixture(), self.fixture("DAC B", 'usb:other&"DAC', 48000)]
        routes = [self.router.add(f) for f in fixtures]
        self.assertIsNone(self.router.api("GET", "/api/state")[1]["selected_route_id"])
        previous = None
        transcript = []
        for index in (0, 1, 0):
            fixture = fixtures[index]
            self.router.choose(routes[index])
            if previous is not None:
                self.assert_closed(previous)
            nonce = f"fresh-{len(transcript)}-{index}"
            conn, auth, reply = self.connect(nonce)
            self.assertEqual(auth, fixture.values("auth")[-1])
            self.assertEqual(reply, fixture.values("auth_reply")[-1])
            self.assertEqual(ET.fromstring(reply).attrib["nonce"], nonce)
            formats = self.initialize(conn)
            self.assertEqual(formats, fixture.values("formats_reply")[-1], "format offer was rewritten or cached")
            self.assertEqual(operation(fixture.values("initialize")[-1]).attrib["device"], fixture.device_id)
            dsd = len(transcript) == 1
            self.assertEqual(self.start(conn, fixture.rate, dsd).attrib["result"], "1")
            payload = bytes(range(256)) * 11 + b'<networkaudio><operation device="hiphi:router"/></networkaudio>\n'
            payload += bytes((-len(payload)) % 8)
            record = audio_record(payload, dsd, metadata=b'<tag device="hiphi:router"/>', picture=b"\x00\xff<device>")
            fragmented(conn, record)
            feedback = exact(conn, 16)
            self.assertEqual(fixture.values("audio")[-1], record, "audio or side section changed")
            self.assertEqual(feedback, fixture.values("feedback")[-1], "feedback changed")
            transcript.append({"destination": fixture.name, "stream": "dsd" if dsd else "pcm", "sha256": hashlib.sha256(record).hexdigest(), "bytes": len(record), "fresh_nonce": nonce})
            previous = conn
        self.assertEqual(len(fixtures[0].values("auth")), 2)
        self.assertEqual(len(fixtures[1].values("auth")), 1)
        if os.environ.get("HIPHI_ROUTER_DEMO"):
            print(json.dumps({"evidence": "loopback software fixtures only", "a_b_a": transcript}, indent=2))

    def _assert_explicit_milestone_success(self, kind):
        for result in (None, "0", "unknown", "2"):
            with self.subTest(kind=kind, result=result):
                fixture = self.fixture(result_overrides={kind: result})
                self.router.choose(self.router.add(fixture))
                conn, _, _ = self.connect(f"invalid-{kind}-{result}")
                if kind == "start":
                    self.initialize(conn)
                    conn.sendall(control("start", bits=32, channels=2, netbuftime=1,
                                         rate=44100, stream="pcm"))
                else:
                    conn.sendall(control("initialize", device=VIRTUAL, direction="output",
                                         channels=2, version_req=5))
                reply = operation(line(conn))
                self.assertEqual(reply.attrib.get("result"), result, "opaque reply result must survive")
                if kind == "start":
                    exact(conn, 16)  # fixture deliberately continues despite its bad reply
                    conn.sendall(audio_record(bytes(64)))
                    exact(conn, 16)
                # A subsequent reply synchronizes past processing the milestone.
                conn.sendall(control("getformats"))
                line(conn)
                session = self.router.api("GET", "/api/state")[1]["session"]
                self.assertIsNotNone(session)
                self.assertFalse(session["initialized" if kind == "initialize" else "started"],
                                 "only result=1 may qualify a session milestone")
                if kind == "start":
                    self.assertGreater(session["audio_bytes"], 0,
                                       "audio itself must not excuse an unaccepted start")
                conn.close()

    def test_new_start_does_not_reuse_previous_accepted_start_or_audio(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("same-session-restart")
        self.initialize(conn)
        self.start(conn, fixture.rate)
        conn.sendall(audio_record(bytes(64)))
        exact(conn, 16)
        before = self.router.api("GET", "/api/state")[1]["session"]
        self.assertTrue(before["started"])
        self.assertGreater(before["audio_bytes"], 0)
        conn.sendall(control("stop"))
        line(conn)
        fixture.result_overrides["start"] = None
        conn.sendall(control("start", bits=32, channels=2, netbuftime=1, rate=44100, stream="pcm"))
        reply = operation(line(conn))
        self.assertNotIn("result", reply.attrib)
        exact(conn, 16)
        conn.sendall(control("getformats"))
        line(conn)
        after = self.router.api("GET", "/api/state")[1]["session"]
        self.assertFalse(after["started"])
        self.assertEqual(after["audio_bytes"], 0)

    def test_initialize_milestone_requires_explicit_success(self):
        self._assert_explicit_milestone_success("initialize")

    def test_start_milestone_requires_explicit_success_even_with_audio(self):
        self._assert_explicit_milestone_success("start")

    def test_no_route_fails_without_connecting_any_fixture(self):
        fixture = self.fixture()
        self.router.add(fixture)
        conn = socket.create_connection(("127.0.0.1", self.router.naa_port), TIMEOUT)
        self.clients.append(conn)
        self.assert_closed(conn)
        self.assertEqual(fixture.values("auth"), [])
        self.assertIsNone(self.router.api("GET", "/api/state")[1]["selected_route_id"])

    def test_unreachable_selected_target_never_falls_back(self):
        good = self.fixture()
        self.router.add(good)
        reserved = socket.socket()
        reserved.bind(("127.0.0.1", 0))
        port = reserved.getsockname()[1]
        reserved.close()
        status, route = self.router.api("POST", "/api/routes", {"name": "offline", "host": "127.0.0.1", "port": port, "device_id": "offline"})
        self.assertIn(status, (200, 201))
        self.router.choose(route["id"])
        conn = socket.create_connection(("127.0.0.1", self.router.naa_port), TIMEOUT)
        self.clients.append(conn)
        with contextlib.suppress(ConnectionError):
            conn.sendall(b"<authenticate nonce='offline'/>\n")
        self.assert_closed(conn)
        self.assertEqual(good.values("auth"), [])
        self.assertEqual(self.router.api("GET", "/api/state")[1]["selected_route_id"], route["id"])

    def test_unknown_route_selection_preserves_existing_route(self):
        fixture = self.fixture()
        route = self.router.add(fixture)
        self.router.choose(route)
        status, body = self.router.api("POST", "/api/select", {"route_id": "does-not-exist"})
        self.assertGreaterEqual(status, 400)
        self.assertIn("error", body)
        self.assertEqual(self.router.api("GET", "/api/state")[1]["selected_route_id"], route)

    def test_stop_cancels_both_sockets(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("stop")
        self.initialize(conn)
        status, _ = self.router.api("POST", "/api/stop", {})
        self.assertEqual(status, 200)
        self.assert_closed(conn)
        wait_until(lambda: fixture.values("closed"))

    def test_coalesced_audio_end_stop_restart_preserves_same_session(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("same-session")
        self.initialize(conn)
        self.start(conn, fixture.rate)
        records = [audio_record(bytes([i]) * 256) for i in range(3)]
        end = struct.pack("<8I", 1, 0, 0, 0, 0, 0, 0, 0)
        conn.sendall(b"".join(records) + end + control("stop"))
        received = exact(conn, 16 * len(records))
        self.assertEqual(received, b"".join(fixture.values("feedback")))
        self.assertEqual(operation(line(conn)).attrib["type"], "stop")
        self.start(conn, fixture.rate, dsd=True)
        dsd = audio_record(b"\x55\xaa" * 128, dsd=True)
        conn.sendall(dsd)
        self.assertEqual(exact(conn, 16), fixture.values("feedback")[-1])
        self.assertEqual(fixture.values("audio"), records + [end, dsd])
        self.assertEqual(len(fixture.values("auth")), 1, "stop/start need not create a second auth")

    def test_downstream_refusal_is_forwarded_without_fallback(self):
        fixture = self.fixture(refuse_start=True)
        unused = self.fixture("unused", "unused", 48000)
        self.router.add(unused)
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("refusal")
        self.initialize(conn)
        reply = self.start(conn, fixture.rate)
        self.assertEqual(reply.attrib["result"], "0")
        self.assertEqual(reply.attrib["reason"], "fixture refuses format")
        self.assertEqual(unused.values("auth"), [])

    def test_single_dac_can_be_learned_without_xml_device_id(self):
        fixture = self.fixture(device='strange:hardware&"identity')
        self.router.choose(self.router.add(fixture, device_id=False))
        conn, _, _ = self.connect("learn-one")
        self.initialize(conn)
        self.assertEqual(operation(fixture.values("initialize")[-1]).attrib["device"], fixture.device_id)

    def test_read_through_dac_catalog_is_per_endpoint_and_replaces_removed_dacs(self):
        a = self.fixture(devices=[("hw:CARD=A,DEV=0", "Main"), ("spare", "Spare")])
        b = self.fixture("Other", "other")
        route_a, route_b = self.router.add(a), self.router.add(b)
        self.router.choose(route_a)
        conn, _, _ = self.connect("catalog-a")
        self.initialize(conn)
        catalog = self.router.api("GET", "/api/state")[1]["dac_catalog"]
        self.assertEqual([d["id"] for d in catalog[0]["devices"]], [a.device_id, "spare"])
        a.devices = [(a.device_id, "Renamed")]
        conn.sendall(control("getdevices", direction="output")); line(conn)
        catalog = self.router.api("GET", "/api/state")[1]["dac_catalog"]
        self.assertEqual(catalog[0]["devices"], [{"id":a.device_id,"description":"Renamed"}])
        self.router.choose(route_b)
        other, _, _ = self.connect("catalog-b")
        self.initialize(other)
        catalog = self.router.api("GET", "/api/state")[1]["dac_catalog"]
        self.assertEqual({entry["port"] for entry in catalog}, {a.port,b.port})
        self.router.api("POST", "/api/stop", {})
        self.assertEqual(self.router.api("GET", "/api/state")[1]["dac_catalog"], catalog)
        self.assertEqual(len(a.values("auth")), 1)
        self.assertEqual(len(b.values("auth")), 1)

    def test_ambiguous_dacs_are_not_arbitrarily_selected(self):
        fixture = self.fixture(devices=[("dac-one", "First"), ("dac-two", "Second")])
        self.router.choose(self.router.add(fixture, device_id=False))
        conn, _, _ = self.connect("ambiguous")
        conn.sendall(control("getdevices", direction="output"))
        conn.settimeout(2)
        try:
            raw = line(conn)
            op = operation(raw)
            self.assertTrue(op.attrib.get("result") == "0" or not op.findall("device"), raw)
        except (EOFError, ConnectionResetError):
            pass
        self.assertEqual(fixture.values("initialize"), [])
        state = self.router.api("GET", "/api/state")[1]
        self.assertTrue(state.get("last_error"), "ambiguous discovery needs actionable error")

    def test_explicit_dac_filters_multi_output_endpoint(self):
        fixture = self.fixture(device="second", devices=[("first", "Unselected"), ("second", "Selected")])
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("explicit-multiple")
        self.initialize(conn)
        self.assertEqual(operation(fixture.values("initialize")[-1]).attrib["device"], "second")

    def test_unknown_virtual_id_is_rejected_before_downstream_initialize(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("unknown-device")
        conn.sendall(control("initialize", device="wrong-device", direction="output"))
        self.assert_closed(conn)
        self.assertEqual(fixture.values("initialize"), [])
        wait_until(lambda: self.router.api("GET", "/api/state")[1].get("last_error"))

    def test_oversized_record_is_rejected_before_unbounded_allocation(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("oversized-record")
        self.initialize(conn)
        self.start(conn, fixture.rate)
        conn.sendall(struct.pack("<8I", 2, 0xFFFFFFFF, 0, 0, 0, 0, 0, 0))
        self.assert_closed(conn)
        self.assertEqual(fixture.values("audio"), [])
        wait_until(lambda: self.router.api("GET", "/api/state")[1].get("last_error"))

    def test_switch_cancels_incomplete_auth_and_reconnects_fresh(self):
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        old = socket.create_connection(("127.0.0.1", self.router.naa_port), TIMEOUT)
        self.clients.append(old)
        old.sendall(b"<authenticate nonce='unfinished")
        wait_until(lambda: a.connections)
        self.router.choose(rb)
        self.assert_closed(old)
        conn, request, reply = self.connect("new-session")
        self.initialize(conn)
        self.assertEqual(b.values("auth"), [request])
        self.assertEqual(ET.fromstring(reply).attrib["endpoint"], "DAC B")
        self.assertEqual(a.values("auth"), [])

    def test_old_generation_cannot_clear_new_session_or_leak_payload(self):
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        routes = [self.router.add(a), self.router.add(b)]
        for i in range(10):
            old_index, new_index = i % 2, (i + 1) % 2
            self.router.choose(routes[old_index])
            old, _, _ = self.connect(f"old-generation-{i}")
            self.initialize(old)
            self.start(old, [a, b][old_index].rate)
            # Leave a partial record in the old generation. It must never become
            # the beginning of the next endpoint's stream or survive as work.
            old.sendall(audio_record(bytes(1024))[:43])
            self.router.choose(routes[new_index])
            fresh, _, _ = self.connect(f"new-generation-{i}")
            self.initialize(fresh)
            self.assert_closed(old)
            state = self.router.api("GET", "/api/state")[1]
            self.assertEqual(state["selected_route_id"], routes[new_index])
            self.assertIsNotNone(state["session"], "old cleanup cleared the new session")
            self.assertEqual(state["session"]["route_id"], routes[new_index])
            self.assertIsNone(state["last_error"], "stale generation published an error")
        self.assertEqual(a.values("audio"), [])
        self.assertEqual(b.values("audio"), [])

    def test_backpressure_does_not_block_selector_or_grow_without_bound(self):
        stalled = self.fixture(stall=True)
        b = self.fixture("DAC B", "b", 48000)
        ra, rb = self.router.add(stalled), self.router.add(b)
        self.router.choose(ra)
        conn, _, _ = self.connect("stalled")
        self.initialize(conn)
        self.start(conn, stalled.rate)
        self.assertTrue(stalled.stall_started.wait(2))
        conn.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 65536)
        conn.settimeout(0.5)
        record = audio_record(bytes(65536))
        written = 0
        blocked = False
        # A black-hole proxy with an unbounded queue will consume this whole budget.
        # Genuine backpressure must show up as a blocked write (socket.timeout): a
        # relay that instead errors/closes the connection under sustained audio
        # would otherwise look indistinguishable from "blocked" and pass wrongly.
        for _ in range(2048):
            try:
                conn.sendall(record)
                written += len(record)
            except socket.timeout:
                blocked = True
                break
            except ConnectionError as exc:
                self.fail(f"router disconnected instead of applying backpressure after {written} bytes: {exc}")
        self.assertTrue(blocked, f"router accepted {written} bytes while downstream was stalled")
        self.assertGreater(written, len(record), "backpressure engaged on the first record; router may not be forwarding at all")
        state = self.router.api("GET", "/api/state")[1]
        self.assertGreater(state["session"]["bytes_to_naa"], len(record), "router reported no forwarding progress before blocking")
        started = time.monotonic()
        self.router.choose(rb)
        self.assertLess(time.monotonic() - started, 2)
        self.assert_closed(conn)
        fresh, _, _ = self.connect("after-stall")
        self.initialize(fresh)
        self.assertEqual(len(b.values("auth")), 1)

    # -- config persistence across restart -----------------------------------

    def test_routes_and_selection_persist_across_restart_and_route_without_reselect(self):
        fixture = self.fixture()
        route_id = self.router.add(fixture)
        self.router.choose(route_id)
        self.router.restart()
        status, routes = self.router.api("GET", "/api/routes")
        self.assertEqual(status, 200)
        self.assertEqual([r["id"] for r in routes], [route_id])
        state = self.router.api("GET", "/api/state")[1]
        self.assertEqual(state["selected_route_id"], route_id)
        # Persisted selection must route HQPlayer with no /api/select call this run.
        conn, _, reply = self.connect("after-restart")
        self.clients.append(conn)
        self.initialize(conn)
        self.assertEqual(ET.fromstring(reply).attrib["endpoint"], fixture.name)

    def test_removed_route_does_not_reappear_after_restart(self):
        fixture = self.fixture()
        route_id = self.router.add(fixture)
        self.router.remove(route_id)
        self.router.restart()
        status, routes = self.router.api("GET", "/api/routes")
        self.assertEqual(status, 200)
        self.assertEqual(routes, [])

    # -- route update/remove ---------------------------------------------------

    def test_update_of_selected_route_disconnects_and_redirects(self):
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        route_id = self.router.add(a)
        self.router.choose(route_id)
        conn, _, _ = self.connect("before-update")
        self.initialize(conn)
        status, updated = self.router.update(route_id, b)
        self.assertEqual(status, 200, updated)
        self.assert_closed(conn)
        fresh, _, reply = self.connect("after-update")
        self.initialize(fresh)
        self.assertEqual(ET.fromstring(reply).attrib["endpoint"], b.name)
        self.assertEqual(a.values("audio"), [])

    def test_update_of_unselected_route_does_not_disturb_active_session(self):
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        conn, _, _ = self.connect("stable")
        self.initialize(conn)
        status, _ = self.router.update(rb, b)
        self.assertEqual(status, 200)
        conn.settimeout(0.3)
        with self.assertRaises(socket.timeout, msg="editing an unselected route disturbed the active session"):
            conn.recv(1)

    def test_update_unknown_route_id_errors_without_side_effects(self):
        fixture = self.fixture()
        route_id = self.router.add(fixture)
        self.router.choose(route_id)
        status, body = self.router.update("does-not-exist", fixture)
        self.assertGreaterEqual(status, 400)
        self.assertIn("error", body)
        self.assertEqual(self.router.api("GET", "/api/state")[1]["selected_route_id"], route_id)

    def test_remove_of_selected_route_clears_selection_and_disconnects(self):
        fixture = self.fixture()
        route_id = self.router.add(fixture)
        self.router.choose(route_id)
        conn, _, _ = self.connect("before-remove")
        self.initialize(conn)
        auth_before = list(fixture.values("auth"))
        status, state = self.router.remove(route_id)
        self.assertEqual(status, 200)
        self.assertIsNone(state["selected_route_id"])
        self.assert_closed(conn)
        blocked = socket.create_connection(("127.0.0.1", self.router.naa_port), TIMEOUT)
        self.clients.append(blocked)
        self.assert_closed(blocked)
        self.assertEqual(fixture.values("auth"), auth_before, "no route selected must never re-contact the fixture")

    def test_remove_unknown_route_id_errors(self):
        status, body = self.router.remove("does-not-exist")
        self.assertGreaterEqual(status, 400)
        self.assertIn("error", body)

    # -- cached initialize without a fresh getdevices ---------------------------

    def test_explicit_device_id_initialize_without_prior_getdevices(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("skip-getdevices-explicit")
        fragmented(conn, control("initialize", device=VIRTUAL, direction="output", channels=2,
                                 channel_offset=0, pack_sdm=0, periodtime=250, low_delay=0, version_req=5))
        reply = operation(line(conn))
        self.assertEqual(reply.attrib["result"], "1", "explicit device_id should not require HQPlayer's own getdevices first")
        self.assertEqual(operation(fixture.values("initialize")[-1]).attrib["device"], fixture.device_id)

    def test_auto_learned_single_device_initialize_without_prior_getdevices(self):
        # The router still performs its own getdevices against the endpoint before
        # HQPlayer ever asks; this proves that internal resolution isn't gated on
        # HQPlayer having sent its own getdevices call in this session.
        fixture = self.fixture(device='auto:learned&"one')
        self.router.choose(self.router.add(fixture, device_id=False))
        conn, _, _ = self.connect("skip-getdevices-auto")
        fragmented(conn, control("initialize", device=VIRTUAL, direction="output", channels=2,
                                 channel_offset=0, pack_sdm=0, periodtime=250, low_delay=0, version_req=5))
        reply = operation(line(conn))
        self.assertEqual(reply.attrib["result"], "1", "auto-learned single device should resolve without HQPlayer's own getdevices")
        self.assertEqual(operation(fixture.values("initialize")[-1]).attrib["device"], fixture.device_id)

    # -- malformed / truncated / coalesced control framing -----------------------

    def test_truncated_control_without_newline_is_rejected(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn = socket.create_connection(("127.0.0.1", self.router.naa_port), TIMEOUT)
        self.clients.append(conn)
        conn.sendall(b"<authenticate nonce='trunc'")
        with contextlib.suppress(OSError):
            conn.shutdown(socket.SHUT_WR)
        self.assert_closed(conn)
        self.assertEqual(fixture.values("auth"), [])

    def test_control_line_over_limit_is_rejected(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("boundary")
        self.initialize(conn)
        padding = "x" * 70000
        oversized = f'<networkaudio><operation type="keepalive" pad="{padding}"/></networkaudio>\n'.encode()
        conn.sendall(oversized)
        self.assert_closed(conn)
        wait_until(lambda: self.router.api("GET", "/api/state")[1].get("last_error"))

    def test_control_with_doctype_declaration_is_rejected(self):
        # The wire classifier only inspects the first 16 bytes (protocol.rs
        # PROBE/is_control), so the "<!" doctype/entity guard must be exercised
        # with a message that starts with a real control prefix.
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("doctype")
        conn.sendall(b'<networkaudio><!DOCTYPE foo [<!ENTITY x "y">]>'
                     b'<operation type="getdevices" direction="output"/></networkaudio>\n')
        self.assert_closed(conn)
        self.assertEqual(fixture.values("getdevices"), [])

    def test_control_missing_operation_element_is_rejected(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("missing-op")
        conn.sendall(b"<networkaudio></networkaudio>\n")
        self.assert_closed(conn)

    def test_control_with_extra_top_level_element_is_rejected(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("extra-op")
        conn.sendall(b'<networkaudio><operation type="getdevices" direction="output"/>'
                     b'<operation type="getdevices" direction="output"/></networkaudio>\n')
        self.assert_closed(conn)

    def test_control_with_invalid_utf8_is_rejected(self):
        # Must be >=16 bytes and start with a recognized control prefix so the
        # wire classifier (protocol.rs PROBE) routes it to the XML/UTF-8 path
        # instead of blocking as an under-length binary record.
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("bad-utf8")
        conn.sendall(b'<networkaudio><operation type="x" a="\xff\xfe"/></networkaudio>\n')
        self.assert_closed(conn)

    def test_coalesced_pipelined_operations_processed_in_order(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("coalesced")
        getdevices = control("getdevices", direction="output")
        initialize = control("initialize", device=VIRTUAL, direction="output", channels=2,
                              channel_offset=0, pack_sdm=0, periodtime=250, low_delay=0, version_req=5)
        getformats = control("getformats")
        # A single write with no fragmentation: the reader must still split on \n.
        conn.sendall(getdevices + initialize + getformats)
        devices = operation(line(conn)).findall("device")
        self.assertEqual(devices[0].attrib["id"], VIRTUAL)
        init_reply = operation(line(conn))
        self.assertEqual(init_reply.attrib["result"], "1")
        formats = line(conn)
        self.assertEqual(formats, fixture.values("formats_reply")[-1])

    # -- stop --------------------------------------------------------------------

    def test_stop_clears_selection_and_refuses_new_connections(self):
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("before-stop")
        self.initialize(conn)
        auth_before = list(fixture.values("auth"))
        status, state = self.router.api("POST", "/api/stop", {})
        self.assertEqual(status, 200)
        self.assertIsNone(state["selected_route_id"])
        self.assert_closed(conn)
        blocked = socket.create_connection(("127.0.0.1", self.router.naa_port), TIMEOUT)
        self.clients.append(blocked)
        self.assert_closed(blocked)
        self.assertEqual(fixture.values("auth"), auth_before, "no route selected must never re-contact the fixture")

    # -- HTTP control-plane boundary (CSRF-relevant headers) ----------------------

    def test_http_rejects_mismatched_host_header(self):
        raw = raw_request("GET", "/api/state", "evil.example:1")
        response = self.router.raw_http(raw)
        self.assertEqual(status_of(response), "HTTP/1.1 403 Forbidden", response)

    def test_http_rejects_foreign_origin(self):
        host = f"127.0.0.1:{self.router.http_port}"
        raw = raw_request("GET", "/api/state", host, extra_headers=[("Origin", "http://evil.example")])
        response = self.router.raw_http(raw)
        self.assertEqual(status_of(response), "HTTP/1.1 403 Forbidden", response)

    def test_http_accepts_matching_origin(self):
        host = f"127.0.0.1:{self.router.http_port}"
        raw = raw_request("GET", "/api/state", host, extra_headers=[("Origin", f"http://{host}")])
        response = self.router.raw_http(raw)
        self.assertEqual(status_of(response), "HTTP/1.1 200 OK", response)

    def test_http_rejects_cross_site_fetch_metadata(self):
        host = f"127.0.0.1:{self.router.http_port}"
        raw = raw_request("GET", "/api/state", host, extra_headers=[("Sec-Fetch-Site", "cross-site")])
        response = self.router.raw_http(raw)
        self.assertEqual(status_of(response), "HTTP/1.1 403 Forbidden", response)

    def test_http_rejects_duplicate_headers(self):
        host = f"127.0.0.1:{self.router.http_port}"
        raw = (f"GET /api/state HTTP/1.1\r\nHost: {host}\r\nX-Dup: 1\r\nX-Dup: 2\r\n\r\n").encode()
        response = self.router.raw_http(raw)
        self.assertEqual(status_of(response), "HTTP/1.1 400 Bad Request", response)

    def test_http_rejects_chunked_transfer_encoding(self):
        host = f"127.0.0.1:{self.router.http_port}"
        body = b'{"name":"x","host":"127.0.0.1","port":1}'
        raw = raw_request("POST", "/api/routes", host,
                           extra_headers=[("Transfer-Encoding", "chunked"), ("Content-Type", "application/json")],
                           body=body)
        response = self.router.raw_http(raw)
        self.assertEqual(status_of(response), "HTTP/1.1 400 Bad Request", response)
        self.assertEqual(self.router.api("GET", "/api/routes")[1], [], "chunked request must not have created a route")

    # -- reverse-direction framing (first-byte '<' ambiguity, now resolved) -------

    def test_downstream_feedback_starting_with_lt_byte_is_forwarded_as_binary(self):
        # protocol.rs classifies a record by its full 16-byte PROBE against known
        # control roots (<networkaudio, <authenticate, <?xml), not by a bare first
        # byte, specifically so a 16-byte feedback record that happens to start
        # with '<' is still treated as binary. This is exercised at the unit level
        # in protocol.rs::tests::binary_record_starting_with_angle_bracket_is_not_control;
        # this end-to-end check confirms the same property holds across a full
        # session, byte-exact, not just for the pure classifier function.
        feedback = bytes([0x3C]) + bytes(15)
        fixture = self.fixture(feedback_override=feedback)
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("feedback-angle-bracket")
        self.initialize(conn)
        self.start(conn, fixture.rate)
        conn.sendall(audio_record(bytes(256)))
        received = exact(conn, 16)
        self.assertEqual(received, feedback, "feedback beginning with '<' must forward byte-exact as binary")
        self.assertIsNone(self.router.api("GET", "/api/state")[1].get("last_error"))

    def test_namespaced_device_attribute_is_left_unmodified(self):
        # roxmltree's Attribute::name() returns the local name regardless of
        # namespace, so a namespaced "x:device" attribute shares the local name
        # "device" with the real one. protocol.rs filters rewrites through
        # plain(a) (a.namespace().is_none()) to avoid rewriting it by mistake;
        # confirmed empirically here, not assumed from reading the guard alone.
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("namespaced-device")
        msg = ('<networkaudio><operation type="initialize" xmlns:x="urn:test" '
               'x:device="untouched-namespaced-value" device="{}" direction="output" '
               'channels="2" channel_offset="0" pack_sdm="0" periodtime="250" '
               'low_delay="0" version_req="5"/></networkaudio>\n').format(VIRTUAL).encode()
        conn.sendall(msg)
        reply = operation(line(conn))
        self.assertEqual(reply.attrib["result"], "1")
        forwarded = operation(fixture.values("initialize")[-1])
        self.assertEqual(forwarded.attrib["{urn:test}device"], "untouched-namespaced-value")
        self.assertEqual(forwarded.attrib["device"], fixture.device_id)

    def test_mid_session_reauthentication_is_relayed_opaquely(self):
        # protocol.rs now passes a later <authenticate> straight through in both
        # directions without touching operation/device state, instead of treating
        # it as a malformed operation. Never previously exercised.
        fixture = self.fixture()
        self.router.choose(self.router.add(fixture))
        conn, _, _ = self.connect("initial")
        self.initialize(conn)
        self.start(conn, fixture.rate)
        reauth = b"<authenticate nonce='renew-1' opaque='second handshake &amp; opaque'/>\n"
        fragmented(conn, reauth)
        reply = line(conn)
        self.assertEqual(reauth, fixture.values("reauth")[-1])
        self.assertEqual(reply, fixture.values("reauth_reply")[-1])
        self.assertEqual(ET.fromstring(reply).attrib["nonce"], "renew-1")
        self.assertEqual(len(fixture.values("initialize")), 1, "reauth must not re-trigger enumeration/initialize")
        self.assertIsNone(self.router.api("GET", "/api/state")[1].get("last_error"))

    # -- optional --hqp-control orchestration (native HQPlayer one-click switch) --
    # See /tmp/hiphi-sonnet-work/control-contract.md for the settled live
    # evidence and constraints these encode. --hqp-control is a required
    # feature by this point (Fable has landed it), so these do not skip: a
    # missing/broken flag must fail the gate, not vanish quietly.
    #
    # Settled sequence for an originally-playing switch: capture State ->
    # explicit Stop -> commit route (old NAA sockets close) -> the real
    # HQPlayer/Embedded reconnects and initializes ON ITS OWN, not triggered
    # by Play -> Play(last=0) once -> verify State 2 AND real audio payload,
    # not just control-plane chatter. FakeHqpClient plays that autoreconnect
    # role; FakeHqpControl is the separate native command endpoint.

    def _assert_no_autoplay(self, original_state):
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        control = self.control_fixture(state=original_state)
        self.with_hqp_control(control)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        conn, _, _ = self.connect("initial")
        self.initialize(conn)
        self.router.choose(rb)
        self.assert_closed(conn)
        fresh, _, _ = self.connect("after-switch")
        self.initialize(fresh)
        used = set(control.tags())
        self.assertNotIn("Play", used, "must never autoplay when originally stopped/paused")
        self.assertNotIn("Seek", used, "nothing to restore when originally stopped/paused")
        self.assertEqual(control.state, 0, "inactive switching must leave transport stopped")
        if original_state == 1:
            self.assertIn("Stop", used, "paused transport must stop before its NAA is disconnected")

    def test_no_autoplay_when_originally_stopped(self):
        self._assert_no_autoplay(0)

    def test_no_autoplay_when_originally_paused(self):
        self._assert_no_autoplay(1)

    def test_one_click_stop_select_play_sequence_delivers_real_audio(self):
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        # A live fractional position: the restore must floor() it, not just
        # echo it back or fabricate an integer.
        control = self.control_fixture(state=0, track="album/track-3", position="47.56")
        self.with_hqp_control(control)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        client = self.hqp_client()
        wait_until(lambda: client.values("initialized"), timeout=10)
        control.state = 2
        control.on_play = client.permit_start
        status, result = self.router.api("POST", "/api/select", {"route_id": rb})
        self.assertEqual(status, 200)
        self.assertEqual(result["selected_route_id"], rb)
        wait_until(lambda: b.values("auth"), timeout=15)
        wait_until(lambda: client.values("audio_sent"), timeout=15)
        # /api/select's 200 only means the mutation was accepted; resumption
        # is asynchronous, so the terminal outcome must be polled separately.
        in_progress = {None, "checking", "stopping", "waiting_for_naa", "resuming"}

        def terminal_hqp_control():
            control_state = self.router.api("GET", "/api/state")[1].get("hqp_control") or {}
            return control_state if control_state.get("phase") not in in_progress else None

        terminal = wait_until(terminal_hqp_control, timeout=15)
        self.assertNotEqual(terminal.get("phase"), "error", terminal)
        self.assertIs(terminal.get("position_restored"), True, terminal)
        self.assertIsNone(self.router.api("GET", "/api/state")[1].get("last_error"))
        wait_until(lambda: "Seek" in control.tags(), timeout=10)
        tags = control.tags()
        self.assertIn("Stop", tags)
        self.assertIn("Play", tags)
        self.assertLess(tags.index("Stop"), tags.index("Play"), "Stop must precede Play in the one-click sequence")
        self.assertTrue(set(tags) <= {"State", "Status", "Stop", "Play", "Seek"},
                        f"no profile/restart commands allowed: {tags}")
        self.assertLess(tags.index("Play"), tags.index("Seek"), "position restore must follow Play, not precede it")
        seeks = control.values("Seek")
        self.assertEqual(seeks[-1].get("position"), "47", "must restore the floored captured position (47.56 -> 47)")
        self.assertEqual(control.state, 2)
        # The NAA reconnect happened on its own, not because Play told it to:
        # a second initialize (for B) already completed before Play was sent.
        self.assertGreaterEqual(len(client.values("initialized")), 2, "must reconnect for both A and the switch to B")

    def test_unreachable_control_fails_select_before_mutation(self):
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        control = self.control_fixture(state=0)  # reachable for the initial bootstrap only
        self.with_hqp_control(control)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        conn, _, _ = self.connect("initial")
        self.initialize(conn)
        control.close()  # unreachable for the switch attempt
        status, body = self.router.api("POST", "/api/select", {"route_id": rb})
        self.assertGreaterEqual(status, 400)
        self.assertIn("error", body)
        self.assertEqual(self.router.api("GET", "/api/state")[1]["selected_route_id"], ra,
                         "unreachable control must fail before any route mutation")
        self.assertEqual(b.values("auth"), [], "target must never be contacted when the select itself failed")
        conn.settimeout(0.3)
        with self.assertRaises(socket.timeout, msg="the untouched original session must still be alive"):
            conn.recv(1)

    def test_stop_cancels_stalled_select_without_late_route_commit_or_play(self):
        a, b = self.fixture(), self.fixture("DAC B", "different-b", 48000)
        control = self.control_fixture(state=2)
        self.with_hqp_control(control)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        conn, _, _ = self.connect("before-stalled-select")
        self.initialize(conn)
        release = threading.Event()
        control.hold_next["State"] = release
        result = []
        worker = threading.Thread(target=lambda: result.append(self.router.api("POST", "/api/select", {"route_id": rb})))
        worker.start()
        try:
            wait_until(lambda: "State" in control.tags())
            started = time.monotonic()
            status, stopped = self.router.api("POST", "/api/stop", {})
            self.assertEqual(status, 200)
            self.assertLess(time.monotonic() - started, 2, "Stop must cancel stalled preflight immediately")
            self.assertIsNone(stopped["selected_route_id"])
            self.assertIsNone(stopped["session"])
            self.assert_closed(conn)
            worker.join(2)
            self.assertFalse(worker.is_alive(), "cancelled control socket must release selection worker")
            self.assertGreaterEqual(result[0][0], 400)
        finally:
            release.set()
            worker.join(2)
        wait_until(lambda: "State" in control.cancelled_holds, timeout=2)
        self.assertEqual(b.values("auth"), [])
        self.assertNotIn("Play", control.tags())
        self.assertIsNone(self.router.api("GET", "/api/state")[1]["selected_route_id"])

    def test_stop_ack_without_stopped_state_is_reported_as_partial_failure(self):
        a = self.fixture()
        control = self.control_fixture(state=2)
        self.with_hqp_control(control)
        self.router.choose(self.router.add(a))
        conn, _, _ = self.connect("ignored-native-stop")
        self.initialize(conn)
        control.ignore_stop = True
        status, stopped = self.router.api("POST", "/api/stop", {})
        self.assertEqual(status, 200, "local routing halt succeeds even when native Stop is ignored")
        self.assertIsNone(stopped["selected_route_id"])
        self.assertIsNone(stopped["session"])
        self.assert_closed(conn)
        self.assertEqual(stopped["hqp_control"]["phase"], "error")
        self.assertNotEqual(stopped["hqp_control"]["transport_state"], "0")
        self.assertIn("state=2", self.hqp_error(stopped))

    def test_unknown_native_state_refuses_selection_before_mutation(self):
        # Concrete known gap per control-contract.md: an unrecognized State
        # string must refuse the select, not silently fall through as if
        # stopped. Fable owns restricting preflight to '0'/'1'/'2'.
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        control = self.control_fixture(state=0)
        self.with_hqp_control(control)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        conn, _, _ = self.connect("initial")
        self.initialize(conn)
        control.state = "bogus"
        status, body = self.router.api("POST", "/api/select", {"route_id": rb})
        self.assertGreaterEqual(status, 400, "an unrecognized native State must refuse before mutation")
        self.assertIn("error", body)
        self.assertEqual(self.router.api("GET", "/api/state")[1]["selected_route_id"], ra)
        self.assertEqual(b.values("auth"), [])

    def test_target_failure_no_silent_fallback_when_play_refused(self):
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        control = self.control_fixture(state=0)
        self.with_hqp_control(control)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        client = self.hqp_client()
        wait_until(lambda: client.values("initialized"), timeout=10)
        control.state = 2
        control.refuse = {"Play"}
        status, result = self.router.api("POST", "/api/select", {"route_id": rb})
        self.assertEqual(status, 200, "the NAA route mutation itself is independent of native transport orchestration")
        self.assertEqual(result["selected_route_id"], rb, "must commit to the newly selected route, not fall back")
        wait_until(lambda: b.values("auth"), timeout=15)
        wait_until(lambda: self.hqp_error(self.router.api("GET", "/api/state")[1]), timeout=15)
        self.assertGreaterEqual(len(control.values("Play")), 1, "must have attempted Play, not silently given up early")
        self.assertEqual(a.values("audio"), [], "no fallback to the previous target")
        self.assertEqual(client.values("audio_sent"), [], "refused Play must never be reported as resumed playback")

    def test_control_ok_and_state_2_without_real_audio_is_not_resumed_success(self):
        # Named false positive from control-contract.md: Play OK and State 2
        # with initialize/start ACKed but ZERO actual audio payload must not
        # count as resumed. session.bytes_to_naa/state=forwarding alone (auth,
        # a header, or an end marker) are not proof; only real payload is.
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        control = self.control_fixture(state=0)
        self.with_hqp_control(control)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        client = self.hqp_client(send_audio_after_start=False)
        wait_until(lambda: client.values("initialized"), timeout=10)
        control.state = 2
        control.on_play = client.permit_start
        status, result = self.router.api("POST", "/api/select", {"route_id": rb})
        self.assertEqual(status, 200)
        wait_until(lambda: client.values("start"), timeout=15)
        self.assertEqual(client.values("start")[-1].get("result"), "1", "start itself was accepted")
        self.assertEqual(client.values("audio_sent"), [], "the fixture is deliberately withholding all audio")
        # Play was answered OK and reached State 2 (both observed above via
        # control.tags()/on_play), which is exactly the named false positive.
        # A router that stops there would wrongly report success; it must
        # keep waiting for real payload and surface a visible error instead.
        error = wait_until(lambda: self.hqp_error(self.router.api("GET", "/api/state")[1]), timeout=15)
        self.assertIn("audio", error.lower(), f"error should explain the missing-audio false positive: {error!r}")
        self.assertIn("Play", control.tags(), "Play OK alone must not have been treated as sufficient")

    def test_seek_ok_without_confirmed_position_is_not_reported_restored(self):
        # Concrete gap per control-contract.md: "Seek OK followed by same-track
        # Status.position still 0 MUST NOT set position_restored=true."
        # Fable is implementing post-Seek same-track/position confirmation;
        # until landed this stays red on purpose, not weakened to pass.
        a = self.fixture()
        b = self.fixture("DAC B", "different-b", 48000)
        control = self.control_fixture(state=0, track="album/track-3", position="47.56", seek_confirms=False)
        self.with_hqp_control(control)
        ra, rb = self.router.add(a), self.router.add(b)
        self.router.choose(ra)
        client = self.hqp_client()
        wait_until(lambda: client.values("initialized"), timeout=10)
        control.state = 2
        control.on_play = client.permit_start
        status, result = self.router.api("POST", "/api/select", {"route_id": rb})
        self.assertEqual(status, 200)
        wait_until(lambda: client.values("audio_sent"), timeout=15)
        wait_until(lambda: "Seek" in control.tags(), timeout=10)

        def terminal():
            state = self.router.api("GET", "/api/state")[1].get("hqp_control") or {}
            return state if state.get("phase") not in (None, "checking", "stopping", "waiting_for_naa", "resuming") else None

        final = wait_until(terminal, timeout=15)
        self.assertEqual(control.position, "0", "fixture must model an ignored Seek after Stop reset")
        self.assertIs(final.get("position_restored"), False,
                      "Seek OK without a confirmed position match must not claim position was restored")
        self.assertIn("position", final.get("last_error", "").lower())
        self.assertGreaterEqual(len(client.values("audio_sent")), 1, "the new route must keep playing regardless")


class HqpControlFixtureTests(unittest.TestCase):
    """Validates FakeHqpControl itself against the reference hqp_control.py
    client, independent of naa-router. The RouterTests.*hqp_control* tests
    above depend on this fixture behaving correctly; these are what make
    that dependable."""

    def setUp(self):
        self.controls = []

    def tearDown(self):
        for control in self.controls:
            control.close()
        for control in self.controls:
            self.assertEqual(control.errors, [], "fake HQP control endpoint failed independently")

    def control(self, **kwargs):
        control = FakeHqpControl(**kwargs)
        self.controls.append(control)
        return control

    def test_state_stop_play_round_trip_and_command_recording(self):
        control = self.control(state=2)
        client = hqp_control.HQPlayerControl(port=control.port, timeout=5)
        self.assertEqual(client.state().attrib["state"], "2")
        self.assertEqual(client.stop().attrib["result"], "OK")
        self.assertEqual(client.state().attrib["state"], "0")
        self.assertEqual(client.play().attrib["result"], "OK")
        self.assertEqual(client.state().attrib["state"], "2")
        self.assertEqual(control.tags(), ["State", "Stop", "State", "Play", "State"])
        self.assertEqual(control.values("Play"), [{"last": "0"}])

    def test_reply_has_no_trailing_newline(self):
        control = self.control(state=2)
        conn = socket.create_connection(("127.0.0.1", control.port), TIMEOUT)
        try:
            conn.sendall(b'<?xml version="1.0"?><State/>')
            conn.settimeout(TIMEOUT)
            data = exact(conn, len(b'<State state="2" track="" position="0"/>'))
            self.assertFalse(data.endswith(b"\n"), f"reply must not depend on a trailing newline: {data!r}")
            self.assertTrue(data.endswith(b"/>"), data)
        finally:
            conn.close()

    def test_refused_command_reports_error_to_the_reference_client(self):
        control = self.control(state=2, refuse={"Play"})
        client = hqp_control.HQPlayerControl(port=control.port, timeout=5)
        with self.assertRaises(hqp_control.HQPlayerError):
            client.play()
        self.assertEqual(control.tags(), ["Play"])

    def test_status_reports_state_track_and_position(self):
        control = self.control(state=1, track="album/track-3", position=17)
        client = hqp_control.HQPlayerControl(port=control.port, timeout=5)
        status = client.status()
        self.assertEqual(status.attrib["state"], "1")
        self.assertEqual(status.attrib["track"], "album/track-3")
        self.assertEqual(status.attrib["position"], "17")
        self.assertEqual(control.values("Status"), [{"subscribe": "0"}])

    def test_seek_sets_integer_position_and_notifies(self):
        seeked = threading.Event()
        control = self.control(state=2, position="10")
        control.on_seek = seeked.set
        client = hqp_control.HQPlayerControl(port=control.port, timeout=5)
        reply = client.request("Seek", position=45)
        self.assertEqual(reply.attrib["result"], "OK")
        self.assertEqual(client.state().attrib["position"], "45")
        self.assertTrue(seeked.is_set())

    def test_seek_on_unseekable_source_reports_error_not_fabricated_support(self):
        control = self.control(state=2, position="10", seekable=False)
        client = hqp_control.HQPlayerControl(port=control.port, timeout=5)
        with self.assertRaises(hqp_control.HQPlayerError):
            client.request("Seek", position=45)
        self.assertEqual(client.state().attrib["position"], "10", "an unseekable source's position must be unchanged")

    def test_unreachable_control_port_refuses_connection(self):
        client = hqp_control.HQPlayerControl(port=reserved_port(), timeout=1)
        with self.assertRaises(hqp_control.HQPlayerError):
            client.state()

    def test_stalled_control_connection_times_out_not_hangs(self):
        control = self.control(state=2, stall=True)
        client = hqp_control.HQPlayerControl(port=control.port, timeout=1)
        started = time.monotonic()
        with self.assertRaises(hqp_control.HQPlayerError):
            client.state()
        self.assertLess(time.monotonic() - started, 3)

    def test_unsupported_command_is_reported_as_an_error(self):
        # No profile/restart commands are ever expected from the router, but
        # the fixture must still answer deterministically if one arrived.
        control = self.control(state=2)
        client = hqp_control.HQPlayerControl(port=control.port, timeout=5)
        with self.assertRaises(hqp_control.HQPlayerError):
            client.pause()


def main():
    global BINARY
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--demo", action="store_true")
    parser.add_argument("--test", help="run one named unittest method")
    args = parser.parse_args()
    BINARY = str(args.binary.resolve())
    if args.demo:
        os.environ["HIPHI_ROUTER_DEMO"] = "1"
        args.test = "test_a_b_a_fresh_auth_identity_formats_pcm_dsd_and_feedback"
    loader = unittest.TestLoader()
    classes = (RouterTests, HqpControlFixtureTests)
    if args.test:
        target = next((c for c in classes if hasattr(c, args.test)), RouterTests)
        suite = loader.loadTestsFromName(args.test, target)
    else:
        suite = unittest.TestSuite(loader.loadTestsFromTestCase(c) for c in classes)
    result = unittest.TextTestRunner(verbosity=2).run(suite)
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    raise SystemExit(main())
