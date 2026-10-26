#!/usr/bin/env python3
"""Opt-in hydrated UI checks against an isolated loopback SSR server and dx assets.

Requires Playwright. All catalog, zone and playback APIs are fixtures; no provider
or live installation is touched. Provider behavior is tested in controller_shortcuts.rs.
Example: python scripts/check_shortcuts_browser.py --url http://127.0.0.1:18898
--assets-url http://127.0.0.1:18797 --output /tmp/shortcut-browser
Pass --chromium when using a system browser; otherwise Playwright's browser is used.
"""

import argparse
import json, os
from pathlib import Path
from urllib.parse import urlparse, parse_qs
from playwright.sync_api import sync_playwright

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--url", required=True)
parser.add_argument("--assets-url")
parser.add_argument("--output", default="/tmp/shortcut-browser")
parser.add_argument("--chromium")
args = parser.parse_args()
assert urlparse(args.url).hostname in [
    "127.0.0.1",
    "localhost",
], "Use an isolated loopback server"
if args.assets_url:
    assert urlparse(args.assets_url).hostname in ["127.0.0.1", "localhost"]
out = Path(args.output)
out.mkdir(parents=True, exist_ok=True)
calls = []
requests = []
errors = []
rows = [
    {
        "id": "sc_evening",
        "name": "Evening listening",
        "title": "Evening listening",
        "provider": "lms",
    },
    {"id": "sc_other", "name": "A Roon playlist", "provider": "roon"},
]
zone = {
    "zone_id": "lms:room",
    "zone_name": "Listening room",
    "source": "lms",
    "browse_supported": True,
    "library_tabs": ["browse", "playlists"],
}
np = {
    "is_playing": False,
    "is_previous_allowed": True,
    "is_next_allowed": True,
    "volume": 30,
    "volume_type": "number",
}
mode = "normal"


def route(r):
    global rows
    req = r.request
    u = urlparse(req.url)
    p = u.path
    requests.append(p)
    if u.hostname not in ["127.0.0.1", "localhost"]:
        return r.abort()

    def answer(v, status=200):
        r.fulfill(status=status, content_type="application/json", body=json.dumps(v))

    if p == "/zones":
        return answer({"zones": [zone]})
    if p == "/status":
        return answer({"version": "shortcut-test", "git_sha": "working-tree"})
    if p == "/api/controller/status":
        return answer(
            {"authenticated": True, "auth_required": False, "bootstrap_required": False}
        )
    if p == "/api/shortcuts" and req.method == "GET":
        if mode == "failed":
            return answer({"error": "Catalog offline"}, 503)
        start = int(parse_qs(u.query).get("offset", ["0"])[0])
        limit = int(parse_qs(u.query).get("limit", [str(len(rows) or 1)])[0])
        end = start + limit
        return answer(
            {
                "version": 1,
                "shortcuts": rows[start:end],
                "next_offset": end if end < len(rows) else None,
            }
        )
    if p.startswith("/api/shortcuts/") and p.endswith("/play"):
        calls.append({"url": p, "body": req.post_data_json})
        return answer(
            {
                "ok": True,
                "code": "accepted",
                "show_notice": mode == "multiple",
                "message": (
                    "Multiple matches found. Sent the first exact match to play."
                    if mode == "multiple"
                    else "Command accepted"
                ),
            }
        )
    if p == "/api/collections":
        return answer(
            {
                "outcome": "ok",
                "data": {"items": [{"title": "A new playlist", "ref": "ref_new"}]},
            }
        )
    if p == "/api/shortcuts" and req.method == "POST":
        row = {
            "id": "sc_new",
            "name": "A new playlist",
            "provider": "lms",
            "title": "A new playlist",
        }
        rows.append(row)
        return answer({"shortcut": row, "already_pinned": False})
    if "now_playing" in p:
        return answer(np)
    if p in ["/events", "/ws"]:
        return r.abort()
    if p.startswith(("/api/", "/settings", "/knobs", "/hqplayer")):
        return answer({})

    if args.assets_url and p.startswith(("/wasm/", "/assets/")):
        return r.fulfill(response=r.fetch(url=args.assets_url.rstrip("/") + u.path))
    return r.continue_()


with sync_playwright() as pl:
    browser = pl.chromium.launch(
        headless=True, executable_path=args.chromium, args=["--no-sandbox"]
    )
    page = browser.new_page(viewport={"width": 1440, "height": 1000})
    page.route("**/*", route)
    page.on("pageerror", lambda e: errors.append(str(e)))
    page.goto(args.url)
    page.wait_for_timeout(3000)

    section = page.locator("#shortcuts")
    section.get_by_role("button", name="Evening listening").wait_for()
    assert section.get_by_role("button", name="A Roon playlist").is_disabled()
    section.get_by_role("button", name="Evening listening").evaluate(
        "(el)=>{el.click();el.click()}"
    )
    page.wait_for_timeout(500)
    assert len(calls) == 1, calls
    assert calls[0]["body"] == {"zone_id": "lms:room"}, calls
    assert "Command accepted" in section.inner_text()
    page.screenshot(path=str(out / "web-shortcuts-desktop.png"), full_page=True)
    page.set_viewport_size({"width": 390, "height": 844})
    page.wait_for_timeout(300)
    assert page.evaluate("document.documentElement.scrollWidth") <= 390
    page.screenshot(path=str(out / "web-shortcuts-mobile.png"), full_page=True)
    section.locator("summary").click()
    page.get_by_role("textbox", name="Name of shortcut Evening listening").wait_for()
    assert page.get_by_role(
        "button", name="Delete shortcut Evening listening"
    ).is_visible()
    page.screenshot(path=str(out / "web-shortcuts-manage-mobile.png"), full_page=True)
    # Actual pin gesture, with a fake durable-ref API so no live music or settings change.
    section.locator("summary").click()
    pin = page.get_by_role("button", name="Add A new playlist to Shortcuts")
    if pin.count():
        pin.click()
        page.wait_for_timeout(600)
        assert section.get_by_role("button", name="A new playlist").count() == 1
    else:
        raise AssertionError("Library did not expose Add to Shortcuts")
    # A failed read cannot masquerade as an empty catalog.
    rows = []
    page.reload()
    page.wait_for_timeout(600)
    assert "Save a playlist or album" in section.inner_text()
    mode = "failed"
    page.reload()
    page.wait_for_timeout(600)
    assert "Catalog offline" in section.inner_text()
    assert "Save a playlist or album" not in section.inner_text()
    mode = "normal"
    rows = [
        {
            "id": f"sc_{i}",
            "name": f"{i:02} " + ("音楽" * 90 if i == 0 else "Playlist"),
            "provider": "lms",
        }
        for i in range(13)
    ]
    section.get_by_role("button", name="Retry", exact=True).click()
    page.wait_for_timeout(500)
    assert section.get_by_role("button", name="More shortcuts").count() == 1
    assert section.get_by_role("button", name="12 Playlist").count() == 0
    section.get_by_role("button", name="More shortcuts").click()
    page.wait_for_timeout(500)
    assert section.get_by_role("button", name="12 Playlist").count() == 1
    section.get_by_role("button", name="Previous", exact=True).click()
    page.wait_for_timeout(500)
    assert section.get_by_role("button", name="12 Playlist").count() == 0
    assert page.evaluate("document.documentElement.scrollWidth") <= 390
    assert len(calls) == 1, "Browsing and refreshing must not play music"
    mode = "multiple"
    section.get_by_role("button", name="01 Playlist").click()
    page.wait_for_timeout(500)
    assert (
        "Multiple matches found. Sent the first exact match to play."
        in section.inner_text()
    )
    assert len(calls) == 2
    assert calls[-1]["body"]["zone_id"] == "lms:room"
    assert not errors, errors
    out.joinpath("web-check.json").write_text(
        json.dumps(
            {
                "calls": calls,
                "requests": requests,
                "errors": errors,
                "text": page.locator("body").inner_text(),
            },
            indent=2,
        )
    )
    browser.close()
print("Browser shortcut assertions passed")
