# Recovering paused HiPhi Cloud access

Starting with the next alpha after v4.0.0-alpha.6, Settings distinguishes a
stopped Cloud connector from one that is connecting. A cost stop appears as
**Cloud paused · cost protection**. Local playback continues.

After the Cloud incident is resolved, choose **Resume Cloud connection** in
local UHC Settings. This resets the local cost stop and connection-attempt
window, then starts one connector task. It does not re-pair the installation,
change its key, reset the replay ledger, or disable traffic limits. Cloud can
still refuse the connection while its own safety stop is active.

Recovery attempts are limited to one every 15 minutes. That cooldown is saved
on disk and survives restarting UHC. Repeated clicks cannot start parallel
connectors. If the original problem continues, cost protection can stop the
connector again.

If Settings reports that safety state needs attention, inspect the UHC logs
and configuration storage. The recovery action will not erase invalid replay
or reconnect state. An ordinary restart deliberately does not clear a cost
quarantine.

## Local API

The owner approved these additions for #694:

- `POST /api/hiphi/connection/resume`: no required request body. Returns the
  pairing status after scheduling startup; this does not claim the remote
  connection is online. A refused or failed attempt returns HTTP 409 with
  `code: "cloud_resume_failed"` and a `message`.
- `GET /api/hiphi/pairing/status`: retains existing fields and adds
  `pause_reason` (null, `cost_limit`, or `safety_state_unavailable`) and
  `can_resume` (boolean). `connector_state` now includes `paused`.

Recovery follows the existing controller-auth policy: when enabled, the
controller session and same-origin CSRF checks apply, just as for pairing.
Authenticated Home Assistant ingress uses the existing ingress boundary.
Controller authentication remains opt-in.

## Recovery design review

The objective is recovery from a local cost stop without filesystem surgery.
This does not assume that every offline installation is quarantined: the
persisted state is checked before offering resume. Restart was observed not to
restore the incident NAS, but its actual quarantine file was not inspected.

The main failure modes are another traffic storm, duplicate connector tasks,
and accidentally resetting replay protection. A persisted cooldown, serialized
startup/recovery, validation before file changes, and preservation tests address
those risks. The cooldown is committed before counter reset; the stop flag is
removed last. Partial file-update failures therefore keep the connector stopped.

The alternative of clearing quarantine on every restart was rejected because
package restart loops would defeat containment. Automatic recovery of corrupt
state was rejected because it could erase evidence or replay protection. The
manual action is limited to a cost stop and leaves the Cloud safety lease intact.
