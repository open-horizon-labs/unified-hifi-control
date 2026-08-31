# HiPhi Cloud issuer-key rotation

UHC independently pins two HiPhi Cloud authorities. The installation-session
authority authenticates the outbound WebSocket session; the command authority
authorizes exact remote commands. They must never share a key ID or public key.

Configure each authority as a version-1 JSON ring containing one to eight
unpadded base64url Ed25519 public keys:

```text
UHC_HIPHI_SESSION_ISSUER_KEYS={"version":1,"keys":[{"kid":"session-v2","key":"BASE64URL_PUBLIC_KEY"}]}
UHC_HIPHI_COMMAND_ISSUER_KEYS={"version":1,"keys":[{"kid":"command-v2","key":"BASE64URL_PUBLIC_KEY"}]}
```

UHC fails closed before connecting if a ring is malformed, empty, larger than
eight entries or 4 KiB, contains an invalid or weak key, repeats a key ID or
public key, or overlaps the other authority's IDs or keys. The former singular
`*_KEY_ID` and `*_PUBLIC_KEY` variables are not accepted.

## Planned rotation

Rotate the two authorities independently. For each one:

1. Add the new public key to the cloud relay's verifier ring while retaining
   the old public key. Restart the relay and confirm readiness.
2. Add the same new public key to UHC's corresponding issuer ring while
   retaining the old entry. Restart UHC and confirm the connector is healthy.
3. Switch the HiPhi authorizer to the new private signer and matching `kid`.
   Confirm newly issued grants use the new ID and UHC accepts them.
4. Start the retirement clock only after the authorizer is confirmed to issue
   exclusively with the new key. Retain a command verifier for at least 75
   seconds and a session verifier for at least 180 seconds: each interval is
   the protocol's maximum grant lifetime plus its 60-second clock tolerance.
   Add an operational safety margin.
5. Remove the old entry from UHC and the cloud relay, restart each component,
   and confirm new grants still work while an otherwise-valid old-key grant is
   rejected as unknown.

Removing the old verifier before step 4 strands a healthy connector. Leaving
retired keys indefinitely enlarges the trust set; the eight-key bound is a
safety ceiling, not a retention policy.

For a suspected signer compromise, remove the affected public key immediately
on both sides and accept the short authentication outage. Issuer rotation does
not require deleting the paired installation or its locally held private key.
No part of this procedure changes or exposes UHC's LAN HTTP API.
