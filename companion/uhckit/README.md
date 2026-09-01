# UHCKit

UHCKit is UHC's public, platform-neutral Apple control client. It contains the
wire models, client behavior, and transport boundary shared by iPhone, iPad,
and Apple Watch. It deliberately contains no MusicKit or account-specific code.

## Remote Swift package

Consumers should pin an audited UHC commit and depend on the repository root:

```swift
.package(
    url: "https://github.com/open-horizon-labs/unified-hifi-control.git",
    revision: "<pinned-commit-sha>"
)
```

Then add the `UHCKit` product to the client target. Pinning a revision keeps the
wire contract and client implementation explicit; production clients must not
track a development branch. Once a release tag contains the root manifest,
consumers may use SwiftPM's `exact` version requirement instead.

The root manifest and this directory's manifest point at the same sources. No
source is copied between the public UHC client and downstream product shells.
The root manifest is intentionally a remote-consumption entry point; this
nested package remains the single test owner so its macOS test run does not
compile unrelated iOS-only products.

## Local development

Run the nested development package:

```sh
swift test --package-path companion/uhckit
```

The contract tests consume `tests/fixtures/uhckit_contract.json`, which the Rust
server contract suite also guards. CI separately builds the root `UHCKit`
target and a fresh detached checkout to verify the remote mapping.

Transport evidence and the unresolved direct-Watch-versus-phone-relay question
are documented in [Transport.md](Transport.md).
