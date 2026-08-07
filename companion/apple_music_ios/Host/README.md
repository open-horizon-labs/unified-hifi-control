# iPhone host scaffold

This folder is an example signed-host starting point, not a pre-signed app.
Create an iOS 17+ Xcode application, add the Swift package as a local
dependency, and include these host files in the app target. Replace the
placeholder UHC HTTPS URL and companion ID with values from setup; the ID must
be stable across relaunches (store it in Keychain).

The host flow is:

1. Register the single explicit bundle ID
   `com.openhorizonlabs.uhc.applemusiccompanion.ios` under the signing team in
   Certificates, Identifiers & Profiles, enable the **MusicKit** App Service,
   and include `Info.plist` here. MusicKit manages both token lifecycles inside
   the signed app; no custom MusicKit entitlement is required. A subscription
   alone is not enough for catalog/library requests.
2. Authorize Apple Music in the app.
3. Generate a short-lived pairing code from the app or UHC, then claim it.
4. Keep command polling in the foreground and publish the bounded
   `SystemMusicPlayer` snapshot projection.
5. Stop polling when suspended; UHC should report the companion stale rather
   than implying an always-on iPhone service.

The package provides `KeychainAppleMusicCompanionInstallationStore` for the
stable companion identity, UHC URL, bridge ID, and paired bridge bearer. The
host app still owns signing, entitlements, and any polished QR onboarding
flow. This repository currently has command-line tools only, so host
validation here is syntax-only. Keychain behavior, signing, and the MusicKit
observation matrix require Xcode and a physical iPhone (#465).
