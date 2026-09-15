//! Exercise real elapsed deadlines, rather than trusting per-read timeout setup.
fn run(mode: &str) {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"
import contextlib, socket, sys, threading, time
sys.path.insert(0, sys.argv[2])
import naa_router_lab as lab
lab.BINARY = sys.argv[1]
lab.TIMEOUT = 25 # The fake NAA must not close earlier than the router deadline.
r = lab.Router()
mode = sys.argv[3]
naa = conn = None
done = threading.Event()
worker = None
try:
    if mode == 'prefix':
        naa = lab.FakeNaa('Deadline fixture', 'deadline-dac', 44100)
        r.choose(r.add(naa))
        conn, _, _ = r.connect('partial-prefix')
        conn.sendall(b'<')
        started = time.monotonic()
        conn.settimeout(12)
        try:
            assert conn.recv(1) == b'', 'unexpected response to an incomplete record'
        except ConnectionResetError:
            pass
        elapsed = time.monotonic() - started
        assert 8 <= elapsed < 12, ('not the expected router record deadline', elapsed)
        assert r.api('GET', '/api/state')[1]['last_error'], 'deadline error was hidden'
    else:
        conn = socket.create_connection(('127.0.0.1', r.http_port), 2)
        host = f'127.0.0.1:{r.http_port}'
        if mode == 'headers':
            initial = f'GET /api/state HTTP/1.1\r\nHost: {host}\r\nX-Trickle: '.encode()
        else:
            initial = f'POST /api/routes HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: 10000\r\n\r\n'.encode()
        conn.sendall(initial)
        writes = []
        def trickle():
            while not done.wait(0.2):
                try:
                    conn.sendall(b'x')
                    writes.append(time.monotonic())
                except OSError:
                    return
        worker = threading.Thread(target=trickle, daemon=True)
        started = time.monotonic()
        worker.start()
        conn.settimeout(12)
        try:
            assert conn.recv(1) == b'', 'unexpected response to incomplete HTTP request'
        except ConnectionResetError:
            pass
        elapsed = time.monotonic() - started
        assert 8 <= elapsed < 12, ('whole-request deadline not enforced', elapsed)
        assert len(writes) >= 5, 'fixture failed before delivering a genuine slow trickle'
        assert r.api('GET', '/api/state')[0] == 200, 'expired request damaged selector'
finally:
    done.set()
    if conn:
        conn.close()
    if worker:
        worker.join(1)
    if naa:
        naa.close()
    r.close()
"#;
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_naa-router"))
        .arg(manifest.join("../../tools"))
        .arg(mode)
        .output()
        .expect("python3 is required for the deadline fixtures");
    assert!(
        output.status.success(),
        "{mode} deadline regression failed\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn incomplete_authenticated_record_prefix_expires() {
    run("prefix");
}

#[test]
fn trickled_http_headers_have_a_whole_request_deadline() {
    run("headers");
}

#[test]
fn trickled_http_body_has_a_whole_request_deadline() {
    run("body");
}
