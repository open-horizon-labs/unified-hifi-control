# HQPlayer control protocol — evidence ledger

<!-- uhc-hqp-ledger/v1 -->

> **What this is.** One ranked, provenance-tagged index of what UHC knows about HQPlayer's control
> protocol, how it knows each thing, for which edition and version on which date with playback active
> or idle, and which executable test proves it.
>
> **What it is not.** It is **not** the protocol authority. The authority is the executable corpus
> under `tests/fixtures/hqplayer/<version>/` driven by `tests/hqplayer_conformance.rs`, per
> [ADR 003](adr/003-hqplayer-conformance-boundary.md). Where a row here disagrees with the corpus, the
> corpus wins and the row is the defect. `tests/hqplayer_ledger_lint.rs` enforces the parts of that
> relationship a machine can check.
>
> **Supersedes:** `docs/hqplayer-protocol-reference.md` (reader's guide, retired as guidance) and the
> protocol sections of `.oh/hqplayer-spec.md` (historical session record). Both are kept, corrected in
> place, and point here.
>
> Issue #341 · epic #311 · program #310 · OH 80222d6d · created 2026-07-30

---

## How to read this ledger

### Evidence classes, strongest first

| Class | Means | What it does not mean |
|---|---|---|
| `E0-uhc-live` | UHC observed it on a daemon UHC ran, and the run is in the **Live runs** registry below | Not "reproducible on other hardware" — one rig, one day |
| `E1-upstream-verified` | Verified upstream against a live daemon, reaching UHC **through a report**, never read first-hand | Not verified by this project. See the chain caveat below |
| `E2-official-source` | Derived from the official `hqp-control` reference implementation's sources | Not observed at all — the CLI's own argument names have been wrong about semantics before |
| `E3-derived` | Transcribed or derived: the shape is right, the specific numbers may be excerpt-local | Not a capture |
| `E4-unverified` | Asserted in earlier UHC prose with no observation behind it | Nothing. This class exists so those assertions can be found and retired |
| `E5-synthetic` | Constructed to build a hazard shape | Never evidence, never promotable |
| `E6-documentary` | A fact about a document, repository, licence or issue record rather than about daemon behaviour | Not weaker than everything above it — it sits outside the ranking, and its strength comes from **chain**, not from its position |

### The provenance quadruple

Every claim's provenance cell reads `source · chain · daemon/version · date · playback`.

- **chain** is one of `direct`, `read-via-report`, `read-via-issue`, `read-via-pr`. It records how the
  observation reached this ledger, and it is load-bearing: three factual errors during #322 came from
  treating a report's citation as though the cited file had been read.
- **date** for a `read-via-report` claim is the **report's** date, not the observation's — the
  observation date was not recorded upstream, and the report date is the closest honest bound UHC has.
- **playback** is `active`, `idle`, `unknown` or `n/a`. The upstream evidence base's probes were
  *largely* collected with the engine **stopped**, which is why `idle` appears on most `E1`
  behavioural rows — a behaviour verified idle is not thereby verified under load. That aggregate
  caveat is **not** a per-probe record and must not be used as one: where the #322 session file
  records a specific probe's state, that record wins (HQP-C-023 is `active` for exactly this reason). `E0` may **never** say `unknown` — UHC ran the daemon, so
  UHC knows. `E1` may say `unknown` only when its prose anchor states *"Playback state was not
  recorded upstream"*, which is the case for two rows (HQP-C-029, HQP-C-038). Guessing a value to fill
  the column, or downgrading a live observation to a transcription to dodge it, would both be worse.

### Proof forms

| Form | Meaning |
|---|---|
| `test:<name>` | A test in `tests/hqplayer_conformance.rs`. **The test name is the canonical citation** — it says what it proves, which a claim ID cannot. Claim IDs are index keys for cross-issue reference, not replacements |
| `test:<file>::<name>` | A test in another target |
| `fixture:<path>` | A corpus document, provenance header included |
| `#332:<row>` | A live-qualification row in #332 that has **not run**. A claim proved only this way can never be `settled` |
| `none:<what would settle it>` | No proof exists. The text after the colon is the acquisition plan |

### Status, IDs, and what this ledger cannot do

`settled` · `open` · `pending-live` · `retired`. Every `open` and `pending-live` row names an owner
issue and has a prose anchor below carrying a **What would settle it** line.

Claim IDs are **append-only and contiguous** from `HQP-C-001`. Retirement is a status change, never a
deletion — a gap in the numbering would be how a retired claim disappears without leaving the record
that it was retired, so the lint refuses gaps.

**Claim IDs are a stable interface for the issues named in the Owner column.** #332, #337, #347 and
#348 are expected to cite them (`HQP-C-026`, `HQP-C-057`, …), so an ID keeps its meaning even when the
row's wording, class or status changes. What an ID must never do is move to a different claim.

**The lint checks form, not truth.** A well-formed row citing a real test can still assert something
false. The classes above exist so a reader can weigh a row; nothing mechanical can do that for them.

---

## Live runs

First-hand UHC observations. `E0-uhc-live` rows must trace to a run recorded here, so qualifying
another rig is an explicit edit to this table rather than a loosened check.

| Run | Edition / version | Date | Playback | Evidence |
|---|---|---|---|---|
| L1 | Embedded 6.0.2 / engine 6.0.4 | 2026-07-30 | idle | PR #337 [comment 5135836825](https://github.com/open-horizon-labs/unified-hifi-control/pull/337#issuecomment-5135836825) — read-only capture matrix, reversible mutation/recovery matrix, and the operator-repaired-host qualification pass. No queue was loaded, so every observation is idle-state |

**L1's own limits, stated once so no row has to repeat them.** One host, one day, one engine build.
145 corpus divergences were recorded and triaged as 144 hardware-enumeration differences (that rig's
SDM DAC chain against an excerpt corpus) plus one fresh-install artefact; **zero** were protocol-shape
divergences. Persistent profile create/delete, `MatrixSetProfile`, and transport with real content
were **not executed** and are recorded as gaps in that comment.

---

## Claim index

### Mode, and the contradiction this issue exists to retire

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-001 | `SetMode value=` carries the **list index**, not the enum ID: SDM is index 2 while its enum ID is 1 | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:modes_list_distinguishes_list_index_from_enum_id · fixture:tests/fixtures/hqplayer/hqpd-6.0.4-opal/modes.xml | settled | — |
| HQP-C-002 | Domain 1 — the **live wire** domain: `State.mode/filter/filter1x/filterNx/shaper/rate` and every `Set*` speak a **list index** into the currently loaded enumeration | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:a_filter_name_is_sent_as_the_index_the_observed_list_gives_it · test:a_filter_name_is_not_sent_as_its_enum_id | settled | — |
| HQP-C-003 | Domain 2 — the **persistent** domain: `hqplayerd.xml` and the 8088 config form store **enum IDs**, which are not list indices and must never be fed to the live lane | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-hqplayer-protocol-conformance.md` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:the_persistent_configuration_lane_stores_enum_ids_not_list_indices · test:feeding_a_persistent_enum_id_to_the_live_lane_is_rejected | settled | — |
| HQP-C-004 | Domain 3 — the **semantic name**: UHC's clients (UI, API, MCP) exchange names and Hz only; index and enum-ID conversion is the adapter's job and never leaves it | E6-documentary | `.oh/hqplayer-spec.md` layer table · direct · n/a · 2026-02-04 · n/a | test:a_filter_name_is_sent_as_the_index_the_observed_list_gives_it | settled | — |
| HQP-C-005 | `SetMode` does **not** reset the filter or shaper selections; the fake once modelled that and no evidence supports it | E3-derived | `.oh/issue-322-…` amendment row B14 · direct · hqplayerd 6.0.4 (Opal) · 2026-07-30 · unknown | test:set_mode_does_not_reset_the_filter_and_shaper_selections | settled | — |
| HQP-C-006 | A mode change reloads the chain, so a list index resolved before it can name a **different** setting after it | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:a_delayed_set_mode_still_clamps_indices_into_the_loaded_chain · test:the_same_filter_index_selects_a_different_filter_per_loaded_chain | settled | — |
| HQP-C-007 | Under configured `[source]` the **loaded chain** can move between PCM and SDM while `State.mode` stays 0 | E1-upstream-verified | UHC-SALVAGE-BETA-DEV via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:the_loaded_chain_moves_under_source_while_the_configured_mode_stays_source | settled | — |

### Enumerations, and how they are numbered

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-008 | Enumerations are **mode-relative**: `GetFilters`/`GetShapers`/`GetRates` return the loaded chain's list only, and the lists differ wholesale — 77/36/7 in SDM against 67/10/22 in PCM on L1 | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:enumerations_are_mode_relative_and_are_refetched_after_a_mode_change | settled | — |
| HQP-C-009 | The same filter **name** occupies different list positions in different chains and on differently-ordered daemons | E3-derived | `tests/fixtures/hqplayer/hqpd-6.0.4-opal/filters_*.xml` · direct · hqplayerd 6.0.4 (Opal) · 2026-07-29 · unknown | test:the_same_filter_name_resolves_to_a_different_index_on_a_differently_ordered_daemon · test:a_stale_cross_chain_filter_index_lands_in_range_on_a_different_filter | settled | — |
| HQP-C-010 | The **persistent** lane numbers the same filter differently per chain: `poly-sinc-gauss-long` is enum 40 under PCM `filter` and 38 under SDM `oversampling` | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…` (`livemap.py:17-18`) · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:the_persistent_lane_numbers_the_same_filter_differently_per_chain | settled | — |
| HQP-C-011 | Whether **`GetFilters`** renumbers a name's enum ID between chains — the live-lane analogue of HQP-C-010 — is unmeasured, so the corpus asserts invariance it cannot prove | E4-unverified | #341 [comment 5125948432](https://github.com/open-horizon-labs/unified-hifi-control/issues/341#issuecomment-5125948432) · read-via-issue · hqplayerd 6.0.4 (Opal) · 2026-07-30 · unknown | none:read-only capture of the loaded PCM and SDM GetFilters lists, recording whether the same name carries a different `value` | open | #332 |
| HQP-C-012 | `ShapersItem` carries `index`/`name`/`value` only — no `arg`, no `description` — while `FiltersItem` carries both | E3-derived | ADR 003 tier-1 family table · direct · hqplayerd 6.0.4 (Opal) · 2026-07-29 · unknown | test:tier1_captures_and_diffs_filter_arg_flags · test:tier1_captures_and_diffs_filter_description_presence | settled | — |
| HQP-C-013 | A device without DSD capability **omits** SDM from `GetModes` while the remaining indices stay intact | E3-derived | `tests/fixtures/hqplayer/hqpd-6.0.4-pcm-only-dac/modes.xml` · read-via-report · hqplayerd 6.0.4, DAC without DSD · 2026-07-29 · unknown | test:a_device_without_dsd_omits_sdm_while_the_remaining_mode_indices_stay_intact · test:a_device_dependent_modes_claim_is_tier_one | settled | — |
| HQP-C-014 | The corpus's enumeration **list positions** are excerpt-local, not observed: name-to-enum-ID pairs and the `Set*` anchors are the evidenced part | E3-derived | fixture provenance headers · direct · hqplayerd 6.0.4 (Opal) · 2026-07-29 · unknown | test:the_verified_profile_marks_excerpts_honestly · fixture:tests/fixtures/hqplayer/hqpd-6.0.4-opal/filters_pcm.xml | pending-live | #332 |

### Rate

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-015 | `SetRate value=` is the **list index** of the wanted rate in the current chain's list; `RatesItem` carries `rate` in Hz and **no** enum ID; `Status.active_rate` reports Hz | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:rates_list_reports_hz_and_has_no_enum_id · test:a_rate_valid_in_one_chain_is_refused_in_the_other | settled | — |
| HQP-C-016 | Rate list index **0 is Auto** (`rate="0"`), and a mode change resets the runtime rate to it — L1 observed `rate 3 → 0` | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:tier1_requires_rate_index_zero_to_be_auto · test:a_same_mode_set_mode_still_clears_the_rate_pin | settled | — |
| HQP-C-017 | A **same-mode** `SetMode` still reloads the chain and clears the exact-rate pin, so a no-op mode write is not a no-op | E1-upstream-verified | UHC-SALVAGE-BETA-DEV via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:a_same_mode_set_mode_still_clears_the_rate_pin | settled | — |
| HQP-C-018 | Under `[source]` a **non-zero** rate pin is accepted and ignored: the setter answers `OK` and the runtime rate does not move | E1-upstream-verified | UHC-SALVAGE-BETA-DEV via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:a_nonzero_rate_pin_under_source_is_accepted_and_ignored | settled | — |
| HQP-C-019 | Under `[source]` an **Auto** (index 0) rate request is ignored and a readback **cannot tell** — comparing 0 against 0 reports success for a command that did nothing | E1-upstream-verified | UHC-SALVAGE-BETA-DEV via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:an_auto_rate_request_under_source_is_ignored_and_readback_cannot_tell | open | #347 |
| HQP-C-020 | The offered rate list varies wholesale **by mode** — PCM 44.1k–768k against SDM 2.8M–24.6M on L1, with no overlap | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:a_rate_valid_in_one_chain_is_refused_in_the_other | settled | — |
| HQP-C-021 | The rate list also depends on the **selected filter**, so mode alone is insufficient to resolve a rate | E1-upstream-verified | UHC-SALVAGE-BETA-DEV §4 via #341 [comment 5126438674](https://github.com/open-horizon-labs/unified-hifi-control/issues/341#issuecomment-5126438674) (`livelane.py:33-38`) · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | none:capture GetRates before and after a SetFilter on one device and record which entries change | open | #332 |
| HQP-C-022 | Whether `GetRates` can be **empty** at all is unsupported by any observation in the audited evidence base | E4-unverified | #341 comment 5126438674 · read-via-issue · n/a · 2026-07-30 · n/a | none:read-only GetRates capture on a daemon with no usable output configured, or upstream confirmation that an empty list is reachable | open | #332 |

### `active_mode` — the question this ledger refuses to settle

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-023 | `Status.active_mode` **echoes the configured mode** under `[source]`, so it cannot resolve the loaded chain | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…:1549-1552` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · active | test:the_fake_does_not_settle_the_independent_state_and_status_active_mode_semantics | settled | — |
| HQP-C-024 | What `State.active_mode` reports under `[source]` — the loaded chain or the configured mode — is **unmeasured**, upstream and here | E4-unverified | `.oh/issue-322-…:1738-1740` · direct · unmeasured on any daemon · 2026-07-30 · unknown | test:the_fake_does_not_settle_the_independent_state_and_status_active_mode_semantics · #332:Resolve State.active_mode versus Status.active_mode under configured PCM/SDM and [source]/Auto with PCM and DSD sources | open | #332 |
| HQP-C-025 | `Status` reports active settings as **display names** while `State` reports numbers, so the two are complementary rather than redundant | E3-derived | `tests/fixtures/hqplayer/hqpd-6.0.4-opal/status_playing_with_metadata.xml` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · unknown | test:status_reports_active_settings_as_display_names | settled | — |
| HQP-C-026 | UHC's own comment at `src/adapters/hqplayer.rs:2473` calls `Status.active_mode` "unreliable" and instructs using `State`'s — a global rule the evidence does not support | E4-unverified | `src/adapters/hqplayer.rs:2473` · direct · n/a · 2026-07-30 · n/a | none:HQP-C-024 settling, after which the comment states a measured fact or is deleted | open | #347 |

#### HQP-C-023's playback state was corrected, and the wrong value had already travelled

An earlier revision of this row recorded `playback: idle`, inferred from the aggregate caveat that the
upstream probes were collected with the engine stopped. **The #322 session file records this specific
probe as `playback active`** (`.oh/issue-322-hqplayer-protocol-conformance.md:1549-1552`), and a
per-probe record beats an aggregate caveat. The row now says `active`.

**This matters beyond one cell.** While this branch was being written, base-branch commit `ab18874`
*removed* a "mid-playback" qualifier from `docs/hqplayer-protocol-reference.md` and
`tests/mock_servers/hqplayer/model.rs`, and its stated reason was: *"the ledger (HQP-C-023) records
this upstream probe as idle."* That is circular — it used this ledger's unverified inference as
evidence against the session file's contemporaneous record. Recorded as **HQP-C-061** so the base
branch can re-decide on a non-circular basis rather than inheriting a value this ledger has since
withdrawn.

### Command outcomes, delivery, and ambiguity

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-027 | Setters and transport commands **echo the request element** carrying `result="OK"` or `result="Error"` with a reason as element text; an **absent** `result` is a third legitimate case, and `<Ok/>` is a shape the daemon never sends | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:an_explicitly_rejected_setter_reports_the_daemon_reason · test:a_rejected_setter_is_reported_as_an_error_without_dropping_the_connection | settled | — |
| HQP-C-028 | `result="OK"` is **not proof of application**: a setter can answer OK without applying, and a change can land a poll later | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:a_setter_accepted_but_not_applied_does_not_report_success · test:a_setter_whose_change_lands_after_a_poll_still_reports_success | settled | — |
| HQP-C-029 | A **timeout or disconnect after a write attempt is ambiguous delivery**, never proof of non-application: on HQPlayer Embedded 6.0.4 a `SetMode` was accepted, logged and acted on while the daemon sent no response and later dropped the connection | E1-upstream-verified | HQPTuner `origin/dev@04ab82e148c8db8ffbcccd4c3c3e69cce7332b64` via #341 body · read-via-issue · HQPlayer Embedded 6.0.4 · 2026-07-30 · unknown | test:next_advances_one_track_when_the_reply_is_lost_after_the_daemon_applied_it · test:volume_mute_retries_and_converges_when_the_reply_is_lost_after_the_daemon_applied_it | open | #332 |
| HQP-C-030 | A **relative or sequential** one-shot (`Next`, `Previous`, `VolumeUp`, `VolumeDown`) is never retried once its write is attempted: the protocol carries no request identity, so a retry doubles the side effect | E1-upstream-verified | `.oh/issue-322-…` execution record · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:next_advances_one_track_when_the_reply_is_lost_after_the_daemon_applied_it | settled | — |
| HQP-C-031 | `VolumeMute` is an **absolute mute-to-floor and idempotent** — three live calls held −60 dB, unmute is a separate `<Volume>` write, and `State` exposes no mute flag — so it is retry-safe | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:mute_is_absolute_and_idempotent_on_the_daemon · test:volume_mute_retries_and_converges_when_the_reply_is_lost_after_the_daemon_applied_it | settled | — |
| HQP-C-032 | A setter **overridden by another controller** is indistinguishable from one the daemon dropped; both fail, and the error names the value the daemon actually reports | E3-derived | `.oh/issue-322-…` stage-2 dissent · direct · hqplayerd 6.0.4 (Opal) · 2026-07-29 · unknown | test:a_setter_overridden_by_another_controller_fails_and_names_the_observed_value | settled | — |
| HQP-C-033 | An **empty transport** answers `result="Error">Empty transport` rather than succeeding silently | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:an_explicitly_rejected_setter_reports_the_daemon_reason | settled | — |

### Framing, encoding and the stream

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-034 | Documents are newline-**terminated**, not newline-**framed**: a document may contain internal newlines, and a `Status` document's self-closing `<metadata …/>` child means the document ends at `</Status>` | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:state_read_after_status_with_metadata_child_reports_the_daemon_state | settled | — |
| HQP-C-035 | Attribute values arrive **entity-escaped**, a bare `&` has been observed, and attribute lookups must be scoped to the root element or the XML declaration's `version="1.0"` shadows `<GetInfo … version="6"/>` | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:the_matrix_profile_family_round_trips_a_name_containing_an_entity · test:get_info_reports_the_verified_daemon_identity | settled | — |
| HQP-C-036 | Whether the daemon emits **double-escaped** attribute values on the wire, or whether that is an artefact of `hqp-control`'s own XML parse, is unresolved — UHC substring-scans and decodes once; the upstream client parses then decodes again | E4-unverified | `.oh/issue-322-…:1189` row D3 · direct · hqplayerd 6.0.4 (Opal) · 2026-07-30 · unknown | none:capture the raw bytes of an attribute value known to contain an entity, without an XML parser in the path | open | #332 |
| HQP-C-037 | `Status subscribe=1` pushed at roughly **1–2 Hz during playback**, can be **silent while stopped**, and did **not** emit an external settings change in the inspected session — so polling remains the portable reconciliation mechanism | E1-upstream-verified | HQPTuner dev audit via #341 body · read-via-issue · HQPlayer Embedded 6.0.4 · 2026-07-30 · active | test:continuous_unsolicited_traffic_cannot_extend_the_command_deadline · test:tier1_records_how_many_unsolicited_documents_the_client_skipped | open | #332 |
| HQP-C-038 | Issuing **`LibraryPicture size`** changes the byte stream from XML-only until exactly the advertised byte count is consumed — a framing hazard for any parser that assumes documents all the way down | E1-upstream-verified | HQPTuner dev audit via #341 body · read-via-issue · HQPlayer Embedded 6.0.4 · 2026-07-30 · unknown | none:UHC does not issue LibraryPicture; a fixture is only warranted when album art is implemented | open | #328 |
| HQP-C-039 | A fully **idle** native connection is closed by the daemon at roughly 156 s — a single observation, not a measured timeout | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…:82` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | #332:Measure native idle-drop and every restart route on each supported edition/version rather than inheriting the single-sample ~156-second observation | pending-live | #332 |

### Volume

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-040 | `State.volume`, `Status.volume` and `VolumeRange.min`/`max`/`step` are **doubles in dB**; L1 reported `volume="-3.00000000000000000"` | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:a_fractional_negative_db_volume_round_trips · test:a_rounded_volume_is_never_reported_as_zero_db | settled | — |
| HQP-C-041 | `VolumeRange` may omit `step` entirely — L1 reported `min=-60 max=0` with no `step` — so a client must not invent one | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:a_volume_range_that_omits_step_reports_it_as_absent · test:volume_range_reports_bounds_and_flags | settled | — |
| HQP-C-042 | `VolumeUp`/`VolumeDown` moved the level by **1.0 dB** on L1 and remain correctly classified as relative one-shots | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:a_volume_step_moves_the_level_by_the_advertised_step | settled | — |

### The persistent HTTP lane — negative findings, recorded so they are not retried

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-043 | A **partial `POST /config` is not a write path**: it can return **HTTP 200 with a failure body**, and it cannot express all owned settings | E1-upstream-verified | UHC-SALVAGE reports via #330 and `.oh/issue-322-…:1190` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:the_restore_response_family_carries_no_outcome_signal · test:the_restore_fixture_records_why_its_status_code_proves_nothing | settled | — |
| HQP-C-044 | The daemon's own **profile save/load/restore** routes are unsafe as a durable preset store: loading a profile restarts the daemon and may leave `/backup/settings.zip` empty until a further restart, and a named active profile may have no `hqplayerd.xml` at all | E1-upstream-verified | UHC-SALVAGE reports via #330 · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | fixture:tests/fixtures/hqplayer/hqpd-6.0.4-opal/restore_response.html · #332:Cover named-profile empty backup and renamed active root-member failures separately | pending-live | #330 |
| HQP-C-045 | A persistent write's **HTTP 200 proves receipt only**; success comes from a readback after the daemon has served a fresh form, never from the POST | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:the_restore_response_family_carries_no_outcome_signal | settled | — |
| HQP-C-046 | The config form distinguishes the **unnamed base configuration** from named profiles, and the field names are `profile` / `profile_name` | E3-derived | `tests/fixtures/hqplayer/hqpd-6.0.4-opal/config_profile_form.html` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · unknown | test:the_persistent_config_form_carries_the_verified_field_names · test:the_persistent_config_form_separates_the_unnamed_base_from_named_profiles | settled | — |
| HQP-C-047 | An authenticated read of `/config` succeeds under digest auth in realm `com.signalyst.hqplayer.embedded` — 3/3 HTTP 200 on L1 | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:tier1_records_the_config_read_side_as_unreached_without_credentials | settled | — |

### Session authentication — negative finding

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-048 | **Self-generated session-authentication keys for port 4321 were rejected** in the inspected precedent, so a client must not attempt to mint one | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…:1190` row D4 · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | none:an authenticated-4321 capture against a daemon configured to require it, recording what the daemon accepts | open | #332 |
| HQP-C-049 | The native surface on L1 needed **no** session authentication: UDP 4321 discovery, TCP 4321 control, TCP 4322, UPnP 1900 and web 8088 all answered, and discovery advertised **no alternate control port** | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:get_info_reports_the_verified_daemon_identity | settled | — |

### Junk filter, and one capability the adapter does not expose

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-050 | The 20 kHz filter is `filter_junk`, an **int index** into `GetJunkFilters` — not a boolean `filter_20k` — and the wire element is `SetJunkFilter` | E1-upstream-verified | UHC-SALVAGE reports via `.oh/issue-322-…` · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | test:the_junk_filter_is_read_as_a_list_index_not_a_boolean | settled | — |
| HQP-C-051 | `SetJunkFilter` round-tripped `filter_junk 0→1→0` with `result="OK"` on L1, but the adapter exposes **no** `set_junk_filter` — a daemon capability UHC does not offer | E0-uhc-live | PR #337 comment 5135836825 · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | test:the_junk_filter_is_read_as_a_list_index_not_a_boolean | open | #329 |

### Licensing and provenance

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-052 | The **official `hqp-control` sources' license is not recorded in this repository**, so protocol facts derived from them are carried as paraphrase with file/line citations and no code is reproduced | E6-documentary | `docs/hqplayer-protocol-reference.md` header · direct · hqp-control v5.2.30 · 2024-03-31 · n/a | test:tests/hqplayer_ledger_lint.rs::no_verbatim_upstream_source_excerpt_remains_in_the_reference_document | settled | — |
| HQP-C-053 | **HQPTuner's code is MIT**, Copyright (c) 2026 Adam Goldsmith. Nothing is ported yet; `THIRD-PARTY-NOTICES` does not exist in this repository and must be added **before** any port | E6-documentary | #348 evidence section (HQPTuner LICENSE at ref 22dfe5cc) · read-via-issue · n/a · 2026-07-30 · n/a | none:#348 landing the guardrail and the notices file, which is maintainer-owned licensing policy | open | #348 |
| HQP-C-054 | HQPTuner's **catalog prose is manual-derived** — its own source is `hqplayer6desktop-manual.pdf`, Signalyst's copyrighted documentation, which the MIT grant does not convey — so filter `description` **presence** is the wire fact UHC asserts and the content is not reproduced | E6-documentary | fixture provenance notes via `.oh/issue-322-…` third-gate pass · read-via-report · hqplayerd 6.0.4 (Opal) · 2026-07-29 · idle | fixture:tests/fixtures/hqplayer/hqpd-6.0.4-opal/filters_pcm.xml · test:tier1_captures_and_diffs_filter_description_presence | settled | — |
| HQP-C-055 | Every upstream claim in this ledger reaches UHC **third-hand at best** — upstream repository → salvage report → #322 session file or issue body → here — and the salvage reports are not in this repository | E6-documentary | `.oh/issue-322-…:1091-1093` · direct · n/a · 2026-07-30 · n/a | test:every_fixture_sourced_from_a_salvage_report_records_that_chain · test:every_corpus_fixture_records_its_provenance | settled | — |
| HQP-C-056 | Private upstream correspondence is cited at a high level only and never reproduced | E6-documentary | #348 constraints · read-via-issue · n/a · 2026-07-30 · n/a | none:#348 landing the guardrail | open | #348 |

### Corpus hygiene observations

| ID | Claim | Class | Provenance | Proof | Status | Owner |
|---|---|---|---|---|---|---|
| HQP-C-057 | Fifteen fixtures record `source_chain: read-via-report`; **eight of them still embed explanatory prose inside that closed-vocabulary field**, which the base branch's own remediation collapsed for only five | E6-documentary | `tests/fixtures/hqplayer/**` provenance headers at `bc9158e` · direct · n/a · 2026-07-30 · n/a | test:every_fixture_sourced_from_a_salvage_report_records_that_chain | open | #337 |
| HQP-C-058 | L1's **SDM enumerations and filter `description` presence are first-hand evidence** that can re-provenance specific `read-via-report` fixtures — recorded here, deliberately not acted on, because re-provenancing from a report of a run is not the same as re-provenancing from the capture | E0-uhc-live | PR #337 comment 5135836825 follow-up · read-via-pr · Embedded 6.0.2 / engine 6.0.4 · 2026-07-30 · idle | #332:Confirm all upstream hqplayerd 6.0.4 observations on supported UHC rigs before converting them into general claims | pending-live | #332 |
| HQP-C-059 | The `hqpd-5.x-legacy` profile is `UNVERIFIED` and exists only to vary list ordering; it is never protocol truth | E4-unverified | `tests/fixtures/hqplayer/hqpd-5.x-legacy/*` · direct · hqplayerd 5.x, unavailable for verification · 2026-07-29 · unknown | test:the_legacy_profile_is_marked_unverified_so_it_cannot_pass_as_protocol_truth | settled | — |
| HQP-C-060 | The `synthetic-chain-hazard` profile is constructed, `never-promotable`, and every name in it is fictional so a row cannot be copied into the evidence corpus by mistake | E5-synthetic | `tests/fixtures/hqplayer/synthetic-chain-hazard/*` · direct · none — no daemon involved · 2026-07-30 · n/a | fixture:tests/fixtures/hqplayer/synthetic-chain-hazard/filters_sdm.xml | settled | — |
| HQP-C-061 | Base-branch commit `ab18874` de-qualified the `Status.active_mode` echo's playback state citing this ledger's own (now withdrawn) `idle`, so the de-qualification rests on circular evidence rather than on the #322 session file's contemporaneous `playback active` record | E6-documentary | `ab18874` commit message and `.oh/issue-322-…:1549-1552` · direct · n/a · 2026-07-30 · n/a | none:the base branch re-deciding on the session file's record, or reading the salvage report directly | open | #337 |

---

## Open questions register

Each row below is an `open` or `pending-live` claim, with the acquisition plan the lint requires.

### HQP-C-011 — does `GetFilters` renumber a name's enum ID between chains?

The persistent lane demonstrably does (HQP-C-010). The corpus gives `poly-sinc-gauss-long` enum ID 40
in **both** chains, which asserts an invariance nobody measured. Renumbering the SDM fixture to 38 was
proposed during #322 and **withdrawn**: it would have replaced one unverified number with another while
newly asserting a live-lane fact from evidence that is about `hqplayerd.xml` attribute names.

**What would settle it:** a read-only capture of the loaded PCM and SDM `GetFilters` lists on one
daemon, recording whether the same name carries a different `value`, plus its list position in each.
**Do not renumber the fixture before that capture** — #341 comment 5125948432 blocks it explicitly.

### HQP-C-014 — the corpus's list positions are excerpt-local

Name-to-enum-ID pairs and the `Set*` anchors are evidenced; absolute list positions in the
enumeration excerpts are contiguous excerpt-local fillers and their provenance says so.

**What would settle it:** the tier-1 read-only capture-and-diff gate
(`tier1_live_read_only_verification_when_opted_in`) run against a daemon whose hardware matches the
fixture's `hardware` marker, then re-provenancing each fixture from the capture. L1 ran that path and
produced 144 hardware-enumeration divergences against this corpus, which is the expected result for a
different DAC chain and does **not** settle the excerpt positions for the Opal profile.

### HQP-C-019 — Auto under `[source]` reports success for an ignored command

`verify_applied` compares the readback against the requested index; requesting Auto (0) under
`[source]`, where the daemon ignores the pin and the rate is already 0, compares 0 against 0 and
reports success. The setter did nothing.

**What would settle it:** #347 introducing an `ignored` outcome distinct from `applied` and
`rejected`, at which point this row becomes a client-behaviour claim with a test rather than an
evidence question. The protocol half is already settled by HQP-C-018.

### HQP-C-021 — the rate list's dependence on the selected filter

Evidenced upstream and unmodelled here: adding it to the corpus means inventing which rates each
filter removes, which is a device- and filter-specific claim no capture supports.

**What would settle it:** one capture of `GetRates` before and after a `SetFilter` on one device,
recording which entries change. That settles it for that device; the general shape needs more.

### HQP-C-022 — can `GetRates` be empty?

#322's acceptance criterion says the rate list can be empty. Searching both audited salvage reports
returns only unrelated backup-archive and now-playing matches. **No observation supports it.**

**What would settle it:** a read-only `GetRates` capture on a daemon with no usable output configured,
or upstream confirmation that an empty list is reachable at all. If it is unreachable, #322's wording
should be corrected rather than covered.

### HQP-C-024 — `State.active_mode` under `[source]`

`Status.active_mode` echoing the configured mode is **measured** (HQP-C-023). What `State.active_mode`
reports under `[source]` is **unmeasured**, by anyone. The two are therefore *independent unresolved
semantics*, not a contradiction — an earlier reading that called them contradictory was itself wrong,
because it came from a stable-branch report claim the same project's newer companion had superseded.

**What would settle it:** #332's row *"Resolve State.active_mode versus Status.active_mode under
configured PCM/SDM and [source]/Auto with PCM and DSD sources"*, on a daemon under `[source]` with
first a PCM and then a DSD source, reading both fields at each step. Until then no global winner is
chosen here, and `the_fake_does_not_settle_the_independent_state_and_status_active_mode_semantics`
keeps the harness from choosing one either.

### HQP-C-026 — UHC's own comment states the unsettled rule as settled

`src/adapters/hqplayer.rs:2473` reads *"Use State's active_mode (INDEX) - Status's active_mode string
is unreliable"*. That is HQP-C-024's unmeasured half asserted as fact.

**What would settle it:** HQP-C-024. The comment is left in place deliberately: `src/adapters/hqplayer.rs`
is under active remediation on the base branch this PR is stacked on, and a comment-only edit there
would conflict for no behavioural gain. #347 owns the file and the semantics.

### HQP-C-029 — apply-then-drop is ambiguous delivery

Upstream, a `SetMode` on HQPlayer Embedded 6.0.4 was **accepted, logged and acted on** while the daemon
sent no response and later dropped the connection. Two consequences, and they are independent:

1. **Portable interoperability rule:** a timeout or disconnect after a write attempt means *unknown*,
   not *failed*. UHC's client already refuses to report success in that case, and refuses to retry a
   relative one-shot — `next_advances_one_track_when_the_reply_is_lost_after_the_daemon_applied_it`
   asserts exactly one side effect **and** an error.
2. **Endpoint-specific engine failure:** that a particular daemon build dropped the connection is a
   fault of that endpoint. It is kept separate here because a command call and an authoritative state
   observation are independent evidence, and conflating them would turn one rig's engine bug into a
   protocol rule.

**Playback state was not recorded upstream** for this observation, so the row says `unknown` rather
than guessing. That matters here more than elsewhere: a write during playback and a write while
stopped are the two cases #332 must separate.

**What would settle it:** #332 exercising setters during active playback on a supported rig and
recording whether any write is acknowledged only by the state change. Convergence behaviour after such
an event belongs to #329's readback loop and #332's qualification, not here.

### HQP-C-036 — is double-escaping on the wire or in the reference client?

UHC substring-scans and decodes once; the upstream client XML-parses and then decodes again. One pass
may be correct *for UHC's pipeline* and the other for a parser-first pipeline, so the observable
difference may be an artefact rather than a wire fact.

**What would settle it:** capture the raw bytes of an attribute value known to contain an entity,
without an XML parser in the path, and compare against what each pipeline yields.

### HQP-C-037 — push `Status` is not a freshness guarantee

Observed at roughly 1–2 Hz during playback, silent while stopped, and it did **not** carry an external
settings change in the inspected session. So a subscription cannot replace polling, and the client's
whole-command deadline must not be extendable by push traffic — which
`continuous_unsolicited_traffic_cannot_extend_the_command_deadline` pins by counting frames rather than
measuring time.

**What would settle it:** #332 measuring push cadence while playing and while stopped on each
supported edition, and recording whether an externally-made settings change is ever pushed.

### HQP-C-038 — the `LibraryPicture` binary interlude

Issuing `LibraryPicture size` switches the stream out of XML until exactly the advertised byte count
has been consumed. Any framer that assumes documents all the way down desynchronises at that point.

**Playback state was not recorded upstream** for this observation. It is unlikely to matter — the
interlude is a property of the command, not of the engine's state — but guessing `idle` to satisfy a
column would be the ledger inventing evidence.

**What would settle it:** UHC does not issue `LibraryPicture` today, so there is nothing to test. A
fixture and a wire-level byte-count mode are warranted when album art is implemented — #328 owns that,
and this row exists so the hazard is known **before** someone adds the call.

### HQP-C-039 — the ~156 s idle close

One observation, inherited. It is recorded as a lower bound on daemon patience, not as a timeout to
design against.

**What would settle it:** #332's measurement row across supported editions and versions.

### HQP-C-044 — daemon profile routes as a preset store

Loading a profile restarts the daemon and may leave `/backup/settings.zip` empty until a further
service restart; a named active profile may have no `hqplayerd.xml` member, a failure that resembles
the empty-backup bug and is **not** fixed by a restart.

**What would settle it:** #330's real-Embedded matrix — successful restore, invalid config rejection,
restart timeout, credential failure, empty backup, rollback — on a daemon that is expendable. Until
then UHC's durable preset store must not be the daemon's own profile routes.

### HQP-C-048 — self-generated 4321 session keys were rejected

Recorded so nobody re-derives it. The precedent tried to mint a session-authentication key for the
native port and the daemon rejected it.

**What would settle it:** an authenticated-4321 capture against a daemon configured to require
session authentication, recording what the daemon accepts. L1 needed none (HQP-C-049), so UHC has no
first-hand evidence either way.

### HQP-C-051 — `SetJunkFilter` works and UHC does not expose it

The daemon accepted the raw element and round-tripped the value on L1. The adapter has no
`set_junk_filter`, so the capability is unavailable to any UHC surface. This is a gap, not a defect.

**What would settle it:** #329 deciding whether the junk filter belongs in the live-settings surface.
The protocol half is settled (HQP-C-050).

### HQP-C-053 / HQP-C-056 — licensing actions this PR must not take

`THIRD-PARTY-NOTICES` does not exist and is **not** created here: it is maintainer-owned licensing
policy, and #348 owns it.

**What would settle it:** #348 landing the guardrail and the notices file, naming Copyright (c) 2026
Adam Goldsmith and preserving the MIT terms, before any HQPTuner implementation code is ported.

### HQP-C-061 — a de-qualification that cites this ledger back at itself

`ab18874` is careful work: it refused to let an unsupported "mid-playback" qualifier stand. But its
evidence was *this ledger's* `idle`, which was an inference from an aggregate caveat and not a reading
of the source report. Two documents now agree with each other because one of them copied the other,
which is the failure mode the `chain` field exists to make visible.

**What would settle it:** the base branch re-deciding on `.oh/issue-322-…:1549-1552` — which records
`playback active` for this probe — or someone reading the salvage report directly and settling it from
the source. Either way the fix belongs on the branch that owns those files, not here.

### HQP-C-057 — eight fixtures still carry prose in a closed-vocabulary field

The base branch's CodeRabbit remediation at `bc9158e` collapsed `source_chain` to exactly
`read-via-report` in five fixtures. Eight others still embed a paragraph in that field. Validators key
on `contains("read-via-report")`, so nothing breaks — but the field is closed-vocabulary by intent and
half of it is prose.

**What would settle it:** the same collapse applied to the remaining eight, on the branch that owns
those files. Not done here: they are base-branch files under active review, and an edit from a stacked
branch would conflict with #337's own remediation for no evidential gain.

### HQP-C-058 — L1's first-hand enumerations against the second-hand corpus

L1 captured this rig's real SDM chain. That is first-hand evidence, and it could re-provenance parts
of the corpus.

**What would settle it:** #332 owning the promotion, with the capture artefact — not a report about
it — as the input. Deliberately not done here: re-provenancing a fixture from a **PR comment about** a
run would repeat the exact error class this ledger tracks under HQP-C-055.

---

## Pending first-hand confirmation

Fixtures whose provenance records `source_chain: read-via-report` — the cited upstream file was not
read directly, so a salvage report is the immediate source. **This table is derived from the corpus,
not curated**: `the_pending_confirmation_table_is_exactly_the_second_hand_corpus` fails if it drifts
from the fixture headers in either direction.

| Fixture | Status label | Tier |
|---|---|---|
| `hqpd-6.0.4-opal/config_profile_form` | derived-excerpt | unspecified |
| `hqpd-6.0.4-opal/filters_pcm` | derived-excerpt | unspecified |
| `hqpd-6.0.4-opal/filters_sdm` | derived-excerpt | tier-2-only |
| `hqpd-6.0.4-opal/getinfo` | verified-upstream | tier-1 |
| `hqpd-6.0.4-opal/junkfilters` | derived | tier-1 |
| `hqpd-6.0.4-opal/matrix_profiles` | derived | tier-1 |
| `hqpd-6.0.4-opal/modes` | verified-upstream | tier-1 |
| `hqpd-6.0.4-opal/persistent_config` | derived-semantics | unspecified |
| `hqpd-6.0.4-opal/rates_pcm` | derived-excerpt | unspecified |
| `hqpd-6.0.4-opal/rates_sdm` | verified-upstream | unspecified |
| `hqpd-6.0.4-opal/restore_response` | derived-semantics | unspecified |
| `hqpd-6.0.4-opal/shapers_pcm` | derived-excerpt | unspecified |
| `hqpd-6.0.4-opal/shapers_sdm` | derived-excerpt | unspecified |
| `hqpd-6.0.4-opal/status_playing_with_metadata` | derived-shape | unspecified |
| `hqpd-6.0.4-pcm-only-dac/modes` | derived-upstream | tier-1 |

No fixture in this corpus claims a bare `verified`. Three claim `verified-upstream`, which means
*verified upstream and read via report* — never verified by this project.

---

## Retired claims

Kept, not deleted, so a reader arriving from an old link sees the correction.

| Retired claim | Where it appeared | Why it is wrong | Replaced by |
|---|---|---|---|
| "`SetMode` expects VALUE (−1 = `[source]`, 0 = PCM, 1 = SDM)" | `.oh/hqplayer-spec.md:57`, `:150` | The client resolves a name to a **list index** and sends that; the live 6.0.2 run round-tripped SDM→PCM→SDM with `State.mode=2` for SDM, whose enum ID is 1 | HQP-C-001 |
| "Always use State's numeric `active_mode`" | `docs/hqplayer-protocol-reference.md:186` | It settles by fiat a question nobody has measured. `State.active_mode` under `[source]` is unmeasured | HQP-C-023, HQP-C-024 |
| "`State` returns INDEX for filter/shaper — the protocol audit doc is wrong when it says send VALUE" as *the* central finding | `.oh/hqplayer-spec.md:62` | The conclusion **survives** (HQP-C-002); what is retired is the framing that this was the protocol's one hard question, which left `result`, decimal dB, and framing undocumented and shipped four defects | HQP-C-002, HQP-C-027, HQP-C-034, HQP-C-040 |
| "`<Ok/>` is what a setter answers" | `tests/mock_servers/hqplayer.rs` before #322 | A shape the daemon never sends; the daemon echoes the request element with a `result` attribute | HQP-C-027 |
| "`filter_20k` is a boolean" | pre-#322 client | It is `filter_junk`, an int index into `GetJunkFilters` | HQP-C-050 |
| "The `/hqp/discover` and LMS full-suite failures are deterministic / environment-specific / ~1-in-10" | ADR 003 earlier revisions | Withdrawn three times; both sources are intermittent in both directions at a single SHA, so a runner can establish a rate but never return a verdict | ADR 003 stage-3 section |

---

## Where to look next

| Question | Go to |
|---|---|
| What does the daemon actually say, byte for byte? | `tests/fixtures/hqplayer/<profile>/` — provenance header first |
| What does the client do about it? | `tests/hqplayer_conformance.rs`, by the test name a claim row cites |
| Why is the boundary shaped this way? | [ADR 003](adr/003-hqplayer-conformance-boundary.md) |
| How does a live run qualify a claim? | ADR 003's tier-1 runbook, and #332 for the matrix |
| What may I copy from where? | HQP-C-052 to HQP-C-056, and #348 for the standing guardrail |
| How was this ledger decided? | `.oh/hqplayer-evidence-ledger.md` |
