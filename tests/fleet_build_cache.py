import importlib.util
import io
import json
import os
from unittest import mock
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("fleet_cache", Path(__file__).resolve().parents[1] / "scripts/fleet-build-cache.py")
cache = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cache)


class FleetCacheTests(unittest.TestCase):
    def test_source_edits_reuse_dependencies_but_lock_toolchain_and_target_do_not(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".cargo").mkdir()
            for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"]:
                (root / name).write_text(name)
            first = cache.cache_key(root, "linux-x64")
            (root / "main.rs").write_text("changed application source")
            self.assertEqual(first, cache.cache_key(root, "linux-x64"))
            self.assertNotEqual(first, cache.cache_key(root, "linux-arm64"))
            with mock.patch.dict(os.environ, {"CARGO_PROFILE_RELEASE_LTO": "false" if os.environ.get("CARGO_PROFILE_RELEASE_LTO") != "false" else "thin", "CARGO_PROFILE_RELEASE_CODEGEN_UNITS": "4"}):
                self.assertNotEqual(first, cache.cache_key(root, "linux-x64"))
            for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"]:
                original = (root / name).read_text()
                (root / name).write_text(original + " changed")
                self.assertNotEqual(first, cache.cache_key(root, "linux-x64"))
                (root / name).write_text(original)

    def test_windows_tool_recipe_changes_invalidate_only_windows_snapshots(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".cargo").mkdir()
            (root / "build/prepared-builders").mkdir(parents=True)
            inputs = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml",
                      "build/prepared-builders/windows-source.yml", "build/prepared-builders/windows-profile.json"]
            for name in inputs:
                (root / name).write_text(name)
            windows = cache.cache_key(root, "windows-x86_64-pc-windows-msvc")
            linux = cache.cache_key(root, "linux-x64")
            for name in inputs[-2:]:
                original = (root / name).read_text()
                (root / name).write_text(original + " changed SDK/compiler")
                self.assertNotEqual(windows, cache.cache_key(root, "windows-x86_64-pc-windows-msvc"))
                self.assertEqual(linux, cache.cache_key(root, "linux-x64"))
                (root / name).write_text(original)

    def test_unsafe_archive_cannot_overwrite_tools_or_escape_working_directory(self):
        for name in ["cargo/bin/cargo", "target/../../escaped", "cargo/credentials.toml"]:
            with self.subTest(name=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                archive = root / "bad.tar.gz"
                with tarfile.open(archive, "w:gz") as bundle:
                    valid = tarfile.TarInfo("target/dependency.rlib")
                    valid.size = 4
                    bundle.addfile(valid, io.BytesIO(b"safe"))
                    bad = tarfile.TarInfo(name)
                    bad.size = 3
                    bundle.addfile(bad, io.BytesIO(b"bad"))
                workspace = root / "work"
                workspace.mkdir()
                with self.assertRaises((ValueError, tarfile.FilterError)):
                    cache.extract(archive, workspace, root / "cargo")
                self.assertFalse((workspace / "target/dependency.rlib").exists())
                self.assertFalse((root / "escaped").exists())

    def test_source_changes_skip_upload_but_dependency_changes_publish_new_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / ".cargo").mkdir()
            (root / "target").mkdir()
            (root / "target/dependency.rlib").write_bytes(b"compiled")
            for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", ".cargo/config.toml"]:
                (root / name).write_text(name)
            key = cache.cache_key(root, "linux")
            state = root / ("nas-cargo-state-" + cache.hashlib.sha256(key.encode()).hexdigest() + ".json")
            state.write_text(json.dumps({"key": key, "source_sha": "same-commit"}))
            environment = {"GITHUB_WORKSPACE": str(root), "CARGO_HOME": str(root / "cargo"), "RUNNER_TEMP": str(root), "GITHUB_SHA": "same-commit"}
            with mock.patch.dict(os.environ, environment), mock.patch("sys.argv", ["cache", "save", "--flavor", "linux"]), mock.patch.object(cache, "request", side_effect=AssertionError("upload attempted")) as transfer:
                cache.main()
                transfer.assert_not_called()
                os.environ["GITHUB_SHA"] = "new-commit"
                cache.main()
                transfer.assert_not_called()
                (root / "Cargo.lock").write_text("new dependencies")
                with self.assertRaisesRegex(AssertionError, "upload attempted"):
                    cache.main()
                transfer.assert_called_once()

    def test_restore_preserves_existing_wasm_assets_and_other_tool_files(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            workspace = root / "work"
            cargo = root / "cargo"
            (workspace / "target/dx").mkdir(parents=True)
            (workspace / "target/dx/current.wasm").write_bytes(b"current")
            (cargo / "bin").mkdir(parents=True)
            (cargo / "bin/dx").write_bytes(b"tool")
            archive = root / "cache.tar.gz"
            with tarfile.open(archive, "w:gz") as bundle:
                for name in ["target/debug/deps/library.rlib", "cargo/registry/src/library.rs"]:
                    item = tarfile.TarInfo(name)
                    item.size = 4
                    bundle.addfile(item, io.BytesIO(b"data"))
            cache.extract(archive, workspace, cargo)
            self.assertEqual(b"current", (workspace / "target/dx/current.wasm").read_bytes())
            self.assertEqual(b"tool", (cargo / "bin/dx").read_bytes())
            self.assertEqual(b"data", (workspace / "target/debug/deps/library.rlib").read_bytes())


if __name__ == "__main__":
    unittest.main()
