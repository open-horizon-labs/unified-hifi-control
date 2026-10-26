"""Final browser-artifact gate: debug leakage, malformed input, immutable hashes."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("wasm_release", ROOT / "scripts/check-wasm-release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def leb(value):
    result = bytearray()
    while value >= 128:
        result.append((value & 127) | 128)
        value >>= 7
    return bytes(result + bytes([value]))


def custom(name, content=b"payload"):
    name = name.encode()
    payload = leb(len(name)) + name + content
    return b"\0" + leb(len(payload)) + payload


class FinalBrowserArtifacts(unittest.TestCase):
    def check(self, data, extra=None):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "client.wasm"
            path.write_bytes(data)
            if extra:
                (Path(directory) / extra[0]).write_bytes(extra[1])
            result = release.audit(directory)
            self.assertEqual(path.read_bytes(), data, "audit must not invalidate content hashes")
            return result

    def test_preserves_runtime_and_bindgen_metadata(self):
        result = self.check(release.MAGIC + custom("target_features") + custom("__wasm_bindgen_unstable"))
        self.assertEqual(len(result[0]["custom_sections"]), 2)

    def test_empty_name_envelope_has_no_debug_data(self):
        result = self.check(release.MAGIC + custom("name", b""))
        self.assertEqual(result[0]["custom_sections"][0]["content_bytes"], 0)

    def test_rejects_each_debug_kind_even_when_not_last_section(self):
        for name in [".debug_info", ".debug_str", ".debug_line", ".zdebug_info",
                     "name", "sourceMappingURL", "external_debug_info"]:
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "shipped debug"):
                self.check(release.MAGIC + custom(name) + custom("target_features"))

    def test_rejects_browser_maps_and_inline_map_references(self):
        for name, content in [("app.js.map", b"{}"), ("app.wasm.map", b"{}"),
                              ("app.css.map", b"{}"), ("app.js", b"//# sourceMappingURL=data:debug"),
                              ("app.css", b"/*# sourceMappingURL=app.css.map */")]:
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, "source.map"):
                self.check(release.MAGIC, (name, content))

    def test_rejects_empty_directories(self):
        with tempfile.TemporaryDirectory() as directory, self.assertRaisesRegex(ValueError, "no final WASM"):
            release.audit(directory)

    def test_rejects_truncated_and_overflowed_modules(self):
        for data in [b"", release.MAGIC + b"\0\x80", release.MAGIC + b"\0\x7fabc",
                     release.MAGIC + b"\0\x01\x20", release.MAGIC + b"\0\xff\xff\xff\xff\xff"]:
            with self.subTest(data=data), self.assertRaises(ValueError):
                self.check(data)


if __name__ == "__main__":
    unittest.main()
