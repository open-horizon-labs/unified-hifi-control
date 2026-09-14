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

## Automatic outage recovery (#730)

Connection failures no longer create permanent quarantine when the persisted
32-attempt hourly budget runs out. The connector remains alive and waits until
it can retry. Settings reports offline and explains automatic retries using the
existing status contract.

The first attempt is immediate. The next two are spaced by at least 5 and 30
seconds; subsequent attempts are at least 15 minutes apart. Each interval adds
0–30 seconds of jitter. A successful authenticated connection must last 15
minutes before fast retries become available again. The hourly attempt ceiling
is independent and is never reset by a successful connection.

`hiphi-relay-epoch.retry` persists the retry sequence and next eligible time
before network work. Restarts retain this schedule. Live scheduling uses elapsed
monotonic time, so wall-clock corrections cannot accelerate retries. Across a
restart, a backwards clock correction rebases the saved interval, retaining its
sequence; an expired schedule admits one attempt and retains slow pacing. A
future-dated hourly budget is likewise rebased without clearing its counter.
An exhausted legacy budget can therefore take up to an hour to expire; ordinary
slow recovery is within 15 minutes plus 30 seconds of jitter.

Traffic quarantine remains deliberate containment. Existing `.quarantine` files
lack a reliable cause and are not automatically removed, including old markers
created by reconnect exhaustion. Use Resume once for those. Resume preserves the
new retry schedule, pairing keys and replay ledger. Corrupted retry state is a
safety error, not permission to start fresh.

Production logs now include failed grant/connect reasons and
`cloud_reconnect_cooldown` with attempt count, relative delay and the estimated
next retry timestamp. Invalid grant JSON is reported without response content.
The original cause of the September 8 network interruption is still unknown;
local logs establish containment, not whether Cloudflare or the network closed
the socket. Persistent Cloud-side denial after an eligible retry requires Cloud
investigation and cannot be cleared by this local policy.

### Execution and risk evidence

The regression `outage_budget_exhaustion_is_temporary_and_never_creates_quarantine`
was observed failing on the previous implementation (`cost_limit` instead of
no pause). The retry-policy tests cover a 24-hour outage, hourly-budget recovery,
restart loops, short authenticated sessions, sustained success, clock jumps,
legacy containment and corrupt state. The runtime test
`exhausted_budget_keeps_supervisor_alive_offline_and_shutdown_interrupts_cooldown`
checks that the live supervisor waits instead of exiting or offering manual
resume, and shuts down promptly. Existing flood, replay, authority, heartbeat
and API-contract tests remain required.

These checks reject increasing the budget, resetting it on restart or each
hourly retry burst, resetting fast retries on every successful handshake, and
expiring all quarantine markers. Live monotonic scheduling is also verified by
inspection: the retry clock is anchored once at run startup, and all subsequent
admission times use only elapsed monotonic time. Actual recovery on the NAS and
the reason for Cloud/network resets require deployment observation; no production
reset or deployment is part of this change.
