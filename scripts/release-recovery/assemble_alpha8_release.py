#!/usr/bin/env python3
"""Assemble alpha.8 release assets from immutable CI artifacts; never uploads."""
from __future__ import annotations
import hashlib
import os
import shutil
import stat
import subprocess
import zipfile
from pathlib import Path

ROOT = Path(os.environ.get("ALPHA8_RECOVERY_ROOT", "/srv/agent-data/work/unified-hifi-control/alpha8-recovery"))
ARTIFACTS = ROOT / "artifacts"
FALLBACK = Path(os.environ.get("ALPHA8_MAC_ARTIFACT_ROOT", ROOT / "fallback33966821484"))
RELEASE = ROOT / "release"
VERSION = "4.0.0-alpha.8"
TAG = "v4.0.0-alpha.8"
# Standard upload-release inventory: 13 unified-hifi binaries/packages, one
# companion DMG, one LMS ZIP, one external feed, and SHA256SUMS.
EXPECTED_ASSETS = 17
LINUX_X64_SHA256 = "713f42c9f71f11307d6c649637f9c62eb542c0bed7d334826f9768a6066a0d93"


def find_one(root: Path, name: str) -> Path:
    matches = sorted(root.rglob(name))
    if len(matches) != 1:
        raise SystemExit(f"expected exactly one {name} below {root}, found {len(matches)}")
    return matches[0]


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def copy_artifact(name: str, destination: Path) -> Path:
    source = find_one(ARTIFACTS, name)
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)
    return destination


def require_macho_fat(path: Path) -> None:
    output = subprocess.check_output(["file", str(path)], text=True)
    if "Mach-O universal binary" not in output or "x86_64" not in output or "arm64" not in output:
        raise SystemExit(f"{path} is not an x86_64+arm64 Mach-O universal binary: {output.strip()}")


def zip_metadata(info: zipfile.ZipInfo) -> tuple:
    return (info.filename, info.date_time, info.compress_type, info.comment, info.extra,
            info.create_system, info.create_version, info.extract_version,
            info.flag_bits, info.external_attr)


def rebuild_lms_zip(source: Path, destination: Path, mac_bridge: Path, mac_pair: Path) -> None:
    with zipfile.ZipFile(source) as original:
        original_infos = original.infolist()
        original_names = [info.filename for info in original_infos]
        if len(original_names) != len(set(original_names)):
            raise SystemExit("source LMS ZIP contains duplicate entry names")
        original_metadata = {info.filename: zip_metadata(info) for info in original_infos}
        original_bytes = {info.filename: original.read(info.filename) for info in original_infos if not info.is_dir()}
        install = original.read("UnifiedHiFi/install.xml").decode("utf-8")
        if "<version>4.0.0-alpha.8</version>" not in install:
            raise SystemExit("LMS install.xml does not declare 4.0.0-alpha.8")
        if b"4.0.0-alpha.7" not in original.read("UnifiedHiFi/repo-beta.xml"):
            raise SystemExit("source embedded repo-beta.xml unexpectedly changed")

        with zipfile.ZipFile(source) as check:
            existing = {info.filename for info in check.infolist()}
        additions = {
            "UnifiedHiFi/Bin/darwin/unified-hifi-control": mac_bridge,
            "UnifiedHiFi/Bin/darwin/uhc-hiphi-pair": mac_pair,
        }
        if existing.intersection(additions):
            raise SystemExit("source LMS ZIP already contains a macOS payload")
        shutil.copy2(source, destination)
        with zipfile.ZipFile(destination, "a", compression=zipfile.ZIP_DEFLATED) as rebuilt:
            for relative, source_path in additions.items():
                info = zipfile.ZipInfo(relative)
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | 0o755) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                rebuilt.writestr(info, source_path.read_bytes())

    with zipfile.ZipFile(destination) as rebuilt:
        for info in original_infos:
            if zip_metadata(rebuilt.getinfo(info.filename)) != original_metadata[info.filename]:
                raise SystemExit(f"metadata changed for original LMS entry {info.filename}")
            if not info.is_dir() and rebuilt.read(info.filename) != original_bytes[info.filename]:
                raise SystemExit(f"content changed for original LMS entry {info.filename}")
        for name, source_path in (
            ("UnifiedHiFi/Bin/darwin/unified-hifi-control", mac_bridge),
            ("UnifiedHiFi/Bin/darwin/uhc-hiphi-pair", mac_pair),
        ):
            info = rebuilt.getinfo(name)
            if info.external_attr != ((stat.S_IFREG | 0o755) << 16):
                raise SystemExit(f"{name} is not executable")
            if rebuilt.read(name) != source_path.read_bytes():
                raise SystemExit(f"{name} does not match the macOS artifact")


def main() -> None:
    mac_bridge = find_one(FALLBACK, "unified-hifi-macos-universal")
    mac_pair = find_one(FALLBACK, "uhc-hiphi-pair-macos-universal")
    companion = find_one(FALLBACK, "unified-hifi-applemusic-companion-macos-arm64-4.0.0-alpha.8.dmg")
    require_macho_fat(mac_bridge)
    require_macho_fat(mac_pair)

    original_lms = find_one(ARTIFACTS, "lms-unified-hifi-control-4.0.0-alpha.8.zip")
    if RELEASE.exists():
        shutil.rmtree(RELEASE)
    RELEASE.mkdir()

    root_names = [
        "unified-hifi-linux-x64", "unified-hifi-linux-arm64", "unified-hifi-linux-armv7",
        "unified-hifi-win64.exe",
        "unified-hifi-control_4.0.0-alpha.8_amd64.deb",
        "unified-hifi-control_4.0.0-alpha.8_arm64.deb",
        "unified-hifi-control_4.0.0-alpha.8_armhf.deb",
        "unified-hifi-control-4.0.0_alpha.8-1.x86_64.rpm",
        "unified-hifi-control_4.0.0-alpha.8_x86_64.qpkg",
        "unified-hifi-control_4.0.0-alpha.8_arm_64.qpkg",
        "unified-hifi-control-x86_64-4.0.0-alpha.8-dsm7.spk",
        "unified-hifi-control-armv8-4.0.0-alpha.8-dsm7.spk",
    ]
    for name in root_names:
        copy_artifact(name, RELEASE / name)
    shutil.copy2(mac_bridge, RELEASE / mac_bridge.name)
    shutil.copy2(companion, RELEASE / companion.name)

    final_lms = RELEASE / original_lms.name
    rebuild_lms_zip(original_lms, final_lms, mac_bridge, mac_pair)
    lms_sha1 = hashlib.sha1(final_lms.read_bytes()).hexdigest()
    (RELEASE / "lms-beta.xml").write_text(f"""<?xml version="1.0"?>
<extensions>
  <plugins>
    <plugin name="UnifiedHiFi" version="{VERSION}" minTarget="8.0" maxTarget="*">
      <title lang="EN">Unified Hi-Fi Control (Beta)</title>
      <desc lang="EN">Beta builds of Unified Hi-Fi Control for testing.</desc>
      <url>https://github.com/open-horizon-labs/unified-hifi-control/releases/download/{TAG}/{final_lms.name}</url>
      <sha>{lms_sha1}</sha>
      <creator>Muness Castle</creator>
      <link>https://github.com/open-horizon-labs/unified-hifi-control/releases/tag/{TAG}</link>
    </plugin>
  </plugins>
</extensions>
""", encoding="utf-8")

    linux_x64 = RELEASE / "unified-hifi-linux-x64"
    if sha256(linux_x64) != LINUX_X64_SHA256:
        raise SystemExit("Linux x86_64 bridge SHA256 does not match known alpha8 artifact")

    with zipfile.ZipFile(final_lms) as archive:
        checks = {
            "x86_64-linux": ("binary-x86_64-unknown-linux-musl", "unified-hifi-linux-x64", "uhc-hiphi-pair-x64", "unified-hifi-control", "uhc-hiphi-pair"),
            "arm-linux": ("binary-armv7-unknown-linux-musleabihf", "unified-hifi-linux-armv7", "uhc-hiphi-pair-armv7", "unified-hifi-control", "uhc-hiphi-pair"),
            "aarch64-linux": ("binary-aarch64-unknown-linux-musl", "unified-hifi-linux-arm64", "uhc-hiphi-pair-arm64", "unified-hifi-control", "uhc-hiphi-pair"),
            "MSWin32-x64-multi-thread": ("binary-windows", "unified-hifi-win64.exe", "uhc-hiphi-pair-win64.exe", "unified-hifi-control.exe", "uhc-hiphi-pair.exe"),
            "darwin": (None, None, None, "unified-hifi-control", "uhc-hiphi-pair"),
        }
        for platform, (artifact_dir, bridge, pair, zip_bridge, zip_pair) in checks.items():
            if platform == "darwin":
                expected_bridge, expected_pair = mac_bridge, mac_pair
                names = (f"UnifiedHiFi/Bin/{platform}/{zip_bridge}", f"UnifiedHiFi/Bin/{platform}/{zip_pair}")
            else:
                expected_bridge = find_one(ARTIFACTS / artifact_dir, bridge)
                expected_pair = find_one(ARTIFACTS / artifact_dir, pair)
                names = (f"UnifiedHiFi/Bin/{platform}/{zip_bridge}", f"UnifiedHiFi/Bin/{platform}/{zip_pair}")
            if archive.read(names[0]) != expected_bridge.read_bytes() or archive.read(names[1]) != expected_pair.read_bytes():
                raise SystemExit(f"LMS bridge/pair bytes mismatch for {platform}")

    sums = [f"{sha256(path)}  {path.name}" for path in sorted(RELEASE.iterdir()) if path.name != "SHA256SUMS"]
    (RELEASE / "SHA256SUMS").write_text("\n".join(sums) + "\n", encoding="utf-8")
    assets = list(RELEASE.iterdir())
    if len(assets) != EXPECTED_ASSETS:
        raise SystemExit(f"expected {EXPECTED_ASSETS} final assets, found {len(assets)}")
    print(f"assembled {len(assets)} assets in {RELEASE}")
    print(f"LMS SHA1: {lms_sha1}")
    print(f"Linux x86_64 SHA256: {sha256(linux_x64)}")
    for path in sorted(assets):
        print(f"{path.name}\t{path.stat().st_size}")


if __name__ == "__main__":
    main()
