# v4 HiPhi integration readiness — 2026-10-02

Tracking: [#751](https://github.com/open-horizon-labs/unified-hifi-control/issues/751). OH: 80222d6d.

## Aim and execution boundary

Preserve the owner's working v4 local and HiPhi Cloud experience while assembling a reviewable integration candidate. Start from current v4, integrate open work only with matching deployment/test evidence, correct concrete readiness defects, and make remaining feature and release gates explicit. Existing clients, authentication, local-first operation, package identifiers, volume safety, and the user's uncommitted checkout work are preservation constraints. No release tag, unattended firmware flash, or speculative provider-completeness claim is part of this pass.

The owner authorized merging open work already tested on 192.168.1.2 and requested Sol 6.1 workers. The owner subsequently instructed us to disable CodeRabbit. The owner subsequently approved the narrowed device-aware OTA contract: optional hardware/channel selection on the existing version/download routes, persisted device identity, and its device-list field. UHC browser flashing is explicitly excluded; local HTTP operation requires no HTTPS setup.

## Observed baseline

- Origin v4 initially: `e98d2c47` (settled Cloud state publication after playback commands).
- NAS `/status`: version `0.0.0-pr745`, git SHA `7a8ab0c`, uptime 343476 seconds at first observation. This exactly matches PR #745's head. The owner reports this installation, including HiPhi Cloud, works well.
- Runtime status reported Roon connected, HQPlayer disconnected, LMS disconnected, and no OpenHome/UPnP devices. This is a point-in-time connectivity observation, not evidence those providers are broken or unsupported.
- Registered hardware reports a mixture of `2.5.2`, `2.7.0-alpha.*`, and development versions. Existing `/knob/devices` has no device identity, so a version number cannot safely determine a hardware target.
- Original working checkout remains on v4 with its pre-existing Apple companion version edit and untracked Plex research document. Work is isolated in managed worktrees.
- PR #745 exact head has successful build, WASM, lint, test, API, QNAP and package-contract checks. Local v4 + #745 regression run: 122 passed, one live test ignored.
- PR #745 merged normally into v4 as `2d9222b2` after the owner's instruction to turn off CodeRabbit and dismissal of its two blocking reviews. The valid findings were independently reproduced and fixed in the integration follow-up.

## Integration work

| Area | Change | Evidence / remaining gate |
|---|---|---|
| Tested relay work | PR #745 integrated and merged to v4 | Deployed exact SHA, owner use, green CI, 122 passing local tests |
| Relay display stability | Artwork changes retain clock anchor; source changes emit metadata boundaries even when text matches; sole-source guidance corrected | Both new regressions failed first; 5 clock and 10 frame tests pass; audio preservation tests retained |
| HiPhi identity (#668) | Shared footer links “A HiPhi project” to hiphi.audio; accessible product name and README updated | Copy checks, strict Clippy, formatting; light/dark shared-footer preview and Settings navigation verified at `730c254e`; eight Cloud actions corrected to use the existing button styles and rebuilt at `c4db7b3e` |
| License explanation (#738) | README and INSTALL explain noncommercial use and commercial deployments consistently | Governing LICENSE text unchanged; no new legal grant asserted |
| Multi-firmware (#274–276) | Legacy OTA safety and immutable publication implemented; nine-family catalog prepared; approved device selectors, isolated target/channel refresh, persisted identity and device display implemented | See firmware section; no physical flashing performed |
| Reproducible CI (#340) | Pin a reviewed blocking Rust toolchain; preserve an advisory latest-stable signal | Local strict Clippy and 14 workflow contracts pass, including cache/filter mutation checks. Hosted CI exposed missing isolated Cargo tools on PATH; a failing-then-passing behavioral regression and shared setup fix cover paths containing spaces. PR #752 passed all blocking Actions, including fresh hosted WASM, Linux build and binary smoke checks |
| CodeRabbit | Owner requested disabling repo automation and removing blocking bot reviews | Blocking reviews dismissed; [PR #752](https://github.com/open-horizon-labs/unified-hifi-control/pull/752) merged as `eb3a6a7b` after all blocking checks passed; reviews/statuses/replies/issue automation disabled; organization-owned app removal still requires an owner |

QNAP and Synology package contracts also passed locally. The Mac standalone Tailwind binary was killed on launch, so generated CSS was built with the same Tailwind 4.1.18 Node CLI using temporary npm-cache dependencies. The downloaded Dioxus 0.7.10 archive matched its published SHA-256. Local bundling required `DYLD_LIBRARY_PATH` pointing to the Rust toolchain lib directory so rust-objcopy could load libLLVM; no system library paths were modified.

## CodeRabbit removal boundary

The owner explicitly requested disabling CodeRabbit. The configuration disables automatic and incremental reviews, blocking change requests, commit/check/review status publishing, labels, unsolicited replies, issue enrichment and planning. The two blocking reviews on #745 were dismissed and that PR merged. A later inventory found and dismissed 12 additional CodeRabbit change-request reviews on #291, #362 and #363; those PRs remain unmerged and still require independent validation. [CodeRabbit documents that each feature branch supplies its own YAML](https://docs.coderabbit.ai/getting-started/yaml-configuration), so this is effective on the prepared branches; older branches can still carry their prior configuration. PR #752 merged into v4 as `eb3a6a7b`; older branches must synchronize that configuration to inherit it. Complete app removal was attempted through GitHub and denied because it requires organization-owner permission. No alternate credential or permission bypass was attempted.

## Feature audit and release priority

| Surface | Current evidence | Remaining work / priority |
|---|---|---|
| Local control and hardware | Owner-tested current NAS; client and API regression harnesses exist | Preserve no-selector legacy Dial behavior and persisted configuration; verify each new hardware family before release |
| HiPhi Cloud pairing and reconnect | Owner-tested operation; issuer separation, grants/replay, epochs, stale-state refusal, opaque identifiers, and recovery tests exist | Re-run contracts on final integration; cloud server deployment/security is a separate system, not proved by bridge tests |
| Optional Cloud music details (#749/#750) | A read-only client and CLI in draft PR #750; native CI tests pass | CI lint/WASM failures, independent Cloud catalog deployment, installer bundling and consumer UI are unresolved; not validated by existing Cloud playback use |
| Roon seek (#740/#741) | Draft adds native absolute seek and observation matching; protocol tests documented | Exact-head physical Roon/firmware jog validation and explicit API-action approval before promotion; not deployed on observed NAS |
| Roon browse/library (#545/#573/#593/#616/#587) | Substantial browsing implementation exists; old issues may describe earlier builds | Reproduce current user journeys before changing session/ref behavior; do not infer fixed or broken solely from open issue state |
| No-op volume (#621) | Report of same-value writes waiting 15 seconds remains open | Reproduce with authoritative provider observation; do not “fix” with cached-state success, which can hide stale or wrong-target writes |
| Spotify | Existing direct control, pairing and onboarding paths; provider limitations recorded separately | Premium/credential copy (#665), Cloud setup simplification (#662), browse limitations (#473); new upstream provider access requires current verification |
| Apple Music companion | Companion code and transport bridge exist | Signed physical iPhone validation (#465), liveness/ownership and queue/content slices; generated capability matrix correctly retains pending status |
| Music Assistant | Adapter and provider slices are implemented in part | Validate connection diagnostics and modes/queues/multiroom against supported server versions; do not close old issues from code presence alone |
| HQPlayer/NAA | Relay/proxy and direct controls now present; #745 specifically tested locally | Identical successive items from the same source with identical metadata need a genuine playback epoch to mark boundaries. Position heuristics would confuse seeks with new tracks |
| LMS/OpenHome/UPnP | Existing adapters and contract tests | LMS live tests are ignored by default; HQPlayer conformance skips without host config; these do not constitute hardware validation. Provider capability gaps remain tracked in the generated matrix |
| Home Assistant | Integration/add-on workflows exist | Ingress/discovery/installation/distribution issues span separate repos (#581/#605/#613/#614); validate exact release packages, not just Rust unit tests |
| Premium updates/signing (#663/#561) | Documented future authority and packaging work | Signed manifests depend on Cloud work; signing/notarization and hardware update validation remain release gates, not hidden assumptions |
| Historical adaptive/voice work | Multiple open v3 and stacked PRs | Re-plan against current v4 and approved protocol contracts; no wholesale merge of old branches |

## Public hiphi.audio site audit

A fresh read-only pass checked the home page, controllers, onboarding, MCP, Cloud sign-in and Stable/Beta/Alpha firmware centers. The local/Cloud boundary is consistent with UHC: Cloud is labeled Alpha and local playback authority remains on the Bridge. The nine-controller catalog distinguishes channel and hardware limitations. Sample Tough manifests bind ESP32 while Dial/Kizz bind ESP32-S3; sample merged-binary URLs returned HTTP 200. This verifies published metadata and availability, not physical flashing.

Concrete site corrections: [the MCP page](https://hiphi.audio/mcp.html) still names v3 and exposes an unfinished screenshot placeholder; [getting started](https://hiphi.audio/getting-started.html) calls HA ingress planned despite the current beta add-on enabling it; [the firmware center](https://hiphi.audio/flash/) mentions Firefox while channel installers direct users to Chrome/Edge. Installation instructions also need an explicit stable/v4-beta choice because GitHub latest stable is v3.6.0 while the promoted streaming/Cloud line is v4.0.0-beta.1. The Docker `latest` tag must not be relabeled as v4 without verifying its published image. The copy corrections are prepared separately in [hiphi #38](https://github.com/open-horizon-labs/hiphi/pull/38), rebased and consolidated commit `3412a47` atop merged firmware worker PR #39. Its existing site check passes local links, all 13 manifests and firmware SHA-256 digests. A fresh GitHub read during OTA execution shows #38 merged as `4588b644` at 2026-10-02 14:15 UTC. This pass has not independently verified the resulting Pages deployment; the site source is not merged into UHC history.

## Dependency security audit

The captured GitHub inventory has 62 open dependency alerts: 6 in Cargo.lock (1 high, 1 medium, 4 low) and 56 in package-lock.json (12 high, 40 medium, 4 low). These counts describe repository entries, not confirmed reachable vulnerabilities in the installed NAS service. The Node lockfile still matters to the separately invoked stdio MCP bridge and browser/test tooling; it is not the Rust server runtime dependency tree.

The integration updates only the compatible rand patch lines, 0.8.5 → 0.8.6 and 0.9.2 → 0.9.3, for [RUSTSEC-2026-0097](https://rustsec.org/advisories/RUSTSEC-2026-0097.html). The advisory's reentrant custom logging conditions were not found in the inspected enabled features and source. The patch is still useful protection against future configuration changes. All 42 Cloud connector and six credential persistence regressions passed with the patch; strict Clippy and formatting also pass.

The old TLS branch remains tracked in [#753](https://github.com/open-horizon-labs/unified-hifi-control/issues/753): rustls-webpki 0.102.8 reaches v4 through rumqttc 0.24 and the older rustls/tokio-tungstenite chain, while reqwest already uses patched 0.103.13. [GHSA-82j2-j2ch-gfr8](https://github.com/rustls/webpki/security/advisories/GHSA-82j2-j2ch-gfr8) and [GHSA-pwjx-qhcg-rvj4](https://github.com/rustls/webpki/security/advisories/GHSA-pwjx-qhcg-rvj4) require CRL verification; no application CRL configuration was found in the inspected paths. The distinct low-severity name-constraint advisories [GHSA-965h-392x-2mh5](https://github.com/advisories/GHSA-965h-392x-2mh5) and [GHSA-xgp8-3hg3-c2mh](https://github.com/advisories/GHSA-xgp8-3hg3-c2mh) remain conditional certificate-chain risks. No compatible patched 0.102 release is listed. Resolving this requires reviewed parent dependency upgrades/backport and transport validation, not a forged Cargo.lock version or disabled certificate verification.

The npm dependency PRs remain separate candidates. This pass does not claim all their SDK/server advisory paths are retired or all current package users are unaffected.

## Firmware findings and approved contract

The current service polls the stable release of `muness/roon-knob` and expects `roon_knob.bin`. Stable `v2.5.2` only contains the legacy Dial release. Prerelease `v2.7.0-alpha.5` has application and merged artifacts for nine current families: Dial, Frame, Tough, Joy, RLCD, M5 Dial, StickS3, StopWatch, and StackChan/Kizz. These are not interchangeable; Tough uses ESP32 while the others use ESP32-S3.

Before this pass, the download route could fall back to the first arbitrary `.bin`, and the browser manifest pointed an application image at offset zero. Both defects were reproduced and corrected. The legacy OTA routes now require a valid matching Dial application image; the browser clean-install manifest returns `404`; USB browser flashing belongs exclusively at [firmware.hiphi.audio](https://firmware.hiphi.audio). Error envelope keys and error_code remain stable; error prose is consolidated. The catalog distinguishes application and merged images and rejects unknown aliases. Actual legacy routes reject cross-target filenames, traversal, directory/metadata/image symlinks, empty or missing images, and filename/version mismatches. They continue reading the existing poller root rather than allowing a stale new subdirectory to shadow it. The polling service now publishes immutable versioned files before atomically switching version.json; unique staging files, identical retry checks, a process-wide publication lock, and no-downgrade checks preserve an old consistent bundle when a new publication fails. This is process/interruption consistency, not a claim of sudden-power-loss durability or atomicity across two separate HTTP requests. Device-aware routing is implemented under the owner’s subsequent approval.

The request examples and compatibility boundary are documented in [Local controller OTA](firmware-ota.md). The approved contract adds optional `device_type` selection to existing `/firmware/version` and `/firmware/download`, accepts the firmware client's `X-Device-Type` identity header, persists canonical device identity and exposes `device_type` in `/knob/devices`. Explicit prerelease selection is opt-in; stable remains the default. No new route or HTTP method is needed. The manifest route stays unavailable, and no merged-image download selector is added. Omitted headers must not erase known identity; aliases must normalize to the exact hardware target.

Fresh firmware inspection supersedes the initial header finding: `/Users/muness1/src/hiphi-repos/roon-knob-integration` at `e1872dfa` sends `X-Device-Type` on direct Dial OTA requests. Its OTA image validation still names Dial specifically. Bridge support alone therefore does not prove OTA on every family; exact firmware artifacts and physical hardware remain release gates. The old #274 suggestion to disable Frame deep sleep is not carried forward: current `frame_app/main/frame_power_manager.c` consumes `deep_sleep_battery_enabled` and its timeout. Identity discovery preserves existing configuration rather than inventing new power defaults. No firmware was flashed during this audit.

### Approved OTA execution checklist

Aim: make the Bridge select and cache the exact controller application image while preserving existing local HTTP clients. The chosen approach uses the existing hardware catalog and routes because target identity must be explicit and browser USB installation already has a canonical HTTPS host. Scope includes device identity, isolated target/channel caches, version/download selection, fetch/poll integration, and device display. Stable is the default; missing target artifacts are an explicit unavailable result.

- Preserve no-selector legacy `knob` behavior, existing firmware response keys/error envelope and route methods; a test must fail a shortcut that changes every default to the modern Dial artifact.
- Persist canonical hardware identity with old-record defaults; test restart/serialization, aliases, invalid identity, and an omitted header after known identity. These checks must fail an implementation that resets all reconnects to `knob`.
- Reject unknown hardware, cross-target and wrong-channel fallback; test distinct bytes and independent metadata. These checks must fail a shared cache or first-`.bin` choice.
- Keep stable isolated from beta/alpha, require explicit prerelease selection, and test upstream selection plus missing assets. These checks must fail blindly using the newest release for every caller.
- Retain immutable publication, no downgrade, traversal/symlink rejection, and failed-fetch preservation. Tests must fail direct overwrite of the active bundle.
- Keep UHC browser flashing excluded: manifest remains unavailable and there is no merged-image selector. HTTPS belongs to the external firmware host.
- Review actual diffs, run integrated firmware/identity/API and relevant client checks, then publish new commits to the integration branch. The owner supplies `api-change-approved` on the PR under repository policy; the agent does not apply it.

Dissent: the strongest contrary evidence is that modern stable artifacts may not exist and firmware implementations differ by hardware. Availability must therefore be separate from support: never silently promote alpha or infer hardware from firmware version. Another failure is a firmware client using only headers while handlers read only query parameters; direct-client tests must cover header selection. The weakest remaining assumption is physical image compatibility, which source/unit tests cannot retire. Accepted with rationale: physical flashing is outside this Mac test pass and requires owner hardware validation. Two separate HTTP requests remain non-atomic across a publication; retained immutable artifacts do not by themselves pin the client's second request. Stop/pivot if correct release assets cannot be identified or a proposed shortcut requires unsafe fallback, new browser flashing, or an unapproved route/schema.


## Verification and risk retirement

| Risk / tempting wrong fix | Required falsifying check | Status |
|---|---|---|
| Treat successful local use as approval for every open PR | Compare live SHA with each PR head and separate observed from untested code | Retired for #745 only; remaining PR disposition below |
| Artwork refresh resets playback time | Same track/seek, changed image key must retain extrapolation | Retired: failing then passing regression |
| Same metadata from another source misses a track boundary | Switch source ID without changing text; require META and unchanged audio | Retired: failing then passing regression |
| Guess a playback epoch from seek position | Retain backward seek tests and document unobservable identical entries | Accepted boundary: provider epoch absent; no heuristic added |
| Serve another device's image or an app image at flash offset zero | Cross-target, missing image, traversal, legacy and app/merged route tests | Retired for existing legacy routes: behavioral red/green tests reject wrong/missing/empty/symlink/escaping artifacts and conflicting versions; unsafe clean-install manifest fails closed |
| Quietly move stable firmware users to alpha | Channel-selection and independent per-target metadata tests | Retired: explicit channel selection, independent metadata/bytes, wrong-channel rejection, and mocked upstream refresh tests pass; stable remains default |
| New compiler makes unchanged code fail | Pinned strict gate plus separate visible latest-stable advisory | PR #752 blocking tests, lint, fresh hosted WASM, Linux artifact and smoke test all passed; latest-stable lint remains a visible nonblocking failure |
| Brand polish changes product/API identity or license | Preserve UHC, IDs/routes, existing LICENSE; API contract | Source checks and final API contracts pass; shared footer observed in light and dark themes |
| Claim a server-only build validates interactive UI | Build Dioxus WASM, then server with embedded current assets; fresh browser observation | Dioxus 0.7.10 WASM and native builds passed on Rust 1.98.1; immutable runtime SHA `730c254e`, shared branding and Settings navigation verified. Styling follow-up builds/runtime SHA `c4db7b3e` also passed |
| Treat skipped live-provider tests as passed | Explicitly report ignored/host-gated tests and physical checks | Retired by evidence reporting; physical checks remain release gates |

## Delivered validation

- Relay baseline: 122 passed, one live-device test ignored. Follow-ups: 5 clock and 10 frame tests passed, with the new failures observed before fixes.
- Firmware: 12 catalog/publication library tests, the comprehensive real-route safety scenario, and 2 public API contract tests passed. The wrong-device fallback, old-metadata/new-bytes race, invalid version, stale storage precedence, and version mismatch all failed before correction. Both logical changes passed strict Clippy and formatting and received independent review.
- Dependency patch: 42 Cloud connector and 6 credential persistence tests passed.
- CI: 14 workflow contracts pass; adverse toolchain, CSS, advisory, cache-key and path-filter mutations are rejected. The actual shared setup shell blocks also pass a behavioral regression proving that tools in a Cargo path containing spaces are executable in the next workflow step. Hosted integration CI first exposed this defect as `dx: command not found` after a successful installation; it is corrected. PR #752 at `69c72884` then passed all blocking Actions, including fresh hosted WASM, Linux packaging and binary smoke tests, and merged into v4.
- Package contracts: QNAP and Synology passed locally.
- Combined WASM and native builds passed at `730c254e`, followed by all 17 final firmware/API/workflow contract tests. A copied immutable executable reported the same SHA at `/status` and in a fresh browser. The shared footer rendered in light and HiPhi Dark themes, and client navigation reached Settings. A subsequent eight-button styling correction passed another combined WASM/native build and immutable runtime SHA check at `c4db7b3e`. A second fresh visual check of that last styling correction remains unverified because the browser automation reported an ambiguous tab binding. Earlier preview evidence from a shared-target executable overwritten by another worker was excluded.

## Review and completion gates

The frame remains a readiness integration pass, not completion of every provider roadmap epic. The first implementation pass targets defects supported by current evidence. Expanding into new service providers, paid capability policy, or old adaptive protocol schemas would need a new bounded decision.

Before release: green final-head CI, target QNAP installation and owner playback check, device-specific OTA and external firmware-site clean-install checks, remaining mobile/ingress and complete theme/state visual checks, and the independent Cloud/Apple signing gates that apply to the selected release scope. A draft integration PR is a review artifact, not evidence these gates passed.

## Execute — approved device-aware OTA

**Task:** implement the owner-approved local HTTP OTA selection and identity contract on the integration branch. **Aim achieved:** the Bridge selects exact application firmware by controller and explicitly selected release channel without requiring UHC HTTPS or browser flashing.

### Declared success criteria and delivered characteristics

- Existing route paths/methods, legacy no-selector OTA behavior, version payload keys and device-list wrapper are preserved: met by the API and client harness plus actual native-server HTTP smoke.
- Canonical identity survives legacy reconnects and old-record migration without resetting configuration: met by store and real HTTP restart tests.
- Target/channel selection, exact upstream asset choice, stable default, draft exclusion and numeric prerelease ordering are explicit: met by catalog, real handler and mocked upstream tests.
- Manual refresh and automatic polling update available caches independently; failed fetches preserve prior usable images: met by the mocked upstream scenario and immutable publication tests.
- Device display consumes the additive identity field while older responses remain usable: met by two UI compatibility tests and a successful WASM client build.
- Browser installation is external; no UHC manifest/image selector is introduced: met by route inspection and the native manifest `404` check.

### Changes and verification

`src/firmware.rs` and the shared catalog now refresh isolated hardware/channel caches from exact published application assets. Existing version/download handlers accept approved query/header selection and reject invalid or conflicting identities. The store and controller handlers persist canonical hardware identity, and the controller page displays its public name and accurately describes cache refresh rather than flashing.

The HTTP restart scenario was moved into the single-test identity executable so parallel client harness tests cannot overwrite its temporary disk state. The normal parallel client harness passes 83/83 and the combined isolated identity scenario passes 1/1. Verification: 88 combined HTTP/client tests passed (83 client harness, two route contracts, one legacy artifact safety scenario, one multi-target/channel scenario and one identity persistence scenario). Fifteen firmware/catalog/publication tests, the canonical header unit test and two UI wire compatibility tests passed. The device/header route and catalog regressions were observed red before their fixes; a controlled restoration of the prior single-target stable-only refresh also made the mocked upstream test fail, and restored full behavior passed. Production strict Clippy (`cargo clippy -- -D warnings`), formatting and diff checks pass. An additional all-targets lint attempt exposed the repository's existing test unwrap/expect lint debt; it was not used to expand this change or loosen production lint.

Dioxus 0.7.10 release WASM and the native server with embedded current assets built successfully. The compiled native server was run with temporary configuration, disabled providers and synthetic firmware fixtures on a loopback HTTP port. Legacy download, header-selected Frame download, explicit alpha, missing beta, invalid/conflicting selection, unavailable manifest and device registration/list identity all passed. The owned process stopped afterward. This smoke establishes the real binary/router behavior, not physical image compatibility. No NAS replacement or hardware flash occurred.

### Risk retirement

| Risk / assumption | Status | Tempting patch falsified | Evidence / boundary |
|---|---|---|---|
| Old clients receive modern Dial or another device image | Retired | Replace the default with newest asset or first binary | Legacy HTTP scenario, distinct target bytes, native server smoke |
| Known hardware is erased on a later legacy request | Retired | Default every omitted header to knob | Store and real HTTP reconnect/restart scenarios |
| Alpha enters stable implicitly or via incomplete metadata | Retired | Share cache/metadata or trust filename without channel checks | Explicit channel tests, missing-version and wrong-channel rejection, mock release selection |
| One unavailable/failing family stops usable updates | Retired | Abort the whole refresh on the first missing asset/download | Mock upstream missing assets and partial HTTP failure with preserved Frame/new Tough bytes |
| Publication overwrites active bytes or downgrades alpha counters | Retired | Mutable app filename or lexical counter comparison | Immutable publication, retry/no-downgrade and alpha `.10` versus `.2` tests |
| Symlink or escaping metadata redirects a cache | Retired | Check only the final filename | Root/channel/target guards and legacy/multi-device adversarial route tests |
| Device-list addition breaks older UI responses | Retired | Require a new field unconditionally | Optional UI field deserialization tests and WASM build |
| Bridge changes imply full end-to-end OTA | Accepted with rationale | Claim server tests prove all firmware families can flash | Physical devices, firmware-side image/geometry checks and exact build validation are outside this Mac test pass |
| Artifact naming proves authenticity or abrupt power-loss safety | Accepted with rationale | Treat a matching filename or metadata rename as attestation/durability | Signing is a separate release gate; directory fsync and separate-request version pinning are not added |

### Review and human verification

Final independent read-only review: Continue; no additional critical finding. The implementation remains within the approved local OTA boundary. Hardware flashing/boot recovery, a client that explicitly requests prereleases, final-head hosted CI and NAS playback remain external verification gates. Today's modern Dial firmware requests stable through its identity header; modern stable assets are not yet published, so `404` is intentional rather than alpha fallback. The owner must add `api-change-approved` to #754 under repository policy; the agent has not applied it. The final device-row visual appearance was not independently observed in a fresh browser; wire compatibility, existing text styles and compiled client behavior were checked.

## Open PR disposition at audit start

CI failures below mean failures visible in the captured check history; repeated/cancelled runs must be resolved to the latest head before promotion. “Not locally evidenced” is not a claim the code is bad.

| PR | Base | Disposition | Title |
|---|---|---|---|
| [#750](https://github.com/open-horizon-labs/unified-hifi-control/pull/750) | `v4` | Draft: optional undeployed catalog/CLI; lint and WASM failures | Read optional music context through an existing HiPhi pairing |
| [#748](https://github.com/open-horizon-labs/unified-hifi-control/pull/748) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump ip-address and express-rate-limit |
| [#747](https://github.com/open-horizon-labs/unified-hifi-control/pull/747) | `v4` | Not locally evidenced; CI failures: Build Linux x64, Compile connector and pairing helper, Connector contracts and zero-trust boundary, Format, lint, and test, Lint, Test | chore(deps): bump the tokio group across 1 directory with 5 updates |
| [#746](https://github.com/open-horizon-labs/unified-hifi-control/pull/746) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump @modelcontextprotocol/sdk from 1.29.0 to 1.30.1 |
| [#745](https://github.com/open-horizon-labs/unified-hifi-control/pull/745) | `v4` | Merged: exact NAS SHA + owner testing + successful CI | Fix HQPlayer relay setup, status, and metadata flicker |
| [#741](https://github.com/open-horizon-labs/unified-hifi-control/pull/741) | `v4` | Draft: real Roon seek test pending | feat: Roon seek support (control_roon, RoonAdapter, observation) |
| [#728](https://github.com/open-horizon-labs/unified-hifi-control/pull/728) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump hono from 4.11.4 to 4.13.7 |
| [#727](https://github.com/open-horizon-labs/unified-hifi-control/pull/727) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump tracing-subscriber from 0.3.22 to 0.3.23 in the tracing group across 1 directory |
| [#726](https://github.com/open-horizon-labs/unified-hifi-control/pull/726) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump the serde group across 1 directory with 2 updates |
| [#725](https://github.com/open-horizon-labs/unified-hifi-control/pull/725) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump the tower group across 1 directory with 2 updates |
| [#724](https://github.com/open-horizon-labs/unified-hifi-control/pull/724) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump actions/setup-python from 5 to 7 |
| [#722](https://github.com/open-horizon-labs/unified-hifi-control/pull/722) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump actions/github-script from 8 to 9 |
| [#721](https://github.com/open-horizon-labs/unified-hifi-control/pull/721) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump actions/setup-node from 6 to 7 |
| [#720](https://github.com/open-horizon-labs/unified-hifi-control/pull/720) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump docker/setup-buildx-action from 3 to 4 |
| [#719](https://github.com/open-horizon-labs/unified-hifi-control/pull/719) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump dioxus-primitives from `ccdb07f` to `9a75825` in the dioxus group across 1 directory |
| [#718](https://github.com/open-horizon-labs/unified-hifi-control/pull/718) | `v4` | Not locally evidenced; independently validate before merge | chore(deps-dev): bump @playwright/test from 1.59.1 to 1.63.0 |
| [#697](https://github.com/open-horizon-labs/unified-hifi-control/pull/697) | `v4` | Superseded/overlapping fleet work; compare with merged #735 before closure | Use isolated Tart VM for trusted macOS CI |
| [#689](https://github.com/open-horizon-labs/unified-hifi-control/pull/689) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump fast-uri from 3.1.0 to 3.1.7 |
| [#688](https://github.com/open-horizon-labs/unified-hifi-control/pull/688) | `v4` | Not locally evidenced; independently validate before merge | chore(deps): bump qs from 6.15.0 to 6.16.0 |
| [#505](https://github.com/open-horizon-labs/unified-hifi-control/pull/505) | `v3` | Historical/stacked work; not part of tested v4 deployment | Voice: route Kizz speech through Codex MCP |
| [#502](https://github.com/open-horizon-labs/unified-hifi-control/pull/502) | `v3` | Historical/stacked work; not part of tested v4 deployment | Represent HiPhi hardware as distinct controller types |
| [#393](https://github.com/open-horizon-labs/unified-hifi-control/pull/393) | `feat/issue-328-direct-hqplayer-zone` | Historical/stacked work; not part of tested v4 deployment | Adaptive control: one producer model, surface-appropriate projections (#331) |
| [#385](https://github.com/open-horizon-labs/unified-hifi-control/pull/385) | `v3` | Historical/stacked work; not part of tested v4 deployment | docs: remove obsolete agent workflow guidance |
| [#380](https://github.com/open-horizon-labs/unified-hifi-control/pull/380) | `v3` | Historical/stacked work; not part of tested v4 deployment | Workflow: allow safe rebases of owned feature branches |
| [#363](https://github.com/open-horizon-labs/unified-hifi-control/pull/363) | `feat/issue-323-adaptive-producer-contract` | Historical/stacked work; not part of tested v4 deployment | feat(#324): publish adaptive producer documents through the bus and aggregator |
| [#362](https://github.com/open-horizon-labs/unified-hifi-control/pull/362) | `fix/issue-338-rust-1-97-lint` | Historical/stacked work; not part of tested v4 deployment | feat(#323): adaptive-control producer document v1 and compatibility policy |
| [#339](https://github.com/open-horizon-labs/unified-hifi-control/pull/339) | `v3` | Historical/stacked work; not part of tested v4 deployment | fix(ci): restore the v3 lint baseline under Rust 1.97 (#338) — Stage 1 analysis |
| [#291](https://github.com/open-horizon-labs/unified-hifi-control/pull/291) | `v3` | Historical/stacked work; not part of tested v4 deployment | feat: command-pattern manifest v2 — per-element behavior declarations |

## Open issue inventory

This inventory preserves the full backlog captured during the audit; listing an issue does not confirm its original defect still reproduces. Domain priorities and concrete evidence are above.

- [#749](https://github.com/open-horizon-labs/unified-hifi-control/issues/749) — Read optional music context through an existing HiPhi pairing
- [#744](https://github.com/open-horizon-labs/unified-hifi-control/issues/744) — Make HQPlayer relay setup and status clear; keep track metadata stable
- [#740](https://github.com/open-horizon-labs/unified-hifi-control/issues/740) — Add Roon seek support (control_roon has no seek action)
- [#738](https://github.com/open-horizon-labs/unified-hifi-control/issues/738) — Align license wording with hiphi-esp: noncommercial-only, Open Horizon Labs copyright
- [#717](https://github.com/open-horizon-labs/unified-hifi-control/issues/717) — Request Apple Music Companion TestFlight access
- [#716](https://github.com/open-horizon-labs/unified-hifi-control/issues/716) — Request Apple Music Companion TestFlight access
- [#698](https://github.com/open-horizon-labs/unified-hifi-control/issues/698) — Run native Linux builds on isolated local workers
- [#696](https://github.com/open-horizon-labs/unified-hifi-control/issues/696) — Run trusted macOS CI on the isolated Tart worker
- [#668](https://github.com/open-horizon-labs/unified-hifi-control/issues/668) — Identify Unified Hi-Fi Control as a HiPhi project
- [#665](https://github.com/open-horizon-labs/unified-hifi-control/issues/665) — Make Spotify Cloud onboarding state Premium and credential boundaries clearly
- [#663](https://github.com/open-horizon-labs/unified-hifi-control/issues/663) — Consume signed premium capability and update manifests safely
- [#662](https://github.com/open-horizon-labs/unified-hifi-control/issues/662) — Simplify Spotify setup to personal Client ID through HiPhi Cloud
- [#661](https://github.com/open-horizon-labs/unified-hifi-control/issues/661) — Activate and persist HiPhi pairing across supported UHC packages
- [#660](https://github.com/open-horizon-labs/unified-hifi-control/issues/660) — Add non-technical HiPhi Cloud onboarding to installed UHC
- [#658](https://github.com/open-horizon-labs/unified-hifi-control/issues/658) — Separate HiPhi session and command verification authorities
- [#651](https://github.com/open-horizon-labs/unified-hifi-control/issues/651) — Harden Spotify OAuth tunnel host verification and callback secrecy
- [#633](https://github.com/open-horizon-labs/unified-hifi-control/issues/633) — Restore packed Frame artwork on the streaming/HA Alpha line
- [#621](https://github.com/open-horizon-labs/unified-hifi-control/issues/621) — Volume: writing the value a zone already holds hangs 15s then 500s
- [#619](https://github.com/open-horizon-labs/unified-hifi-control/issues/619) — Companion: extract UHCKit and add an Apple Watch controller
- [#616](https://github.com/open-horizon-labs/unified-hifi-control/issues/616) — Roon: stale item_key errors on Library — we mint a new browse session per request
- [#614](https://github.com/open-horizon-labs/unified-hifi-control/issues/614) — Distribution: one-click install of the UHC integration for non-add-on users (HACS default listing)
- [#613](https://github.com/open-horizon-labs/unified-hifi-control/issues/613) — HA add-on should install the UHC integration itself (no MQTT, no manual copy)
- [#610](https://github.com/open-horizon-labs/unified-hifi-control/issues/610) — HA add-on: 'auto-configure' stops short — HA's own MQTT integration must still be added by hand
- [#607](https://github.com/open-horizon-labs/unified-hifi-control/issues/607) — MQTT status reports running=true while the broker is unreachable
- [#605](https://github.com/open-horizon-labs/unified-hifi-control/issues/605) — HA add-on: auto-wire MQTT from the Supervisor so zones appear as entities without manual setup
- [#597](https://github.com/open-horizon-labs/unified-hifi-control/issues/597) — Spotify onboarding: state-aware stepper with progressive action disclosure
- [#596](https://github.com/open-horizon-labs/unified-hifi-control/issues/596) — Zones strip: full design pass — unified grid, vertical control rail, elevated armed header
- [#594](https://github.com/open-horizon-labs/unified-hifi-control/issues/594) — Zones strip mobile polish: clipped armed-zone art, cramped mini-zone rows
- [#593](https://github.com/open-horizon-labs/unified-hifi-control/issues/593) — Roon: albums at the Artists level carry no play ref (can't Play an album)
- [#592](https://github.com/open-horizon-labs/unified-hifi-control/issues/592) — Spotify tunnel: real OAuth redirect hits ERR_CONNECTION_RESET at the pinggy URL
- [#588](https://github.com/open-horizon-labs/unified-hifi-control/issues/588) — Design: owner-auth claim UX that never requires server-log access
- [#587](https://github.com/open-horizon-labs/unified-hifi-control/issues/587) — Roon My Live Radio shows empty despite stations existing (stale session or over-filtering)
- [#585](https://github.com/open-horizon-labs/unified-hifi-control/issues/585) — Zones strip picker: per-zone now-playing, play state, pause, and volume nudge
- [#584](https://github.com/open-horizon-labs/unified-hifi-control/issues/584) — HA distribution: license detection fix, brands PR, my-home-assistant badges
- [#581](https://github.com/open-horizon-labs/unified-hifi-control/issues/581) — HA Add-on Tier 2: Ingress support (UHC UI embedded in the HA dashboard)
- [#573](https://github.com/open-horizon-labs/unified-hifi-control/issues/573) — Library UI defect audit — live crawl findings (integration build)
- [#572](https://github.com/open-horizon-labs/unified-hifi-control/issues/572) — Build provenance: truthful git_sha in /status + CI guard for exactly one client bundle
- [#570](https://github.com/open-horizon-labs/unified-hifi-control/issues/570) — Controller-auth 401s must route users into the bootstrap flow, not raw HTTP errors
- [#566](https://github.com/open-horizon-labs/unified-hifi-control/issues/566) — Search results are dead ends: no browse paths from hifi_search; category rows inert
- [#561](https://github.com/open-horizon-labs/unified-hifi-control/issues/561) — Release signing epic: QPKG, DMG notarization, GPG/cosign, Windows (deferred)
- [#557](https://github.com/open-horizon-labs/unified-hifi-control/issues/557) — Library page render/fetch loop pins the browser; Roon zones report browse_supported=false
- [#554](https://github.com/open-horizon-labs/unified-hifi-control/issues/554) — Fix roon_protocol config-dir race: concurrent tests lose pairing-state writes
- [#550](https://github.com/open-horizon-labs/unified-hifi-control/issues/550) — Library-first UI overhaul: browse becomes the home page, zones become the play-target strip
- [#548](https://github.com/open-horizon-labs/unified-hifi-control/issues/548) — Settings migration: zone hide list lost upgrading from v3 release to integration build
- [#545](https://github.com/open-horizon-labs/unified-hifi-control/issues/545) — Roon browse UX: infinite playlist nesting, missing Play buttons, wrong action matching
- [#543](https://github.com/open-horizon-labs/unified-hifi-control/issues/543) — MCP admin plane: settings, adapter toggles, and integration setup via agent
- [#538](https://github.com/open-horizon-labs/unified-hifi-control/issues/538) — Spotify onboarding: built-in HTTPS tunnel for the OAuth callback
- [#531](https://github.com/open-horizon-labs/unified-hifi-control/issues/531) — Implement hifi_collections for LMS, Roon, Spotify, and Apple Music (complete the provider slices)
- [#520](https://github.com/open-horizon-labs/unified-hifi-control/issues/520) — Zone-setting and control POST routes are open to cross-origin writes
- [#504](https://github.com/open-horizon-labs/unified-hifi-control/issues/504) — Voice: add LAN-only Codex realtime provider for Kizz
- [#503](https://github.com/open-horizon-labs/unified-hifi-control/issues/503) — Brand UHC as Unified Hi-Fi Control by Open Horizon Labs
- [#501](https://github.com/open-horizon-labs/unified-hifi-control/issues/501) — Represent HiPhi controllers by their actual device type
- [#500](https://github.com/open-horizon-labs/unified-hifi-control/issues/500) — Modernize and own the QNAP QPKG builder toolchain
- [#494](https://github.com/open-horizon-labs/unified-hifi-control/issues/494) — Add Music Assistant repeat, shuffle, and multiroom controls
- [#493](https://github.com/open-horizon-labs/unified-hifi-control/issues/493) — Add Music Assistant queue read and mutation controls
- [#492](https://github.com/open-horizon-labs/unified-hifi-control/issues/492) — Add Music Assistant browse, saved playlists, and favorites
- [#490](https://github.com/open-horizon-labs/unified-hifi-control/issues/490) — Add secure Music Assistant connection setup and diagnostics
- [#488](https://github.com/open-horizon-labs/unified-hifi-control/issues/488) — Define UHC installation/controller authentication boundary
- [#487](https://github.com/open-horizon-labs/unified-hifi-control/issues/487) — Wave 2: Model AirPlay routes truthfully for Apple Music companions
- [#486](https://github.com/open-horizon-labs/unified-hifi-control/issues/486) — Wave 2: Validate and add a native Mac Apple Music companion
- [#485](https://github.com/open-horizon-labs/unified-hifi-control/issues/485) — Expose Apple Music feedback and adaptation primitives to MCP
- [#484](https://github.com/open-horizon-labs/unified-hifi-control/issues/484) — Add Apple Music playlist and library mutation through MCP with explicit safeguards
- [#483](https://github.com/open-horizon-labs/unified-hifi-control/issues/483) — Add Apple Music queue construction and truthful listening-plan control to MCP
- [#482](https://github.com/open-horizon-labs/unified-hifi-control/issues/482) — Expose Apple Music library, playlists, and personalized retrieval to MCP
- [#481](https://github.com/open-horizon-labs/unified-hifi-control/issues/481) — Add Apple Music catalog search and exact MCP play-by-reference
- [#480](https://github.com/open-horizon-labs/unified-hifi-control/issues/480) — Connect the iPhone Apple Music companion to the adapter, bus, and aggregator
- [#479](https://github.com/open-horizon-labs/unified-hifi-control/issues/479) — Harden Apple companion pairing, liveness, and owner-scoped state
- [#478](https://github.com/open-horizon-labs/unified-hifi-control/issues/478) — Epic: Apple Music companions and MCP listening control
- [#477](https://github.com/open-horizon-labs/unified-hifi-control/issues/477) — Correct Spotify-aware MCP initialization and tool descriptions
- [#476](https://github.com/open-horizon-labs/unified-hifi-control/issues/476) — Add Spotify repeat and shuffle controls to MCP
- [#475](https://github.com/open-horizon-labs/unified-hifi-control/issues/475) — Add Spotify playlists and liked-library access to MCP
- [#474](https://github.com/open-horizon-labs/unified-hifi-control/issues/474) — Add Spotify queue support and truthful queue limitations to MCP
- [#473](https://github.com/open-horizon-labs/unified-hifi-control/issues/473) — Add Spotify catalog browse to MCP
- [#472](https://github.com/open-horizon-labs/unified-hifi-control/issues/472) — Add Spotify MCP search and exact play-by-reference
- [#471](https://github.com/open-horizon-labs/unified-hifi-control/issues/471) — Can't detect roon server...
- [#470](https://github.com/open-horizon-labs/unified-hifi-control/issues/470) — Root privileges required for installation under Synology DSM
- [#469](https://github.com/open-horizon-labs/unified-hifi-control/issues/469) — Build first-run streaming provider onboarding
- [#467](https://github.com/open-horizon-labs/unified-hifi-control/issues/467) — Add optional Music Assistant adapter
- [#466](https://github.com/open-horizon-labs/unified-hifi-control/issues/466) — Add direct Spotify Connect controller adapter
- [#465](https://github.com/open-horizon-labs/unified-hifi-control/issues/465) — Validate iPhone SystemMusicPlayer companion foundation
- [#464](https://github.com/open-horizon-labs/unified-hifi-control/issues/464) — Discover viable direct Amazon Music adapter access
- [#463](https://github.com/open-horizon-labs/unified-hifi-control/issues/463) — Define streaming-provider authorization and Apple bridge contract
- [#462](https://github.com/open-horizon-labs/unified-hifi-control/issues/462) — Epic: direct streaming-service adapters
- [#459](https://github.com/open-horizon-labs/unified-hifi-control/issues/459) — Controller: recover authoritative zone projection after provider lifecycle changes
- [#458](https://github.com/open-horizon-labs/unified-hifi-control/issues/458) — Accept unavailable battery level in /now_playing query
- [#445](https://github.com/open-horizon-labs/unified-hifi-control/issues/445) — Add cached monochrome artwork profiles for reflective displays
- [#444](https://github.com/open-horizon-labs/unified-hifi-control/issues/444) — Do not return successful mixed-staleness now-playing snapshots to hardware clients
- [#442](https://github.com/open-horizon-labs/unified-hifi-control/issues/442) — Expose provider-neutral seek and position scrubbing for Roon zones
- [#441](https://github.com/open-horizon-labs/unified-hifi-control/issues/441) — LMS: make polling and CLI one logical bus producer
- [#440](https://github.com/open-horizon-labs/unified-hifi-control/issues/440) — Reliable bus delivery and projection commit barriers
- [#438](https://github.com/open-horizon-labs/unified-hifi-control/issues/438) — Aggregator projections: remove direct adapter state and status reads from every surface
- [#437](https://github.com/open-horizon-labs/unified-hifi-control/issues/437) — Typed bus services: move browse, configuration, artwork, profiles, and instance operations behind the app boundary
- [#436](https://github.com/open-horizon-labs/unified-hifi-control/issues/436) — Architecture lint: deterministically inventory and reject surface-to-adapter bypasses
- [#434](https://github.com/open-horizon-labs/unified-hifi-control/issues/434) — [Epic] Make the in-app bus the only production adapter boundary
- [#430](https://github.com/open-horizon-labs/unified-hifi-control/issues/430) — LMS refs: on a real, fully-populated server, hifi_search may never take the durable Library path
- [#423](https://github.com/open-horizon-labs/unified-hifi-control/issues/423) — OpenHome adapter: a constant track URI freezes now-playing metadata (internet radio)
- [#418](https://github.com/open-horizon-labs/unified-hifi-control/issues/418) — MCP album art: bounded, explicitly-requested cover art behind a reference
- [#417](https://github.com/open-horizon-labs/unified-hifi-control/issues/417) — Test infrastructure: MockLmsServer has no search handler, so hifi_play's success path is untested
- [#415](https://github.com/open-horizon-labs/unified-hifi-control/issues/415) — LMS adapter: two dead parses left behind by #407 — artwork tag J, and playlist_cur_index as a string
- [#414](https://github.com/open-horizon-labs/unified-hifi-control/issues/414) — LMS adapter: no declared minimum LMS version, and the search artist loop key may differ on older servers
- [#409](https://github.com/open-horizon-labs/unified-hifi-control/issues/409) — Follow-ups from #394: stale mock workaround, and GET /mcp returns 500 where DELETE returns 404
- [#403](https://github.com/open-horizon-labs/unified-hifi-control/issues/403) — MCP for LMS player features: modes, saved playlists, favorites, sync, play-next
- [#402](https://github.com/open-horizon-labs/unified-hifi-control/issues/402) — MCP browse for LMS: walk the LMS library hierarchy, not a search shim
- [#401](https://github.com/open-horizon-labs/unified-hifi-control/issues/401) — MCP surface validation: docs, capability matrix, and end-to-end proof
- [#400](https://github.com/open-horizon-labs/unified-hifi-control/issues/400) — MCP queue: inspect the queue, play from it, and mutate it where LMS allows
- [#399](https://github.com/open-horizon-labs/unified-hifi-control/issues/399) — MCP browse: provider-neutral navigation contract with paging (Roon implementation)
- [#392](https://github.com/open-horizon-labs/unified-hifi-control/issues/392) — [Epic] MCP surface completeness: navigable, addressable, observable (additive)
- [#384](https://github.com/open-horizon-labs/unified-hifi-control/issues/384) — Agent guidance: remove obsolete superego, ba, wm, and force-push rules
- [#379](https://github.com/open-horizon-labs/unified-hifi-control/issues/379) — Workflow: allow safe rebases of owned feature branches
- [#375](https://github.com/open-horizon-labs/unified-hifi-control/issues/375) — HQPlayer: publish a coherent native adaptive-control document
- [#374](https://github.com/open-horizon-labs/unified-hifi-control/issues/374) — Adaptive control: remove the debug-only direct aggregator mutation seam
- [#372](https://github.com/open-horizon-labs/unified-hifi-control/issues/372) — Release builds cannot provide in-process worker panic recovery
- [#369](https://github.com/open-horizon-labs/unified-hifi-control/issues/369) — HQPlayer lifecycle: add restart-aware bounded backoff and worker self-healing
- [#368](https://github.com/open-horizon-labs/unified-hifi-control/issues/368) — HQPlayer: make runtime reconfiguration atomic with semantic operations
- [#361](https://github.com/open-horizon-labs/unified-hifi-control/issues/361) — Beta D: publish an installable adaptive interaction system test build
- [#360](https://github.com/open-horizon-labs/unified-hifi-control/issues/360) — MCP: expose adaptive controls, content, queues, and saved collections from shared models
- [#359](https://github.com/open-horizon-labs/unified-hifi-control/issues/359) — Content: manage provider playlists and UHC-owned listening programs truthfully
- [#358](https://github.com/open-horizon-labs/unified-hifi-control/issues/358) — Content: model observed playback sessions and capability-gated queue mutation
- [#357](https://github.com/open-horizon-labs/unified-hifi-control/issues/357) — Voice: route client sessions through Home Assistant Assist
- [#356](https://github.com/open-horizon-labs/unified-hifi-control/issues/356) — Voice: define and accept authenticated client voice sessions
- [#355](https://github.com/open-horizon-labs/unified-hifi-control/issues/355) — Voice: support a fully local Wyoming processing path
- [#354](https://github.com/open-horizon-labs/unified-hifi-control/issues/354) — [Epic] Provider-neutral client voice sessions and processing
- [#353](https://github.com/open-horizon-labs/unified-hifi-control/issues/353) — [Program] Adaptive interaction plane across UI, voice, content, and MCP
- [#352](https://github.com/open-horizon-labs/unified-hifi-control/issues/352) — HQPlayer Beta C: publish an installable adaptive-device test build
- [#351](https://github.com/open-horizon-labs/unified-hifi-control/issues/351) — HQPlayer Beta B: publish an installable verified-tuning test build
- [#350](https://github.com/open-horizon-labs/unified-hifi-control/issues/350) — HQPlayer Beta A: publish an installable direct-control test build
- [#349](https://github.com/open-horizon-labs/unified-hifi-control/issues/349) — HQPlayer: make currently advertised profile loading safe and verifiable
- [#348](https://github.com/open-horizon-labs/unified-hifi-control/issues/348) — HQPlayer: define evidence-acquisition and one-way source provenance guardrails
- [#347](https://github.com/open-horizon-labs/unified-hifi-control/issues/347) — HQPlayer: verify live setters and scope enumerations to the loaded chain
- [#346](https://github.com/open-horizon-labs/unified-hifi-control/issues/346) — HQPlayer: compose advanced matrix, EQ, crossfeed, and pipeline controls safely
- [#345](https://github.com/open-horizon-labs/unified-hifi-control/issues/345) — HQPlayer: expose observed signal path and engine-health diagnostics
- [#344](https://github.com/open-horizon-labs/unified-hifi-control/issues/344) — Adaptive control: narrow large option sets without hiding current or unknown choices
- [#343](https://github.com/open-horizon-labs/unified-hifi-control/issues/343) — Adaptive control: govern provenance and licensing for semantic control catalogs
- [#342](https://github.com/open-horizon-labs/unified-hifi-control/issues/342) — HQPlayer tuning workspace: separate immediate, staged, persistent, and preset intent
- [#341](https://github.com/open-horizon-labs/unified-hifi-control/issues/341) — HQPlayer: reconcile protocol evidence and retire contradictory assumptions
- [#340](https://github.com/open-horizon-labs/unified-hifi-control/issues/340) — CI: make the blocking Clippy baseline reproducible and keep latest-stable advisory
- [#338](https://github.com/open-horizon-labs/unified-hifi-control/issues/338) — CI: restore the v3 lint baseline under Rust 1.97
- [#336](https://github.com/open-horizon-labs/unified-hifi-control/issues/336) — [Epic] Content discovery, queues, playlists, and listening programs
- [#335](https://github.com/open-horizon-labs/unified-hifi-control/issues/335) — [Epic] Home Assistant adaptive controller experience
- [#332](https://github.com/open-horizon-labs/unified-hifi-control/issues/332) — HQPlayer adaptive control: validate the end-to-end compatibility and recovery matrix
- [#331](https://github.com/open-horizon-labs/unified-hifi-control/issues/331) — Adaptive control: make web, MCP, and control devices consume the shared producer model
- [#330](https://github.com/open-horizon-labs/unified-hifi-control/issues/330) — HQPlayer Embedded: persist configuration with backup, restart, readback, and recovery
- [#329](https://github.com/open-horizon-labs/unified-hifi-control/issues/329) — HQPlayer: apply live modes and settings with verified pending/error semantics
- [#327](https://github.com/open-horizon-labs/unified-hifi-control/issues/327) — Adaptive control: persist zone, producer, device, and matcher bindings by stable identity
- [#326](https://github.com/open-horizon-labs/unified-hifi-control/issues/326) — Adaptive control: resolve producer controls through data matchers into device manifests
- [#325](https://github.com/open-horizon-labs/unified-hifi-control/issues/325) — HQPlayer: produce declarative controls from discovered state and constraints
- [#324](https://github.com/open-horizon-labs/unified-hifi-control/issues/324) — Adaptive control: publish producer documents through the bus and aggregator
- [#323](https://github.com/open-horizon-labs/unified-hifi-control/issues/323) — Adaptive control: specify producer document v1 and compatibility policy
- [#322](https://github.com/open-horizon-labs/unified-hifi-control/issues/322) — HQPlayer: build an executable native-protocol conformance harness
- [#321](https://github.com/open-horizon-labs/unified-hifi-control/issues/321) — Expose content-selection intents for voice surfaces, starting with Home Assistant Assist
- [#320](https://github.com/open-horizon-labs/unified-hifi-control/issues/320) — Define normalized browse, favorites, playlist, and program-selection capabilities
- [#319](https://github.com/open-horizon-labs/unified-hifi-control/issues/319) — Map Home Assistant entities, areas, scenes, and services into adaptive controls
- [#318](https://github.com/open-horizon-labs/unified-hifi-control/issues/318) — Implement a Home Assistant connection, subscription, and action adapter
- [#314](https://github.com/open-horizon-labs/unified-hifi-control/issues/314) — Negotiate device capabilities and adaptive-control protocol versions
- [#313](https://github.com/open-horizon-labs/unified-hifi-control/issues/313) — [Epic] Deliver complete HQPlayer controls safely across UHC surfaces
- [#312](https://github.com/open-horizon-labs/unified-hifi-control/issues/312) — [Epic] Define and distribute the versioned adaptive-control contract
- [#311](https://github.com/open-horizon-labs/unified-hifi-control/issues/311) — [Epic] Establish a trustworthy HQPlayer producer lifecycle
- [#310](https://github.com/open-horizon-labs/unified-hifi-control/issues/310) — [Program] HQPlayer as the first full adaptive-control integration
- [#309](https://github.com/open-horizon-labs/unified-hifi-control/issues/309) — Feature: expose queue management tools (hifi_queue_list, hifi_queue_remove, hifi_queue_clear, hifi_play_from_here)
- [#308](https://github.com/open-horizon-labs/unified-hifi-control/issues/308) — Bug: hifi_search/hifi_play with source=tidal or source=qobuz always fails with "Search not found in TIDAL/Qobuz"
- [#304](https://github.com/open-horizon-labs/unified-hifi-control/issues/304) — Adding power on / power off commands to players
- [#290](https://github.com/open-horizon-labs/unified-hifi-control/issues/290) — feat: LLM manifest generation — command-pattern elements from natural language
- [#289](https://github.com/open-horizon-labs/unified-hifi-control/issues/289) — feat: manifest config web UI — command-pattern element editor
- [#288](https://github.com/open-horizon-labs/unified-hifi-control/issues/288) — feat: manifest v2 — command-pattern elements with per-element behavior declarations
- [#284](https://github.com/open-horizon-labs/unified-hifi-control/issues/284) — feat: paid tier upgrade/trial onboarding flow in web UI
- [#283](https://github.com/open-horizon-labs/unified-hifi-control/issues/283) — feat: LLM-driven device configurator — natural language → manifest generation
- [#282](https://github.com/open-horizon-labs/unified-hifi-control/issues/282) — feat: trial provisioning — automated time-limited JWT issuance
- [#281](https://github.com/open-horizon-labs/unified-hifi-control/issues/281) — feat: define tier feature matrix (free vs pro)
- [#278](https://github.com/open-horizon-labs/unified-hifi-control/issues/278) — feat: license-gated OTA and feature tiers for subscriber vs free devices
- [#277](https://github.com/open-horizon-labs/unified-hifi-control/issues/277) — chore: rename roon-knob → HiPhi Dial references in bridge codebase
- [#276](https://github.com/open-horizon-labs/unified-hifi-control/issues/276) — feat(ui): device-type-aware Knobs page — conditional settings and firmware per device class
- [#275](https://github.com/open-horizon-labs/unified-hifi-control/issues/275) — feat(firmware): multi-device OTA — serve correct firmware artifact per device type
- [#274](https://github.com/open-horizon-labs/unified-hifi-control/issues/274) — feat(knobs): add device_type field — extract from header, persist, branch config defaults
- [#258](https://github.com/open-horizon-labs/unified-hifi-control/issues/258) — ci: GitHub Actions pipeline for Pi image builds
- [#257](https://github.com/open-horizon-labs/unified-hifi-control/issues/257) — feat: First-run onboarding experience in web UI
- [#255](https://github.com/open-horizon-labs/unified-hifi-control/issues/255) — Roon SOOD discovery fails with "No route to host" on macOS with utun interfaces
- [#254](https://github.com/open-horizon-labs/unified-hifi-control/issues/254) — hifi_control: add mute/unmute/toggle_mute actions
- [#253](https://github.com/open-horizon-labs/unified-hifi-control/issues/253) — feat: push-primary UDP — bridge pushes state to knobs, replaces polling
- [#251](https://github.com/open-horizon-labs/unified-hifi-control/issues/251) — fix: LMS first-time setup broken — enabling in Settings doesn't trigger discovery
- [#249](https://github.com/open-horizon-labs/unified-hifi-control/issues/249) — fix: refactor LmsPlayer.state from String to PlaybackState enum
- [#248](https://github.com/open-horizon-labs/unified-hifi-control/issues/248) — feat: Configurable media line composition for knob manifest
- [#247](https://github.com/open-horizon-labs/unified-hifi-control/issues/247) — fix: Replace resize_exact with crop-to-fill for non-square album art
- [#246](https://github.com/open-horizon-labs/unified-hifi-control/issues/246) — Add configurable max volume limit per zone
- [#222](https://github.com/open-horizon-labs/unified-hifi-control/issues/222) — UX: Show 'Applying...' state when changing HQPlayer settings
- [#221](https://github.com/open-horizon-labs/unified-hifi-control/issues/221) — feat(mcp): improve response structure for AI agent clarity
- [#220](https://github.com/open-horizon-labs/unified-hifi-control/issues/220) — chore: cleanup docs/ directory
- [#209](https://github.com/open-horizon-labs/unified-hifi-control/issues/209) — HQPlayer MCP: Expose available options and additional controls
- [#208](https://github.com/open-horizon-labs/unified-hifi-control/issues/208) — HQPlayer: Support multiple instances (Desktop + Embedded)
- [#205](https://github.com/open-horizon-labs/unified-hifi-control/issues/205) — Refactor: Use PrefixedZoneId type in adapter public methods for compile-time enforcement
- [#199](https://github.com/open-horizon-labs/unified-hifi-control/issues/199) — Bridge: Remote MCP server support (SSE for mobile clients)
- [#179](https://github.com/open-horizon-labs/unified-hifi-control/issues/179) — Add APT repository for Debian/Raspberry Pi OS installation
- [#174](https://github.com/open-horizon-labs/unified-hifi-control/issues/174) — MCP volume_up/volume_down commands fail with 'Unknown action: vol_rel'
- [#162](https://github.com/open-horizon-labs/unified-hifi-control/issues/162) — HQPlayer: continuously observe state and recover through the managed adapter lifecycle
- [#145](https://github.com/open-horizon-labs/unified-hifi-control/issues/145) — LMS plugin: Remove leftover Bin/public folder from distribution
- [#142](https://github.com/open-horizon-labs/unified-hifi-control/issues/142) — bug: Knob stuck on 'Attempt 1 of 5' after power outage - one-way communication
- [#141](https://github.com/open-horizon-labs/unified-hifi-control/issues/141) — feat: Add /debug/snapshot endpoint for field diagnostics
- [#121](https://github.com/open-horizon-labs/unified-hifi-control/issues/121) — feat: Add display_config to now_playing response for device UI customization
- [#113](https://github.com/open-horizon-labs/unified-hifi-control/issues/113) — Arch Linux
- [#82](https://github.com/open-horizon-labs/unified-hifi-control/issues/82) — Validate the MCP still works / update as needed
