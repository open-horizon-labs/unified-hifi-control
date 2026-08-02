# HQPlayer persistence execution record

## Aim

Make named HQPlayer settings profiles safely selectable from UHC, with success defined by observed
HQPlayer state rather than an HTTP response.

## Selected solution

Replay the daemon's browser contract: fetch the profile page, retain hidden inputs, submit
`application/x-www-form-urlencoded` data to `/config/profile/load` with `Origin` and `Referer`, and
handle Digest challenge/retry. After dispatch, invalidate profile-dependent caches and require native
`ConfigurationGet` to name the requested profile before reporting success. Refresh native setting
lists and the profile form before returning.

The archive `/backup/settings.zip` + `/restore` lane is removed from UHC. On root-run HQPlayer
Embedded 6.0.2, a live request to the backup route started copying `/` rather than a bounded settings
tree. User-mode backup behaved normally, proving the hazard is service-context dependent but not a
safe basis for client behavior.

## Execution evidence

- Hermetic browser-form test covers Digest challenge/retry, sorted hidden fields, URL encoding,
  `profile`, `Origin`, and `Referer`.
- Endpoint-lease tests cover form dispatch, dropped/500 HTTP responses reconciled by native state,
  cache invalidation/refill, and queued reconfiguration.
- Opt-in tier-2 live test changed an isolated HQPlayer daemon to a non-identical named profile and
  verified it with native `ConfigurationGet`; the same test restored the rollback profile.
- Temporary profiles, copied configuration, and the isolated daemon were removed. The ordinary
  `hqplayerd` service was restored active on ports 4321 and 8088 with zero restarts.

## Safety boundary

HTTP status and body are dispatch details, not state authority. If the form POST is lost or returns
an error after dispatch, UHC polls native state and reports success only on an exact semantic match.
UHC does not download, rewrite, upload, or retain settings archives.
