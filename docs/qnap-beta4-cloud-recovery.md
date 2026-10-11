# QNAP beta.4 acceptance: artwork fill and HiPhi Cloud reconnect

This candidate combines #771 (artwork fill, including portrait viewports), #772 (Zensally subject-aware composition), and #769 (a readable Cloud access failure with owner-initiated reconnect). It retains the current v4 prepared builders. Download the x86_64 QPKG from the release PR's Build artifact `qnap-x86_64`, or the beta.4 release asset when published. Install through QNAP App Center → Install Manually, replacing the existing UHC package. Keep the existing configuration and pairing; do not uninstall first. The QPKG remains unsigned and may display QNAP's signature warning.

Before updating, retain the previous QPKG for rollback and make an owner-only backup of the package's `config` directory. The installation preserves that directory. Verify the downloaded package against the SHA256 supplied with the build. Open UHC at `http://<NAS>:8088`; Settings should show the installed version and commit. Reload the browser after the upgrade so the embedded UI is current.

## Healthy baseline

1. Open Settings → HiPhi Cloud and confirm the same installation is paired and comes online. Send a real Garmin/HiPhi Cloud playback or volume command and verify that the intended zone responds.
2. Check the Frame still fills its artwork area above the tiny strip. No Frame firmware update is needed. Zensally is embedded and supplies the default subject-aware crop even when `vips` is absent from the NAS service PATH. Libvips attention is a fallback; centered fill is used if both composition paths fail.
3. Keep UHC open on Zones as well as Settings. The shared Cloud status is refreshed about every ten seconds; there should be no persistent access-error notice during a healthy connection.

## Simulate changed file access, then reconnect

Run these commands in an administrator SSH shell on the NAS after the healthy baseline. This briefly interrupts Cloud connectivity. The configuration directory stays private; only group-readable access on the existing installation key is introduced. Do not print or copy the key's contents.

```sh
qpkg_dir=$(/sbin/getcfg unified-hifi-control Install_Path -f /etc/config/qpkg.conf)
cloud_dir="$qpkg_dir/config"
key_file="$cloud_dir/hiphi-installation.key"
test -d "$cloud_dir" && test -f "$key_file" && test ! -L "$key_file" || exit 1
key_hash_before=$(sha256sum "$key_file" | cut -d ' ' -f 1)
# Record the current replay epoch without changing it.
saved_epoch_before=$(cat "$cloud_dir/hiphi-relay-epoch" 2>/dev/null || true)
"$qpkg_dir/unified-hifi-control.sh" stop
chmod 0640 "$key_file"
"$qpkg_dir/unified-hifi-control.sh" start
```

Wait for UHC to start, reload its page, and allow ten seconds for status refresh. Both Settings and Zones should show **Remote control is disconnected**, explain that saved connection-file access changed and remote controls are unavailable, and offer **Reconnect**. Local zone controls should continue working. If controller authentication is enabled, use the signed-in browser session.

Click **Reconnect** once. The button should show **Reconnecting…** while busy. The key's permissions should return to 0600, the installation should remain paired, and the UI should show the actual connection state. An accepted reconnect request is not a claim that Cloud is online: if the relay is unavailable, the UI should explain that UHC is retrying. Once Cloud is online, repeat the real Garmin/HiPhi Cloud command.

```sh
stat -c '%a' "$key_file"  # Expected: 600
key_hash_after=$(sha256sum "$key_file" | cut -d ' ' -f 1)
test "$key_hash_before" = "$key_hash_after" && echo 'Pairing key unchanged'
# This value may legitimately increase after a new Cloud session. It must not be deleted or reset.
saved_epoch_after=$(cat "$cloud_dir/hiphi-relay-epoch" 2>/dev/null || true)
printf 'Replay epoch before=%s after=%s\n' "$saved_epoch_before" "$saved_epoch_after"
```

Restart UHC once more and confirm the repaired access and pairing survive. Do not delete the key, replay epoch, retry budget, or quarantine files as part of this test.

If reconnect fails, restore the test-only permission change and restart the service before investigating:

```sh
chmod 0600 "$key_file"
"$qpkg_dir/unified-hifi-control.sh" restart
```

Collect the visible message and UHC logs under the package's `logs` directory, with secrets redacted. A separate usage-limit pause must remain visible after access is repaired; it has its own saved cooldown and budget. Damaged or missing state should show a storage/access explanation and should not offer to erase or re-pair automatically. These destructive failure cases are covered in automated tests; do not corrupt a live NAS installation to exercise them.

## Pass criteria

The package installs over the existing version; Frame artwork still works; a changed-access notice appears outside Settings; Reconnect repairs 0600 access without changing the pairing key or resetting replay protection; the actual Cloud state is shown; a real remote command works again; and the repaired state survives restart. Automated tests cover concurrent reconnects, missing/damaged/linked/unrelated files, retained usage limits, and preserved retry history. A real NAS/Cloud/Garmin test is deployment acceptance and is not replaced by CI.

## Artwork crop acceptance

The default proportional artwork fill now uses Zensally UltraFace + MicroSalNet and zenlayout's Minimal crop solver. Both models are embedded in the server; the NAS does not download models or require an ONNX Runtime installation. Analysis is shared by decoded artwork content across zones, display sizes, and orientations. A bounded 32-entry process cache retains faces and saliency maps; eviction or a server restart can cause another analysis. Inference is serialized to bound working memory, and image conversion remains in the existing bounded blocking workers. Explicit fit preserves the complete cover with its existing mat.

Check the Crown cover that previously lost the woman's face on Frame. Confirm the new 800×450 fill keeps her face visible. Check another cover without a face, and check an Android display in portrait as well as landscape. Expect cropping to remove some of a square cover when filling a rectangular screen; multiple widely separated faces cannot always fit. If model initialization or crop geometry fails, UHC tries libvips attention and then centered fill. Pi 3 latency and memory remain hardware acceptance checks; the offline Mac measurements are not a Pi benchmark.
