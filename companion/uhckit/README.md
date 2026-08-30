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

The root manifest and this directory's manifest point at the same sources and
tests. No source is copied between the public UHC client and downstream product
shells.

## Local development

Run either package entry point:

```sh
swift test
swift test --package-path companion/uhckit
```

Both commands exercise the same contract tests against
`tests/fixtures/uhckit_contract.json`, which the Rust server contract suite also
guards.

Transport evidence and the unresolved direct-Watch-versus-phone-relay question
are documented in [Transport.md](Transport.md).
