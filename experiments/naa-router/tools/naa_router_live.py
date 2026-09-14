#!/usr/bin/env python3
"""Two loopback Rust NAA endpoints backed only by bounded recording sinks.

Uses the production Rust NAA engine and existing simulated-clock sink. Fresh
opaque authentication is delegated to an explicitly supplied official helper.
No physical output, discovery broadcasts, HQPlayer settings or playback control.
Providers are explicit; compare actual signed exchanges rather than inferring
identity from names, ports or processes.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import select
import signal
import socket
import struct
import subprocess
import threading
import time
import wave

from naa_clock_sink import ClockSink

ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / 'native/naa-native/target/debug/naa-android'
DEVICE = 'hw:CARD=ANDROIDNAA,DEV=0'


def frame(kind, generation, payload=b''):
    return struct.pack('<HHQI', 2, kind, generation, len(payload)) + payload


class Fixture:
    def __init__(self, name, port, auth_port, directory, stopped, rates=(44100,48000)):
        self.name, self.port, self.directory, self.stopped = name, port, directory, stopped
        directory.mkdir(mode=0o700)
        self.ipc = socket.socket(socket.AF_UNIX)
        self.ipc_path = directory / 'sink.sock'
        self.ipc.bind(str(self.ipc_path)); self.ipc.listen(1); self.ipc.settimeout(5)
        self.log = (directory/'rust.log').open('xb')
        self.events = (directory/'events.jsonl').open('x', buffering=1)
        env = dict(os.environ, HIPHI_NAA_BIND='127.0.0.1', HIPHI_NAA_PORT=str(port),
                   HIPHI_NAA_AUTH_PORT=str(auth_port), HIPHI_NAA_IPC_SOCKET=str(self.ipc_path),
                   HIPHI_NAA_FORMATS=','.join(str(rate)+':32:32:2:1:0' for rate in rates), HIPHI_NAA_DEVICE_NAME='HiPhi Router recording '+name,
                   HIPHI_NAA_DISCOVERY='0')
        self.proc = subprocess.Popen([str(BINARY)], env=env, stdout=self.log, stderr=self.log)
        self.peer, _ = self.ipc.accept(); self.peer.settimeout(2)
        self.sink = ClockSink(capacity_frames=192000, capture_dir=directory)
        self.generation = None; self.pending_stop = False; self.buffer = bytearray()
        self.error = None
        self.thread = threading.Thread(target=self.run, name='recording-'+name, daemon=True)
        self.thread.start()

    def event(self, kind, **values):
        self.events.write(json.dumps(dict(event=kind, time_ns=time.monotonic_ns(), **values))+'\n')

    def send(self, kind, payload=b''):
        self.peer.sendall(frame(kind, self.generation, payload))

    def record(self, kind, generation, payload):
        if kind == 1:
            if self.generation is not None or len(payload) != 36:
                raise ValueError('FORMAT_REQUEST_STATE')
            rate, channels, valid_bits, bits, format_kind = struct.unpack_from('<IHHHH', payload)
            if format_kind != 1 or bits != 32 or valid_bits != 32 or channels != 2 or rate not in (44100,48000):
                self.peer.sendall(frame(7, generation, b'RECORDING_FORMAT_UNSUPPORTED')); return
            self.generation = generation; self.sink_generation = self.sink.start(rate,channels,bits)
            self.pending_stop = False
            self.event('start', generation=generation, sink_generation=self.sink_generation, rate=rate)
            self.send(6,payload)
        elif generation != self.generation:
            raise ValueError('STALE_GENERATION')
        elif kind == 2:
            self.sink.write(payload,self.sink_generation)
            status = self.sink.status()
            self.send(9,struct.pack('<Q',status['accepted_frames']))
        elif kind == 3:
            self.sink.stop(drain=True); self.pending_stop = True
            self.event('drain_requested',generation=generation)
        elif kind == 4:
            status=self.sink.status();self.sink.stop(drain=False)
            self.event('abort',wire_generation=generation,**status)
            self.send(8,struct.pack('<QQ',status['accepted_frames'],status['rendered_frames']))
            self.generation=None;self.pending_stop=False
        elif kind != 10:
            raise ValueError('UNEXPECTED_IPC_KIND_'+str(kind))

    def run(self):
        try:
            while not self.stopped.is_set():
                ready,_,_=select.select([self.peer],[],[],.02)
                if ready:
                    data=self.peer.recv(65536)
                    if not data: raise EOFError('Rust IPC closed')
                    self.buffer.extend(data)
                    while len(self.buffer)>=16:
                        version,kind,generation,length=struct.unpack_from('<HHQI',self.buffer)
                        if version != 2 or length > 1048576:raise ValueError('IPC_HEADER')
                        if len(self.buffer)<16+length:break
                        payload=bytes(self.buffer[16:16+length]);del self.buffer[:16+length]
                        self.record(kind,generation,payload)
                if self.generation is not None:
                    self.sink.advance();status=self.sink.status()
                    self.send(5,struct.pack('<QQ',status['rendered_frames'],time.monotonic_ns()))
                    if self.pending_stop and status['state']=='closed':
                        if status['accepted_frames']!=status['rendered_frames']:raise ValueError('DRAIN_INCOMPLETE')
                        self.send(8,struct.pack('<QQ',status['accepted_frames'],status['rendered_frames']))
                        self.event('drained',wire_generation=self.generation,**status)
                        self.generation=None;self.pending_stop=False
        except Exception as exc:
            if not self.stopped.is_set():
                self.error=str(exc);self.event('error',error=self.error);self.stopped.set()
        finally:
            self.sink.stop(drain=False)

    def close(self):
        self.thread.join(timeout=3)
        self.peer.close();self.ipc.close()
        self.proc.terminate()
        try:self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:self.proc.kill();self.proc.wait()
        self.log.close();self.events.close()
        self.ipc_path.unlink(missing_ok=True)


def compare(directory):
    results=[]
    for path in sorted(directory.glob('*/sink-*/accepted.pcm')):
        with wave.open(str(path.parent/'rendered.wav'),'rb') as wav:
            rendered=wav.readframes(wav.getnframes())
        accepted=path.read_bytes()
        results.append(dict(sink=str(path.parent.relative_to(directory)),bytes=len(accepted),
                            sha256=hashlib.sha256(accepted).hexdigest(),
                            accepted_equals_rendered=bool(accepted) and accepted==rendered))
    return results


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--auth-port',required=True,type=int)
    parser.add_argument('--auth-port-b',type=int,help='optional different official helper for fixture B')
    parser.add_argument('--port-a',type=int,default=49311)
    parser.add_argument('--port-b',type=int,default=49312)
    parser.add_argument('--directory',type=Path,required=True)
    parser.add_argument('--seconds',type=int,default=1800)
    parser.add_argument('--rates-a',default='44100,48000')
    parser.add_argument('--rates-b',default='44100,48000')
    args=parser.parse_args()
    try:
        rates_a=tuple(int(r) for r in args.rates_a.split(','));rates_b=tuple(int(r) for r in args.rates_b.split(','))
    except ValueError:parser.error('rates must be44100 and/or48000')
    if not rates_a or not rates_b or any(r not in (44100,48000) for r in rates_a+rates_b):parser.error('unsupported recording rate')
    if not 1 <= args.seconds <= 7200:parser.error('seconds must be 1..7200')
    if len({args.auth_port,args.port_a,args.port_b}) != 3:parser.error('ports must differ')
    if not all(1024<=p<=65535 for p in [args.auth_port,args.port_a,args.port_b]+([args.auth_port_b] if args.auth_port_b is not None else [])):parser.error('invalid port')
    if args.auth_port_b in (args.port_a,args.port_b):parser.error('auth port B conflicts with endpoint listener')
    if not BINARY.exists():parser.error('build native/naa-native --bin naa-android first')
    for port in (args.port_a,args.port_b):
        with socket.socket() as check:
            check.setsockopt(socket.SOL_SOCKET,socket.SO_REUSEADDR,1)
            check.bind(('127.0.0.1',port))
    with socket.create_connection(('127.0.0.1',args.auth_port),2):pass
    args.directory.mkdir(mode=0o700,parents=True,exist_ok=False)
    # Ordinary PCM source for explicit HQPlayer loading. Opens no audio device.
    with wave.open(str(args.directory/'source.wav'),'wb') as wav:
        wav.setnchannels(2);wav.setsampwidth(2);wav.setframerate(44100)
        block=b''.join(struct.pack('<hh',int(4096*math.sin(2*math.pi*440*i/44100)),int(3072*math.sin(2*math.pi*660*i/44100))) for i in range(44100))
        for _ in range(30):wav.writeframes(block)
    stopped=threading.Event();signal.signal(signal.SIGTERM,lambda *_:stopped.set());signal.signal(signal.SIGINT,lambda *_:stopped.set())
    fixtures=[]
    try:
        for name,port,rates in [('a',args.port_a,rates_a),('b',args.port_b,rates_b)]:
            fixtures.append(Fixture(name,port,(args.auth_port_b or args.auth_port) if name=='b' else args.auth_port,args.directory/name,stopped,rates))
        ready=dict(kind='naa-router-live-recording',pid=os.getpid(),source=str(args.directory/'source.wav'),
                   auth='fresh official helper relay; distinct identity must be measured, never inferred from ports',
                   endpoints=[dict(name=f.name,address='127.0.0.1:'+str(f.port),device=DEVICE,pid=f.proc.pid) for f in fixtures])
        (args.directory/'ready.json').write_text(json.dumps(ready,indent=2)+'\n')
        print(json.dumps(ready),flush=True)
        deadline=time.monotonic()+args.seconds
        while not stopped.wait(.25):
            if time.monotonic()>=deadline:break
            for f in fixtures:
                if f.proc.poll() is not None:raise RuntimeError(f.name+' Rust process exited')
    finally:
        stopped.set()
        for f in fixtures:f.close()
        report=dict(fixtures=[dict(name=f.name,error=f.error) for f in fixtures],sinks=compare(args.directory))
        (args.directory/'report.json').write_text(json.dumps(report,indent=2)+'\n')
        print(json.dumps(report),flush=True)
    if any(f.error for f in fixtures):
        raise SystemExit(1)

if __name__=='__main__':main()
