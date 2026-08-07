# macOS Apple Music companion host scaffold

Create a signed macOS 14+ SwiftUI app, add the sibling
`AppleMusicCompanion` package, enable MusicKit, and give the app a stable
Keychain-backed companion ID. The host flow is:

1. Request MusicKit authorization in the signed app.
2. Claim the short-lived bridge code shown by UHC.
3. Poll and acknowledge transport commands while the app is active.
4. Publish `ApplicationMusicPlayerCompanion.snapshot()` on a foreground
   cadence.
5. Stop polling when suspended/terminated and let UHC classify the owner
   stale; revoke explicitly when removing the installation.

This is not a Music.app controller. `ApplicationMusicPlayer` is an app-private
session, and the package makes no AirPlay route or destination-control claim.
The repository has no macOS SDK/Xcode target, so local validation is syntax
only. Signing, Keychain persistence, authorization failures, background
lifecycle, and AirPlay-route observations require a physical Mac (#486/#487).
Keep the bridge behind HTTPS and an authenticated controller boundary until
#488 is implemented.
