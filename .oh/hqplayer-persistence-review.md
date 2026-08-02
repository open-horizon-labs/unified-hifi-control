# Review: HQPlayer named-profile persistence

## Review summary

**Aim:** Make HQPlayer settings profiles selectable from UHC by reproducing the working browser
protocol and proving the resulting daemon state.

**Status:** Continue

## Alignment check

- **Necessary:** Yes. The existing archive implementation failed against the live daemon and could
  invoke an unsafe root-scope backup.
- **Aligned:** Yes. The replacement uses the user's previously working browser payload shape and
  directly enables the profile-setting control path.
- **Sufficient:** Yes. The implementation is one form dispatcher plus native verification; the
  archive parser, rewrite, rollback storage, multipart transport, and unused dependencies are gone.
- **Mechanism clear:** Yes. Fetch fresh form state, submit the browser request, then accept success
  only when `ConfigurationGet` names the requested profile and both control lanes recover.
- **Changes complete:** Yes. Production code, hermetic HTTP/native fakes, opt-in live test, endpoint
  lease coverage, evidence ledger, execution record, and dependency surface agree.

## Drift detected

**Solution drift (intentional, evidence-driven):** The branch started with ZIP backup/rewrite/restore
and changed to direct browser-form profile loading. Live observation showed the archive route is both
unnecessary and unsafe under the production service context, while a non-identical form load and
rollback succeeded and were natively verifiable. The aim did not change.

## Decision

Continue to commit and update PR #428 after the full verification gate. Do not restore any archive
code or treat an HTTP response as state authority.
