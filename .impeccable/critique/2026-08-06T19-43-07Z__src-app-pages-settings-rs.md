---
target: src/app/pages/settings.rs
total_score: 26
max_score: 40
na_heuristics: 
p0_count: 0
p1_count: 1
timestamp: 2026-08-06T19-43-07Z
slug: src-app-pages-settings-rs
---
Method: dual-agent (A: settings_design_critique · B: critique_b)

## Design Health Score

| # | Heuristic | Score | Key Issue |
|---|---|---:|---|
| 1 | Visibility of System Status | 3 | Status exists, but disabled providers previously still exposed active setup actions. |
| 2 | Match System / Real World | 2 | A provider section implied setup was available even when its feature was off. |
| 3 | User Control and Freedom | 2 | Reconnect/refresh/disconnect actions were available in a disabled state. |
| 4 | Consistency and Standards | 3 | Feature toggles and provider cards now share one enablement model. |
| 5 | Error Prevention | 2 | Progressive disclosure was missing; users could enter an irrelevant OAuth flow. |
| 6 | Recognition Rather Than Recall | 2 | Users had to infer that a Disabled badge changed the behavior of the whole section. |
| 7 | Flexibility and Efficiency | 3 | The settings surface supports both hosted and self-hosted setup, but only when enabled. |
| 8 | Aesthetic and Minimalist Design | 3 | Removing inactive provider cards materially reduces page weight and blank space. |
| 9 | Error Recovery | 3 | Existing status/error regions remain available when a provider is enabled. |
| 10 | Help and Documentation | 3 | Tunnel guidance is now generic to any UHC host and configured port. |
| **Total** | | **26/40** | Progressive disclosure was the main gap. |

## Design Specificity Verdict

The issue was a progressive-disclosure failure, not a visual-style problem. The incumbent Settings surface is product-specific and readable, but it treated disabled providers as active onboarding destinations. The narrow fix is to render the entire Streaming providers section only when `spotify_enabled() || applemusic_enabled()`.

The deterministic detector returned `[]` for `src/app/pages/settings.rs`. Browser evidence confirmed that the disabled state still rendered both large provider cards, including setup fields and actions; after the fix, the section is absent with both providers disabled and appears reactively when Spotify is enabled. The QNAP-specific instructions were also generalized to “the machine running UHC.”

## What's Working

- Feature status and provider status are now aligned: disabled means the onboarding block is absent.
- Existing loading, empty, and error regions retain `role="status"`/`role="alert"` semantics when the section is enabled.
- The updated tunnel copy names the UHC host and configured port rather than assuming QNAP.

## Priority Issues

- **[P1] Disabled providers still exposed active setup**: this created contradictory affordances and unnecessary cognitive load. **Fix:** conditional section rendering; implemented.
- **[P2] Deployment-specific tunnel copy:** “QNAP”/“NAS” made a general UHC feature appear platform-bound. **Fix:** generic self-hosted/another-machine language; implemented.
- **[P2] Provider cards remain equal-height when both are enabled:** Apple Music can leave a large blank lower area beside Spotify’s longer setup card. **Fix:** follow-up layout refinement, outside this narrow patch.
- **[P3] Theme buttons lack selected semantics:** visual theme state is not exposed as `aria-pressed`; follow-up accessibility work, outside this patch.

## Persona Red Flags

- **First-time self-hosted user:** previously saw OAuth fields before enabling a provider and could mistake a disabled badge for an actionable setup state. The section now stays out of the way until enabled.
- **Remote operator:** QNAP/NAS wording implied the workflow did not apply to Docker, macOS, Linux, or another host. The copy now describes UHC generically and keeps the temporary HTTPS tunnel requirement.
- **Power user:** provider actions are now discoverable from the feature toggle instead of requiring interpretation of a disabled card.

## Minor Observations

- Theme selection should eventually expose `aria-pressed` or equivalent selected state.
- Provider status changes could be announced through a live region.
- When only one provider is enabled, a future refinement could hide or collapse the other provider card.

## Questions to Consider

- Should the feature rows become the sole entry point, with provider cards appearing directly beneath the enabled row?
- When one provider is enabled, should the disabled provider be omitted entirely or shown as a compact “Enable” affordance?
