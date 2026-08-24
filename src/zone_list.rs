//! The one place that decides which zones appear in a zone list, and in what order.
//!
//! # Why this is a module and not a helper inside one surface
//!
//! Zone-list *membership* used to be a fact about the aggregator: if a zone was in
//! [`ZoneAggregator`], it belonged in every list. Adapter settings were enforced upstream —
//! `register_from_settings` gates startup, and #429 made disabling an adapter flush its zones on
//! `AdapterStopping` — so a surface that read `aggregator.get_zones()` raw still produced a correct
//! answer. The adapter filter in `knobs::routes::get_all_zones_internal` was a second line of
//! defence, not the only one.
//!
//! Per-zone visibility breaks that. Hiding a zone is a *policy* the aggregator knows nothing about:
//! a hidden zone is still discovered, still updated, still controllable. From here on, any surface
//! that reads the aggregator directly to build a list silently opts out of the user's choice.
//!
//! Ordering has the same shape. `aggregator.get_zones()` returns `HashMap::values()`, so the order
//! differs *between two calls with an unchanged zone set*. The zones page compensated locally
//! (`zones.rs`), which fixed the page and left every other consumer — knobs, MCP, `/zones` —
//! with a list that reshuffles under them.
//!
//! So: every zone list goes through [`visible_zones`]. Not "should" — the guardrail for this work
//! is that no zone list calls `aggregator.get_zones()` directly.
//!
//! # What this deliberately does not do
//!
//! Hiding is a *list* filter, never a control filter. A hidden zone addressed by ID still plays,
//! pauses, and changes volume, on every surface including MCP. Knob bindings, MCP `zone_id`
//! arguments, and HQPlayer links all resolve through `aggregator.get_zone(id)`, which this module
//! does not touch. Hiding declutters; it does not deauthorise. Making "hidden" mean "unreachable"
//! would silently break working knob and assistant setups on upgrade.

use crate::api::{AppSettings, AppState};
use crate::bus::Zone;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

/// Every zone the user should be offered, in a stable order.
///
/// Applies, in order: the adapter enable filter, the per-zone hide list, then a deterministic sort.
/// This is what every zone list uses.
pub async fn visible_zones(state: &AppState) -> Vec<Zone> {
    let settings = crate::api::load_app_settings();
    let zones = state.aggregator.get_zones().await;
    apply_zone_list_policy(zones, &settings)
}

/// Every zone the user *could* be offered, hidden ones included, in the same stable order.
///
/// Exists for one caller: the settings surface that lets someone unhide a zone. Hiding must not be
/// a one-way door — if the only way to see a zone were [`visible_zones`], a hidden zone would be
/// unreachable in the very UI meant to bring it back. Not for zone lists; use [`visible_zones`].
pub async fn manageable_zones(state: &AppState) -> Vec<Zone> {
    let settings = crate::api::load_app_settings();
    let zones = state.aggregator.get_zones().await;
    apply_adapter_filter_and_sort(zones, &settings)
}

/// [`visible_zones`] with the settings and zone set passed in, so the policy is testable without an
/// `AppState`, a bus, or a config file on disk.
pub fn apply_zone_list_policy(zones: Vec<Zone>, settings: &AppSettings) -> Vec<Zone> {
    let hidden: HashSet<&str> = settings
        .hidden_zone_ids()
        .iter()
        .map(String::as_str)
        .collect();

    let mut visible = apply_adapter_filter_and_sort(zones, settings);
    visible.retain(|z| !hidden.contains(z.zone_id.as_str()));
    visible
}

/// [`manageable_zones`] with its inputs passed in.
pub fn apply_adapter_filter_and_sort(zones: Vec<Zone>, settings: &AppSettings) -> Vec<Zone> {
    let mut listed: Vec<Zone> = zones
        .into_iter()
        .filter(|z| adapter_enabled(&z.zone_id, settings))
        .map(|z| apply_custom_name(z, settings))
        .collect();

    order_zones(&mut listed, settings.zone_order_ids());
    listed
}

/// Replace a zone's display name with the user's override, if they set one.
///
/// Applied *before* [`order_zones`], deliberately. Sorting on the name the user chose is what turns
/// renaming into grouping: prefix three zones with `Basement - ` and they sort together in every
/// list, with no grouping feature to build or explain. Sorting on the provider's name instead would
/// scatter them and make the rename purely cosmetic.
///
/// Only the display name changes. `zone_id` is untouched, so knob bindings, HQPlayer links, and MCP
/// `zone_id` arguments keep working across a rename.
fn apply_custom_name(mut zone: Zone, settings: &AppSettings) -> Zone {
    if let Some(name) = settings.custom_zone_name(&zone.zone_id) {
        zone.zone_name = name.to_string();
    }
    zone
}

/// Whether the adapter owning `zone_id` is enabled in settings.
///
/// Redundant with the upstream enforcement described in the module docs, and kept anyway: settings
/// are re-read from disk on every call here, while the aggregator only learns of a change through
/// the settings endpoint. Editing `app-settings.json` directly — plausible for Docker users
/// bind-mounting a config directory — leaves the two disagreeing until restart, and this is the
/// side that reflects what the file actually says.
fn adapter_enabled(zone_id: &str, settings: &AppSettings) -> bool {
    let adapters = &settings.adapters;
    if let Some((prefix, _)) = zone_id.split_once(':') {
        match prefix {
            "roon" => adapters.roon,
            "lms" => adapters.lms,
            "openhome" => adapters.openhome,
            "upnp" => adapters.upnp,
            // `hqplayer:` is the prefix `PrefixedZoneId::hqplayer` emits and the only one HQPlayer
            // zones ever carry. This tested `hqp:` until #328, so it never matched and HQPlayer
            // zones fell through to the default-include arm — the settings toggle silently did
            // nothing for them.
            "hqplayer" => adapters.hqplayer,
            // Unknown prefix: include. A zone from a provider this build predates is better shown
            // than silently swallowed.
            _ => true,
        }
    } else {
        true
    }
}

/// Order zones: the user's explicit order first, then everything else alphabetically.
///
/// A zone named in `zone_order` sorts by its position there. A zone not named in it — never
/// reordered, or discovered after the user last arranged things — sorts after all of them, by
/// [`compare_by_name`]. That fallback is the point: a new zone appears at a stable, findable place
/// instead of wherever `HashMap` iteration happened to put it, and it does so without silently
/// rewriting an order the user set by hand.
///
/// `zone_order` may name zones that no longer exist; they simply never match.
pub fn order_zones(zones: &mut [Zone], zone_order: &[String]) {
    let rank: HashMap<&str, usize> = zone_order
        .iter()
        .enumerate()
        .map(|(index, zone_id)| (zone_id.as_str(), index))
        .collect();

    zones.sort_by(|a, b| {
        match (rank.get(a.zone_id.as_str()), rank.get(b.zone_id.as_str())) {
            (Some(left), Some(right)) => left.cmp(right),
            // Explicitly ordered zones come before ones the user has never placed.
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => compare_by_name(a, b),
        }
    });
}

/// Compare by display name, ascending, case-insensitively, with `zone_id` as tiebreaker.
///
/// Case-insensitive because a byte-order sort puts every lowercase name after every uppercase one —
/// `kitchen` lands after `Zone`, which reads as unsorted to anyone who names a zone in lowercase.
///
/// The `zone_id` tiebreaker is what makes this *deterministic* rather than merely sorted. Two zones
/// can legitimately share a display name (the same endpoint name on two providers, or two
/// identical devices), and with equal keys a stable sort preserves input order — which is
/// `HashMap::values()`, i.e. arbitrary and different next call. Zone IDs are unique, so the
/// composite key never ties.
fn compare_by_name(a: &Zone, b: &Zone) -> Ordering {
    a.zone_name
        .to_lowercase()
        .cmp(&b.zone_name.to_lowercase())
        .then_with(|| a.zone_id.cmp(&b.zone_id))
}

/// Move one zone one step up or down, returning the full resulting order as zone IDs.
///
/// Takes the *effective* order (what the user is looking at) and returns a complete list, so the
/// first reorder materialises the alphabetical order into an explicit one. Without that, moving the
/// third zone up would produce a two-element order list whose meaning depended on the alphabetical
/// fallback for everything else — and the visible result would not match what was clicked.
///
/// # Why a visible zone swaps with its nearest *visible* neighbour
///
/// The order is stored over manageable zones, hidden ones included, so that unhiding a zone
/// restores it to where the user put it. The naive consequence is that moving a visible zone "up"
/// past a hidden neighbour changes the stored order but changes nothing on any surface the user
/// looks at — `/zones`, the knobs, and MCP all filter hidden zones out. The click would appear to
/// do nothing, and the more zones you hide the more often it happens, which is precisely the user
/// this feature is for.
///
/// So a visible zone swaps with the nearest visible zone in that direction, carrying any hidden
/// zones between them along. Every click on a visible row therefore has a visible effect
/// everywhere. A hidden zone still swaps with its immediate neighbour, since "visible position" has
/// no meaning for it.
///
/// Returns `None` when the move is a no-op: the zone is absent, or there is nothing left to swap
/// with in that direction.
pub fn reorder(
    effective_order: &[Zone],
    zone_id: &str,
    direction: MoveDirection,
    hidden: &HashSet<&str>,
) -> Option<Vec<String>> {
    let index = effective_order.iter().position(|z| z.zone_id == zone_id)?;
    let target_is_hidden = hidden.contains(zone_id);

    let is_swap_candidate = |candidate: &Zone| -> bool {
        // A hidden zone has no visible position to trade, so it just takes the adjacent slot.
        target_is_hidden || !hidden.contains(candidate.zone_id.as_str())
    };

    let swap_with = match direction {
        MoveDirection::Up => effective_order[..index]
            .iter()
            .rposition(is_swap_candidate)?,
        MoveDirection::Down => {
            let offset = effective_order[index + 1..]
                .iter()
                .position(is_swap_candidate)?;
            index + 1 + offset
        }
    };

    let mut ids: Vec<String> = effective_order.iter().map(|z| z.zone_id.clone()).collect();
    let moved = ids.remove(index);
    // Re-insert at the neighbour's slot: a plain `swap` would displace the hidden zones between
    // them rather than stepping over them, so the moved zone would jump further than one place.
    ids.insert(swap_with, moved);
    Some(ids)
}

/// Which way [`reorder`] moves a zone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MoveDirection {
    Up,
    Down,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AdapterSettings;
    use crate::bus::PlaybackState;

    fn zone(zone_id: &str, zone_name: &str) -> Zone {
        Zone {
            zone_id: zone_id.to_string(),
            zone_name: zone_name.to_string(),
            state: PlaybackState::Stopped,
            volume_control: None,
            now_playing: None,
            source: zone_id.split(':').next().unwrap_or("roon").to_string(),
            is_controllable: true,
            is_seekable: false,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }

    fn all_adapters_on() -> AppSettings {
        AppSettings {
            adapters: AdapterSettings {
                roon: true,
                upnp: true,
                openhome: true,
                lms: true,
                hqplayer: true,
            },
            ..AppSettings::default()
        }
    }

    fn ids(zones: &[Zone]) -> Vec<&str> {
        zones.iter().map(|z| z.zone_id.as_str()).collect()
    }

    /// The defect users actually reported. A byte-order sort — the obvious
    /// `sort_by_key(|z| z.zone_name.clone())` — passes a fixture whose names all share a case, and
    /// fails here: it yields Attic, Kitchen, Zone, basement, den.
    #[test]
    fn sorts_case_insensitively_not_by_byte_value() {
        let settings = all_adapters_on();
        let zones = vec![
            zone("roon:1", "Zone"),
            zone("roon:2", "kitchen"),
            zone("roon:3", "Attic"),
            zone("roon:4", "den"),
            zone("roon:5", "Basement"),
        ];

        let visible = apply_zone_list_policy(zones, &settings);

        assert_eq!(
            ids(&visible),
            vec!["roon:3", "roon:5", "roon:4", "roon:2", "roon:1"],
            "expected Attic, Basement, den, kitchen, Zone"
        );
    }

    /// Equal display names must not leave order to the input.
    ///
    /// A sort without the `zone_id` tiebreaker passes every single-name fixture and fails this one:
    /// Rust's sort is stable, so equal keys preserve input order, and the input is
    /// `HashMap::values()`. Two remotes would see the two "Living Room" zones in different orders,
    /// and a knob bound to "the second Living Room" would drift between devices.
    #[test]
    fn equal_names_break_the_tie_on_zone_id_not_on_input_order() {
        let settings = all_adapters_on();

        let one = apply_zone_list_policy(
            vec![
                zone("lms:bbb", "Living Room"),
                zone("roon:aaa", "Living Room"),
            ],
            &settings,
        );
        let other = apply_zone_list_policy(
            vec![
                zone("roon:aaa", "Living Room"),
                zone("lms:bbb", "Living Room"),
            ],
            &settings,
        );

        assert_eq!(ids(&one), vec!["lms:bbb", "roon:aaa"]);
        assert_eq!(
            ids(&one),
            ids(&other),
            "the same zone set in a different input order must produce the same output order"
        );
    }

    /// Hiding keys on `zone_id`, which is prefixed by provider before it reaches here, so the
    /// filter is provider-agnostic with no per-adapter branch. A Roon-only implementation —
    /// tempting, since the request came from a Roon user — passes a Roon-only fixture and fails
    /// this one.
    #[test]
    fn hides_zones_from_every_provider() {
        let settings = AppSettings {
            hidden_zones: Some(vec![
                "roon:phone".to_string(),
                "lms:laptop".to_string(),
                "upnp:tv".to_string(),
                "openhome:spare".to_string(),
                "hqplayer:test".to_string(),
            ]),
            ..all_adapters_on()
        };
        let zones = vec![
            zone("roon:phone", "My Phone"),
            zone("roon:kitchen", "Kitchen"),
            zone("lms:laptop", "Laptop"),
            zone("upnp:tv", "TV"),
            zone("openhome:spare", "Spare"),
            zone("hqplayer:test", "Test"),
        ];

        let visible = apply_zone_list_policy(zones, &settings);

        assert_eq!(ids(&visible), vec!["roon:kitchen"]);
    }

    /// Hiding must not become an adapter filter by accident: hiding one Roon zone leaves the rest
    /// of Roon alone.
    #[test]
    fn hiding_one_zone_leaves_its_siblings_visible() {
        let settings = AppSettings {
            hidden_zones: Some(vec!["roon:phone".to_string()]),
            ..all_adapters_on()
        };
        let zones = vec![
            zone("roon:phone", "My Phone"),
            zone("roon:kitchen", "Kitchen"),
            zone("roon:study", "Study"),
        ];

        let visible = apply_zone_list_policy(zones, &settings);

        assert_eq!(ids(&visible), vec!["roon:kitchen", "roon:study"]);
    }

    /// A hide list naming a zone that no longer exists must be inert, not an error and not a
    /// filter that drops everything.
    #[test]
    fn stale_hidden_ids_are_ignored() {
        let settings = AppSettings {
            hidden_zones: Some(vec!["roon:zone-from-a-previous-core".to_string()]),
            ..all_adapters_on()
        };

        let visible = apply_zone_list_policy(vec![zone("roon:kitchen", "Kitchen")], &settings);

        assert_eq!(ids(&visible), vec!["roon:kitchen"]);
    }

    /// Default settings hide nothing. We have no trustworthy signal for which zones are private —
    /// Roon exposes none to extensions — so defaulting anything to hidden would be a guess the user
    /// cannot see us making.
    #[test]
    fn nothing_is_hidden_by_default() {
        let settings = all_adapters_on();
        assert!(settings.hidden_zone_ids().is_empty());

        let visible = apply_zone_list_policy(
            vec![
                zone("roon:phone", "My Phone"),
                zone("roon:kitchen", "Kitchen"),
            ],
            &settings,
        );

        assert_eq!(ids(&visible), vec!["roon:kitchen", "roon:phone"]);
    }

    #[test]
    fn disabled_adapters_are_still_filtered() {
        let settings = AppSettings {
            adapters: AdapterSettings {
                roon: true,
                lms: false,
                ..all_adapters_on().adapters
            },
            ..all_adapters_on()
        };

        let visible = apply_zone_list_policy(
            vec![
                zone("lms:player", "Player"),
                zone("roon:kitchen", "Kitchen"),
            ],
            &settings,
        );

        assert_eq!(ids(&visible), vec!["roon:kitchen"]);
    }

    /// The explicit order wins over the alphabet, or reordering does nothing.
    #[test]
    fn explicit_order_overrides_alphabetical() {
        let settings = AppSettings {
            zone_order: Some(vec!["roon:study".to_string(), "roon:attic".to_string()]),
            ..all_adapters_on()
        };
        let zones = vec![zone("roon:attic", "Attic"), zone("roon:study", "Study")];

        let visible = apply_zone_list_policy(zones, &settings);

        assert_eq!(ids(&visible), vec!["roon:study", "roon:attic"]);
    }

    /// A zone discovered after the user last arranged things must land somewhere findable, and must
    /// not disturb the order they set.
    ///
    /// The tempting implementation — sort the ordered ones, then leave the rest in input order —
    /// passes a fixture with one unplaced zone and fails this one: the two unplaced zones come back
    /// in `HashMap` order rather than alphabetically.
    #[test]
    fn unplaced_zones_follow_the_ordered_ones_alphabetically() {
        let settings = AppSettings {
            zone_order: Some(vec!["roon:study".to_string(), "roon:attic".to_string()]),
            ..all_adapters_on()
        };
        let zones = vec![
            zone("roon:new-b", "zeta"),
            zone("roon:attic", "Attic"),
            zone("roon:new-a", "alpha"),
            zone("roon:study", "Study"),
        ];

        let visible = apply_zone_list_policy(zones, &settings);

        assert_eq!(
            ids(&visible),
            vec!["roon:study", "roon:attic", "roon:new-a", "roon:new-b"],
            "explicit order first, then newcomers alphabetically"
        );
    }

    /// An order list naming departed zones must not shift the survivors around.
    #[test]
    fn stale_ids_in_the_order_list_are_inert() {
        let settings = AppSettings {
            zone_order: Some(vec![
                "roon:gone".to_string(),
                "roon:study".to_string(),
                "roon:also-gone".to_string(),
                "roon:attic".to_string(),
            ]),
            ..all_adapters_on()
        };
        let zones = vec![zone("roon:attic", "Attic"), zone("roon:study", "Study")];

        let visible = apply_zone_list_policy(zones, &settings);

        assert_eq!(ids(&visible), vec!["roon:study", "roon:attic"]);
    }

    /// The first reorder must write a *complete* order, not a two-element one.
    ///
    /// A `reorder` that returned only the moved pair would leave every other zone on the
    /// alphabetical fallback — so moving the third zone up would visibly reshuffle the first two as
    /// well. This asserts the whole visible order comes back.
    fn nothing_hidden() -> HashSet<&'static str> {
        HashSet::new()
    }

    #[test]
    fn reorder_materialises_the_full_order() {
        let effective = vec![
            zone("roon:a", "Attic"),
            zone("roon:b", "Basement"),
            zone("roon:c", "Cellar"),
        ];

        let moved = reorder(&effective, "roon:c", MoveDirection::Up, &nothing_hidden())
            .expect("move should apply");

        assert_eq!(moved, vec!["roon:a", "roon:c", "roon:b"]);
    }

    #[test]
    fn reorder_moves_down() {
        let effective = vec![zone("roon:a", "Attic"), zone("roon:b", "Basement")];

        let moved = reorder(&effective, "roon:a", MoveDirection::Down, &nothing_hidden())
            .expect("move should apply");

        assert_eq!(moved, vec!["roon:b", "roon:a"]);
    }

    /// Moving past either end is a no-op, not a panic and not a wrap-around.
    #[test]
    fn reorder_at_the_ends_does_nothing() {
        let effective = vec![zone("roon:a", "Attic"), zone("roon:b", "Basement")];
        let hidden = nothing_hidden();

        assert!(reorder(&effective, "roon:a", MoveDirection::Up, &hidden).is_none());
        assert!(reorder(&effective, "roon:b", MoveDirection::Down, &hidden).is_none());
        assert!(reorder(&effective, "roon:nonexistent", MoveDirection::Up, &hidden).is_none());
    }

    /// Moving a visible zone past a hidden one must change what the user can actually see.
    ///
    /// The naive `ids.swap(index, index - 1)` swaps with the immediate neighbour. When that
    /// neighbour is hidden, the stored order changes but `/zones`, the knobs, and MCP all show the
    /// identical list — the click appears to do nothing everywhere except the settings table it was
    /// clicked in. This test fails that implementation: it requires Cellar to end up above Attic,
    /// stepping over the hidden Basement.
    #[test]
    fn a_visible_zone_steps_over_hidden_neighbours() {
        let effective = vec![
            zone("roon:a", "Attic"),
            zone("roon:b", "Basement"),
            zone("roon:c", "Cellar"),
        ];
        let hidden: HashSet<&str> = ["roon:b"].into_iter().collect();

        let moved =
            reorder(&effective, "roon:c", MoveDirection::Up, &hidden).expect("move should apply");

        assert_eq!(
            moved,
            vec!["roon:c", "roon:a", "roon:b"],
            "Cellar must end up above Attic, the nearest visible neighbour"
        );

        // The visible order actually changed, which is the point.
        let settings = AppSettings {
            zone_order: Some(moved),
            hidden_zones: Some(vec!["roon:b".to_string()]),
            ..all_adapters_on()
        };
        let visible = apply_zone_list_policy(effective, &settings);
        assert_eq!(ids(&visible), vec!["roon:c", "roon:a"]);
    }

    /// A visible zone with only hidden zones above it is already first, and must report a no-op
    /// rather than shuffling hidden zones around to no visible effect.
    #[test]
    fn a_visible_zone_with_only_hidden_zones_above_it_cannot_move_up() {
        let effective = vec![
            zone("roon:hidden-1", "Aaa"),
            zone("roon:hidden-2", "Bbb"),
            zone("roon:visible", "Ccc"),
        ];
        let hidden: HashSet<&str> = ["roon:hidden-1", "roon:hidden-2"].into_iter().collect();

        assert!(
            reorder(&effective, "roon:visible", MoveDirection::Up, &hidden).is_none(),
            "already first among visible zones"
        );
    }

    /// A hidden zone has no visible position, so it trades with whatever is adjacent.
    #[test]
    fn a_hidden_zone_swaps_with_its_immediate_neighbour() {
        let effective = vec![
            zone("roon:a", "Attic"),
            zone("roon:b", "Basement"),
            zone("roon:c", "Cellar"),
        ];
        let hidden: HashSet<&str> = ["roon:c"].into_iter().collect();

        let moved =
            reorder(&effective, "roon:c", MoveDirection::Up, &hidden).expect("move should apply");

        assert_eq!(moved, vec!["roon:a", "roon:c", "roon:b"]);
    }

    /// Hiding a zone must not lose its place. The order is computed over *manageable* zones —
    /// hidden included — so unhiding puts it back where the user had it rather than at the end.
    #[test]
    fn hidden_zones_keep_their_place_in_the_order() {
        let ordered = vec![
            "roon:c".to_string(),
            "roon:b".to_string(),
            "roon:a".to_string(),
        ];
        let zones = || {
            vec![
                zone("roon:a", "Attic"),
                zone("roon:b", "Basement"),
                zone("roon:c", "Cellar"),
            ]
        };

        let while_hidden = AppSettings {
            zone_order: Some(ordered.clone()),
            hidden_zones: Some(vec!["roon:b".to_string()]),
            ..all_adapters_on()
        };
        assert_eq!(
            ids(&apply_zone_list_policy(zones(), &while_hidden)),
            vec!["roon:c", "roon:a"]
        );

        let after_unhiding = AppSettings {
            zone_order: Some(ordered),
            hidden_zones: Some(vec![]),
            ..all_adapters_on()
        };
        assert_eq!(
            ids(&apply_zone_list_policy(zones(), &after_unhiding)),
            vec!["roon:c", "roon:b", "roon:a"],
            "unhidden zone returns to its placed position, not to the end"
        );
    }

    /// The management view shows hidden zones; every zone *list* does not. Without this, the only
    /// screen able to unhide a zone could not see it.
    #[test]
    fn the_management_view_shows_what_lists_hide() {
        let settings = AppSettings {
            hidden_zones: Some(vec!["roon:phone".to_string()]),
            ..all_adapters_on()
        };
        let zones = || {
            vec![
                zone("roon:phone", "My Phone"),
                zone("roon:kitchen", "Kitchen"),
            ]
        };

        assert_eq!(
            ids(&apply_zone_list_policy(zones(), &settings)),
            vec!["roon:kitchen"]
        );
        assert_eq!(
            ids(&apply_adapter_filter_and_sort(zones(), &settings)),
            vec!["roon:kitchen", "roon:phone"]
        );
    }

    fn named(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(id, name)| ((*id).to_string(), (*name).to_string()))
            .collect()
    }

    /// Renaming must happen before sorting, or it is cosmetic only.
    ///
    /// This is the whole point of the feature: prefixing zones with a common string groups them,
    /// with no grouping feature. An implementation that renames during the final projection to
    /// `ZoneInfo` — the obvious place, right where the name is displayed — passes any test that
    /// only checks the name and fails this one, because the list is still ordered by the provider's
    /// names.
    #[test]
    fn renaming_groups_zones_because_it_happens_before_sorting() {
        let settings = AppSettings {
            zone_names: Some(named(&[
                ("roon:kitchen", "Basement - Kitchen"),
                ("roon:workshop", "Basement - Workshop"),
            ])),
            ..all_adapters_on()
        };
        let zones = vec![
            zone("roon:attic", "Attic"),
            zone("roon:kitchen", "Kitchen"),
            zone("roon:lounge", "Lounge"),
            zone("roon:workshop", "Workshop"),
        ];

        let visible = apply_zone_list_policy(zones, &settings);

        assert_eq!(
            visible
                .iter()
                .map(|z| z.zone_name.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Attic",
                "Basement - Kitchen",
                "Basement - Workshop",
                "Lounge"
            ],
            "the two renamed zones must sort together under their shared prefix"
        );
    }

    /// A rename changes the display name and nothing else. Zone IDs are what knob bindings,
    /// HQPlayer links, and MCP `zone_id` arguments address, so a rename that touched them would
    /// break working setups.
    #[test]
    fn renaming_leaves_zone_ids_untouched() {
        let settings = AppSettings {
            zone_names: Some(named(&[("roon:kitchen", "Somewhere Else")])),
            ..all_adapters_on()
        };

        let visible = apply_zone_list_policy(vec![zone("roon:kitchen", "Kitchen")], &settings);

        assert_eq!(ids(&visible), vec!["roon:kitchen"]);
        assert_eq!(visible[0].zone_name, "Somewhere Else");
    }

    /// An override naming a zone that no longer exists must be inert.
    #[test]
    fn stale_renames_are_ignored() {
        let settings = AppSettings {
            zone_names: Some(named(&[("roon:gone", "Ghost")])),
            ..all_adapters_on()
        };

        let visible = apply_zone_list_policy(vec![zone("roon:kitchen", "Kitchen")], &settings);

        assert_eq!(visible[0].zone_name, "Kitchen");
    }

    /// An explicit order still beats the renamed alphabet — renaming groups zones only where the
    /// user has not placed them by hand.
    #[test]
    fn an_explicit_order_still_outranks_a_rename() {
        let settings = AppSettings {
            zone_names: Some(named(&[("roon:z", "Aaa")])),
            zone_order: Some(vec!["roon:a".to_string(), "roon:z".to_string()]),
            ..all_adapters_on()
        };
        let zones = vec![zone("roon:z", "Zed"), zone("roon:a", "Attic")];

        let visible = apply_zone_list_policy(zones, &settings);

        assert_eq!(ids(&visible), vec!["roon:a", "roon:z"]);
    }

    /// A provider this build predates should appear rather than vanish.
    #[test]
    fn unknown_prefixes_are_included() {
        let settings = all_adapters_on();

        let visible = apply_zone_list_policy(vec![zone("someday:new", "New Thing")], &settings);

        assert_eq!(ids(&visible), vec!["someday:new"]);
    }
}
