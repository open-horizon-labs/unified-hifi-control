# macOS Apple Music companion host scaffold

The repository now includes a buildable `AppleMusicCompanionApp` executable
target. For a local signed bundle, run `./build-app.sh` from this directory;
it wraps the SwiftPM executable as a macOS `.app` and ad-hoc signs it. Set
`CODE_SIGN_IDENTITY` to an Apple Development or Developer ID identity when
using a real signing team. Enable the MusicKit App Service for the explicit
bundle ID in Certificates, Identifiers & Profiles; MusicKit is associated with
the App ID at runtime and does not require a custom entitlement key. For a
polished distribution target, create a macOS 14+ SwiftUI app in Xcode, add the
sibling package, and include these host files in the app target. The model
restores (or creates) its installation with
`KeychainAppleMusicCompanionInstallationStore`, so the companion ID survives
relaunches. The host flow is:

1. Request MusicKit authorization in the signed app.
2. Generate/claim the short-lived bridge code shown by UHC.
3. Poll and acknowledge transport commands while the app is active.
4. Publish `ApplicationMusicPlayerCompanion.snapshot()` on a foreground
   cadence.
5. Stop polling when suspended/terminated and let UHC classify the owner
   stale; revoke explicitly when removing the installation.

This is not a Music.app controller. `ApplicationMusicPlayer` is an app-private
session, and the package makes no AirPlay route or destination-control claim.
The repository has no macOS SDK/Xcode target, so local validation is syntax
only. Signing, Keychain behavior, authorization failures, background
lifecycle, and AirPlay-route observations require a physical Mac (#486/#487).
Keep the bridge behind HTTPS and an authenticated controller boundary until
#488 is implemented. The example deliberately uses a placeholder endpoint;
replace it during onboarding and do not ship that URL.
