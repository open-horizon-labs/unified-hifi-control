# Install Unified Hi-Fi Control

Choose the host that already stays on with your hi-fi. All local playback and
discovery remain local; a HiPhi account is optional and is used only for hosted
features such as authenticated remote controllers and Spotify's secure callback.

## Home Assistant OS, Green, or Supervised

The supported add-on is the shortest path: it runs UHC, embeds the UI through
Home Assistant ingress, and installs the matching `media_player` integration.

[![Add add-on repository to Home Assistant](https://my.home-assistant.io/badges/supervisor_add_addon_repository.svg)](https://my.home-assistant.io/redirect/supervisor_add_addon_repository/?repository_url=https%3A%2F%2Fgithub.com%2Fopen-horizon-labs%2Fuhc-home-assistant-addon)

Manual repository URL:

```text
https://github.com/open-horizon-labs/uhc-home-assistant-addon
```

Follow the add-on's [complete installation guide](https://github.com/open-horizon-labs/uhc-home-assistant-addon/blob/main/unified-hifi-control/DOCS.md).

## NAS, Docker, LMS, or standalone binary

- **QNAP and Synology:** download the matching QPKG or SPK from
  [Releases](https://github.com/open-horizon-labs/unified-hifi-control/releases/latest).
- **Docker:** use a versioned `muness/unified-hifi-control:<version>` image for
  reproducible installs. `latest` follows the newest stable release.
- **Lyrion Music Server:** add
  `https://raw.githubusercontent.com/open-horizon-labs/unified-hifi-control/v4/lms-plugin/repo.xml`
  under Additional Repositories.
- **Linux, macOS, and Windows:** download the matching standalone artifact from
  [Releases](https://github.com/open-horizon-labs/unified-hifi-control/releases/latest).

See the README's [installation section](README.md#installation) for package
names, Docker Compose, checksums, and signature verification.

## Apple Music companion

- **iPhone and iPad:** the companion is in alpha. [Request TestFlight access](https://github.com/open-horizon-labs/unified-hifi-control/issues/new?title=Request%20Apple%20Music%20Companion%20TestFlight%20access).
- **Apple Silicon Mac:** download
  `unified-hifi-applemusic-companion-macos-arm64-*.dmg` from the latest
  [release assets](https://github.com/open-horizon-labs/unified-hifi-control/releases/latest).

After installing the companion, open **UHC → Settings → Apple Music** and
confirm that the pairing code shown by UHC matches the companion.

## HiPhi Cloud and Garmin

UHC works without the cloud. To add authenticated remote access or a Garmin
watch, open **UHC → Settings → Connect HiPhi Cloud** and follow the owner-bound
enrollment flow. Never port-forward UHC's trusted-LAN HTTP port.
