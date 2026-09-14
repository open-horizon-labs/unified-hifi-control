//! Stop is an immediate audio action even when configuration cannot be saved.
#[test]
fn stop_disconnects_and_unselects_even_when_persistence_fails() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"
import contextlib, os, pathlib, socket, sys
sys.path.insert(0, sys.argv[2])
import naa_router_lab as lab
lab.BINARY = sys.argv[1]
r = lab.Router()
naa = lab.FakeNaa('Stop failure fixture', 'fixture-stop', 44100)
conn = None
path = pathlib.Path(r.temp.name)
backup = path.with_name(path.name + '-saved')
try:
    r.choose(r.add(naa))
    conn, _, _ = r.connect('stop-save-failure')
    assert r.api('GET', '/api/state')[1]['session'] is not None
    # Deterministic failure independent of chmod/root behavior: parent is a file.
    path.rename(backup)
    path.write_text('configuration parent deliberately unavailable')
    status, body = r.api('POST', '/api/stop', {})
    conn.settimeout(2)
    try:
        assert conn.recv(1) == b'', 'Stop left audio transport connected after save failed'
    except ConnectionResetError:
        pass
    state = r.api('GET', '/api/state')[1]
    assert state['session'] is None, state
    assert state['selected_route_id'] is None, state
    assert state.get('last_error') or (isinstance(body, dict) and body.get('error')), 'persistence error was hidden'
    attempt = socket.create_connection(('127.0.0.1', r.naa_port), 2)
    try:
        attempt.settimeout(2)
        try:
            assert attempt.recv(1) == b'', 'new playback accepted after Stop'
        except ConnectionResetError:
            pass
    finally:
        attempt.close()
    after_retry = r.api('GET', '/api/state')[1]
    assert after_retry.get('last_error') == state.get('last_error'), 'automatic HQPlayer retry erased the unsaved-Stop error'
    lab.wait_until(lambda: naa.values('closed'))
finally:
    if backup.exists():
        path.unlink()
        backup.rename(path)
    if conn:
        conn.close()
    naa.close()
    r.close()
"#;
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_naa-router"))
        .arg(manifest.join("../../tools"))
        .output()
        .expect("python3 is required for the loopback Stop regression");
    assert!(
        output.status.success(),
        "Stop persistence regression failed\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn config_save_does_not_delete_unowned_temporary_files() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = r#"
import pathlib, sys
sys.path.insert(0, sys.argv[2])
import naa_router_lab as lab
lab.BINARY = sys.argv[1]
r = lab.Router()
try:
    directory = pathlib.Path(r.temp.name)
    foreign = directory / '.naa-router-424242.tmp'
    sentinel = b'foreign writer owns these bytes'
    foreign.write_bytes(sentinel)
    r.restart()
    assert foreign.exists() and foreign.read_bytes() == sentinel, 'startup removed a temporary file it did not create'
    collision = directory / f'.naa-router-{r.proc.pid}.tmp'
    collision.write_bytes(sentinel)
    # Saving may use another exclusively created name or report a collision.
    # It may never claim ownership by deleting somebody else's existing path.
    status, body = r.api('POST', '/api/routes', {'name':'Explicit fixture', 'host':'127.0.0.1', 'port':1})
    assert collision.exists() and collision.read_bytes() == sentinel, 'save removed or overwrote an unowned temporary file'
    assert status in (200, 201) or (status >= 400 and body.get('error')), (status, body)
finally:
    r.close()
"#;
    let output = std::process::Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(env!("CARGO_BIN_EXE_naa-router"))
        .arg(manifest.join("../../tools"))
        .output()
        .expect("python3 is required for the configuration ownership fixture");
    assert!(
        output.status.success(),
        "configuration ownership regression failed\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
