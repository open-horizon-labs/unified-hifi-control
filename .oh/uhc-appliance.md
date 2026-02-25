# Session: uhc-appliance

## Aim
**Updated:** 2026-02-20

**Aim:** Anyone can run UHC by flashing an SD card and plugging in a Pi. No Docker, no NAS, no Linux knowledge. The bridge is invisible infrastructure that just works.

**Current State:** Running UHC requires: choose a platform (Docker, Synology SPK, QNAP QPKG, bare binary), configure networking (host mode, port mapping), manage updates, troubleshoot. The target user who just wants their Roon/LMS/UPnP zones unified doesn't want to be a sysadmin.

**Desired State:** User flashes SD card → plugs Pi into ethernet/power → bridge auto-starts, discovers sources via mDNS/SSDP → user controls music from any client (knob, phone, web, AI). The Pi is invisible. The clients are the experience.

### Mechanism

**Change:** A purpose-built Pi Linux image (Pi OS Lite + UHC + systemd + mDNS + onboarding) distributed as a flashable .img file.

**Hypothesis:** The barrier to UHC adoption isn't features — it's deployment. Docker and NAS packages serve technical users. A flashable Pi image serves everyone else. The Pi Zero 2 W at $15 makes the hardware cost negligible.

**Assumptions:**
- Pi Zero 2 W (armv7, 512MB, WiFi) can run UHC comfortably (validate: the binary is ~15MB, async I/O, minimal CPU)
- WiFi is sufficient for mDNS/SSDP discovery (validate: multicast over WiFi can be flaky on some routers)
- Users have a way to flash SD cards (Raspberry Pi Imager makes this trivial)
- The onboarding experience can be delivered via the existing web UI at :8088 (no display attached to the Pi)

### Feedback

**Signal:**
1. Time from download to first zone discovered < 5 minutes
2. Support tickets shift from "how do I install" to "how do I use"
3. Pi image downloads outpace Docker pulls

**Timeframe:** Image buildable in days. Onboarding UX in a week. User feedback within a month of release.

### Guardrails

- **Don't fork the core** — Same UHC binary, just packaged into an image
- **Don't require a display** — Pi runs headless. Onboarding happens via web UI from phone/laptop on same network
- **Don't build a custom distro** — Pi OS Lite + configuration, not Yocto/Buildroot
- **Don't break existing deployment methods** — Docker, SPK, QPKG, bare binary remain first-class
- **Ethernet first, WiFi as bonus** — Multicast discovery is more reliable on wired. WiFi works but warn users.

### Decisions

1. **Bridge is the product, clients are accessories** — The Pi runs the bridge headless. Knob, touchscreen, phone, web, AI are all clients to the same HTTP API + SSE. No display needed on the Pi.
2. **Pi Zero 2 W is the target** — $15, WiFi, runs the existing armv7 musl binary. Cheapest possible invisible bridge.
3. **Pi 5/4/3 are also supported** — The image works on any Pi. Zero 2 W is the minimum spec / recommended for new users.
4. **Onboarding is a web experience** — User plugs in Pi, opens http://uhc.local:8088 from phone/laptop, sees setup wizard: network status → discovering sources → found N zones → here are your clients (knob, app, web).
5. **Display products are separate** — Touchscreen controller (ESP32 + LVGL), knob (existing), iOS app (alpha), web UI (existing). All independent products that connect to the bridge.

### Architecture

```
                    ┌─────────────┐
                    │  UHC Bridge  │  ← Pi Zero 2 W, $15, headless
                    │  (Pi image)  │     Flash SD, plug in, done
                    └──────┬──────┘
                           │ HTTP API + SSE + mDNS
          ┌────────┬───────┼───────┬──────────┐
          │        │       │       │          │
       ┌──┴──┐ ┌──┴──┐ ┌──┴──┐ ┌──┴──┐  ┌───┴───┐
       │Knob │ │Touch│ │ iOS │ │ Web │  │  MCP  │
       │OLED │ │LVGL │ │Watch│ │Dioxus│  │Claude │
       └─────┘ └─────┘ └─────┘ └─────┘  └───────┘
       exists   future  alpha   exists    exists
```

### Dissent Record

**ESP32 embedded bridge (C rewrite):** Considered and rejected for now. The knob already exists as the thin client. Rewriting the bridge in C for ESP32 is a 6-month bet that duplicates a working Rust codebase. The Pi Zero at $15 makes the cost argument moot — it's cheaper than an ESP32-P4 dev board.

**Pi 5 all-in-one appliance:** Original concept. Rejected after challenge: $187 for a device to run a 15MB binary is not a compelling product when the user already has something to run it on. The display should be a separate product (client), not bundled with the server.

---

## Problem Statement
**Updated:** 2026-02-20

**Problem:** Make a Pi Linux image with UHC bundled in and excellent onboarding.

**Current framing:** Build a flashable Pi image that turns any Pi into a UHC bridge with zero configuration.

**Decomposed:**

### Problem 1: No flashable image exists

A user who wants to run UHC on a Pi today must: install Pi OS, SSH in, download the binary, create a systemd service, configure auto-start, set up mDNS. That's 20+ minutes of Linux sysadmin work.

**Need:** A single .img.gz file. Flash with Pi Imager. Boot. Done.

### Problem 2: No onboarding experience

When UHC starts fresh on a headless Pi, nothing tells the user it's working. They have to guess the IP, open a browser, navigate to :8088. If discovery is still running, they see an empty zone list and think it's broken.

**Need:** mDNS advertisement so http://uhc.local works. A first-run UI that shows: "Discovering sources... Found Roon core. Found 3 LMS players. Found 2 UPnP renderers. You're ready. Here's how to connect your clients."

### Problem 3: No image build pipeline

Even once the image is defined, it needs to be reproducible — built in CI, versioned, distributed alongside Docker images and NAS packages in GitHub Releases.

**Need:** A scripted image build (pi-gen or similar) that runs in GitHub Actions and produces .img.gz artifacts for each release.

### Constraints

**Hard:**
- Must run on Pi Zero 2 W (armv7, 512MB RAM, WiFi)
- Must also run on Pi 3/4/5 (aarch64)
- Must use the existing UHC binary (no forking)
- Must auto-discover sources without manual configuration
- Must be accessible via http://uhc.local from any device on the network

**Soft:**
- "Pi OS Lite based" — could be DietPi or another minimal distro, but Pi OS Lite is the path of least resistance
- "pi-gen for image building" — alternatives exist (pi-bakery, custom scripts) but pi-gen is the standard
- "WiFi configuration" — could use Pi Imager's built-in WiFi config, or a captive portal, or just recommend ethernet

### What this framing enables

One workstream, not three. The deliverable is a flashable image + CI pipeline. The touch UI, the ESP32 touchscreen, the iOS app — all separate products that benefit from the bridge being easy to deploy but don't block it.

### What this framing excludes

- Touch-optimized UI (separate product)
- ESP32 touchscreen controller (separate product)
- Display/kiosk mode (the Pi has no display)
- OTA updates (v2 — for now, reflash or SSH + apt)
- Custom case/hardware (it's a bare Pi, hide it behind your amp)

---

## Plan
**Updated:** 2026-02-20
**Issues:** #256, #257, #258

| Issue | Title | Depends on |
|---|---|---|
| #256 | Flashable Pi image with WiFi captive portal provisioning | — |
| #257 | First-run onboarding experience in web UI | #256 (but benefits all deployment types) |
| #258 | GitHub Actions pipeline for Pi image builds | #256 |

#256 is the foundation. #257 and #258 can proceed in parallel once #256 has a working image.
