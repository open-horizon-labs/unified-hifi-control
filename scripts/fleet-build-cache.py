#!/usr/bin/env python3
"""Transfer isolated Cargo state to the fleet's NAS S3 service (no dependencies)."""
import argparse
import datetime
import hashlib
import hmac
import http.client
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import time
from urllib.parse import quote, urlsplit


def cache_key(workspace, flavor):
    digest = hashlib.sha256(flavor.encode())
    for name in ["Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"]:
        digest.update(name.encode())
        digest.update((workspace / name).read_bytes())
    return "uhc-build-state/v1/" + quote(flavor, safe="") + "/" + digest.hexdigest() + ".tar.gz"


def request(method, key, body=None):
    endpoint = urlsplit(os.environ["SCCACHE_ENDPOINT"])
    if endpoint.scheme not in {"http", "https"} or endpoint.path not in {"", "/"}:
        raise ValueError("expected an HTTP(S) S3 endpoint without a path")
    region = os.environ.get("SCCACHE_REGION", "us-east-1")
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    date = stamp[:8]
    payload_hash = hashlib.sha256()
    if body:
        with body.open("rb") as stream:
            for chunk in iter(lambda: stream.read(8 * 1024 * 1024), b""):
                payload_hash.update(chunk)
    headers = {"host": endpoint.netloc, "x-amz-content-sha256": payload_hash.hexdigest(), "x-amz-date": stamp}
    if os.environ.get("AWS_SESSION_TOKEN"):
        headers["x-amz-security-token"] = os.environ["AWS_SESSION_TOKEN"]
    names = ";".join(sorted(headers))
    path = "/" + quote(os.environ["SCCACHE_BUCKET"], safe="") + "/" + key
    canonical = "\n".join([method, path, "", "".join(k + ":" + headers[k] + "\n" for k in sorted(headers)), names, payload_hash.hexdigest()])
    scope = date + "/" + region + "/s3/aws4_request"
    to_sign = "AWS4-HMAC-SHA256\n" + stamp + "\n" + scope + "\n" + hashlib.sha256(canonical.encode()).hexdigest()
    signing = ("AWS4" + os.environ["AWS_SECRET_ACCESS_KEY"]).encode()
    for part in [date, region, "s3", "aws4_request"]:
        signing = hmac.new(signing, part.encode(), hashlib.sha256).digest()
    signature = hmac.new(signing, to_sign.encode(), hashlib.sha256).hexdigest()
    headers["Authorization"] = "AWS4-HMAC-SHA256 Credential=" + os.environ["AWS_ACCESS_KEY_ID"] + "/" + scope + ", SignedHeaders=" + names + ", Signature=" + signature
    connection_class = http.client.HTTPSConnection if endpoint.scheme == "https" else http.client.HTTPConnection
    connection = connection_class(endpoint.hostname, endpoint.port, timeout=90)
    if body:
        headers["Content-Length"] = str(body.stat().st_size)
        with body.open("rb") as stream:
            connection.request(method, path, body=stream, headers=headers)
    else:
        connection.request(method, path, headers=headers)
    return connection, connection.getresponse()


def extract(archive, workspace, cargo):
    # Extract before merging, so a malformed archive cannot partially alter Cargo.
    with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP")) as temporary:
        root = Path(temporary)
        with tarfile.open(archive) as bundle:
            for member in bundle.getmembers():
                parts = Path(member.name).parts
                if not parts or parts[0] not in {"target", "cargo"}:
                    raise ValueError("unexpected cache root")
                if parts[0] == "cargo" and (len(parts) < 2 or parts[1] not in {"registry", "git"}):
                    raise ValueError("cache must not replace Cargo tools or credentials")
            bundle.extractall(root, filter="data")
        for source, destination in [(root / "target", workspace / "target"), (root / "cargo/registry", cargo / "registry"), (root / "cargo/git", cargo / "git")]:
            if source.exists():
                shutil.copytree(source, destination, dirs_exist_ok=True, symlinks=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("mode", choices=["restore", "save"])
    parser.add_argument("--flavor", required=True)
    args = parser.parse_args()
    workspace = Path(os.environ["GITHUB_WORKSPACE"])
    cargo = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    key = cache_key(workspace, args.flavor)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(dir=os.environ.get("RUNNER_TEMP")) as temporary:
        archive = Path(temporary) / "state.tar.gz"
        if args.mode == "restore":
            connection, response = request("GET", key)
            try:
                if response.status == 404:
                    print("NAS Cargo state: cache miss")
                    return
                if response.status != 200:
                    raise RuntimeError("NAS cache GET returned HTTP " + str(response.status))
                with archive.open("wb") as stream:
                    shutil.copyfileobj(response, stream, 8 * 1024 * 1024)
            finally:
                connection.close()
            extract(archive, workspace, cargo)
        else:
            # Each job owns its working directories. S3 PUT publishes a complete
            # immutable snapshot atomically; jobs never share mutable target/.
            arguments = ["tar", "-cf", "-"]
            if (workspace / "target").exists():
                arguments += ["--exclude=target/dx", "--exclude=target/cargo-timings", "-C", str(workspace), "target"]
            for name in ["registry", "git"]:
                if (cargo / name).exists():
                    arguments += ["--transform=s,^" + name + ",cargo/" + name + ",", "-C", str(cargo), name]
            if len(arguments) == 3:
                print("NAS Cargo state: nothing to save")
                return
            with archive.open("wb") as output:
                producer = subprocess.Popen(arguments, stdout=subprocess.PIPE)
                compressor = subprocess.run(["gzip", "-1"], stdin=producer.stdout, stdout=output)
                producer.stdout.close()
                if producer.wait() or compressor.returncode:
                    raise RuntimeError("cache archive creation failed")
            connection, response = request("PUT", key, archive)
            try:
                response.read()
                if response.status not in {200, 201, 204}:
                    raise RuntimeError("NAS cache PUT returned HTTP " + str(response.status))
            finally:
                connection.close()
        print("NAS Cargo state " + args.mode + ": " + str(round(archive.stat().st_size / 1024**2)) + " MiB in " + str(round(time.monotonic() - started, 1)) + "s")


if __name__ == "__main__":
    main()
