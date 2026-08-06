//! Deterministic lifecycle/projection architecture checks.
//!
//! The adapter-boundary lint prevents new surface-to-adapter bypasses. This
//! suite protects the recovery half of that contract using parsed Rust syntax:
//! every composed observer must be able to re-admit a full snapshot, Core loss
//! must remove its projection, and every coordinator/API stop route must flush
//! before cancellation.

use syn::visit::{self, Visit};
use syn::{Expr, ExprMatch, ImplItem, Item, Pat};

fn parse(source: &str) -> syn::File {
    syn::parse_file(source).expect("production Rust must parse")
}

fn named_body<'a>(file: &'a syn::File, name: &str) -> &'a syn::Block {
    fn find<'a>(items: &'a [Item], name: &str) -> Option<&'a syn::Block> {
        for item in items {
            match item {
                Item::Fn(function) if function.sig.ident == name => return Some(&function.block),
                Item::Mod(module) => {
                    if let Some((_, children)) = &module.content {
                        if let Some(block) = find(children, name) {
                            return Some(block);
                        }
                    }
                }
                Item::Impl(implementation) => {
                    for member in &implementation.items {
                        if let ImplItem::Fn(function) = member {
                            if function.sig.ident == name {
                                return Some(&function.block);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        None
    }
    if let Some(block) = find(&file.items, name) {
        return block;
    }
    panic!("missing function `{name}`")
}

#[derive(Default)]
struct SyntaxFacts {
    method_calls: Vec<String>,
    paths: Vec<String>,
}

#[derive(Default)]
struct PrefixFacts {
    prefixes: Vec<String>,
}

impl<'ast> Visit<'ast> for PrefixFacts {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "starts_with" {
            if let Some(Expr::Lit(literal)) = call.args.first() {
                if let syn::Lit::Str(prefix) = &literal.lit {
                    self.prefixes.push(prefix.value());
                }
            }
        }
        visit::visit_expr_method_call(self, call);
    }
}

impl<'ast> Visit<'ast> for SyntaxFacts {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.method_calls.push(call.method.to_string());
        visit::visit_expr_method_call(self, call);
    }

    fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
        self.paths.push(segment.ident.to_string());
        visit::visit_path_segment(self, segment);
    }
}

fn facts(block: &syn::Block) -> SyntaxFacts {
    let mut facts = SyntaxFacts::default();
    facts.visit_block(block);
    facts
}

fn expression_facts(expression: &Expr) -> SyntaxFacts {
    let mut facts = SyntaxFacts::default();
    facts.visit_expr(expression);
    facts
}

#[derive(Default)]
struct StopFacts {
    events: Vec<&'static str>,
}

impl<'ast> Visit<'ast> for StopFacts {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = call.func.as_ref() {
            if path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "stop_adapter_and_flush_zones")
            {
                self.events.push("flush");
                self.events.push("stop");
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "publish" {
            let mut arguments = SyntaxFacts::default();
            for argument in &call.args {
                arguments.visit_expr(argument);
            }
            if arguments.paths.iter().any(|path| path == "AdapterStopping") {
                self.events.push("flush");
            }
        }
        if call.method == "stop" {
            self.events.push("stop");
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn stop_events(block: &syn::Block) -> Vec<&'static str> {
    let mut facts = StopFacts::default();
    facts.visit_block(block);
    facts.events
}

fn requires_snapshot_bridge(
    source: &str,
    bridge_method: &str,
    observer_publish: &str,
    observer_name: &str,
) {
    let file = parse(source);
    let bridge = facts(named_body(&file, bridge_method));
    assert!(
        bridge.paths.iter().any(|path| path == "Snapshot"),
        "the reliable bridge must publish ProjectionKind::Snapshot, not a lossy delta"
    );
    let observer = facts(named_body(&file, observer_name));
    assert!(
        observer
            .method_calls
            .iter()
            .any(|call| call == observer_publish),
        "{observer_name} must re-publish an authoritative zone through `{observer_publish}`"
    );
}

fn pat_has_core_lost(pat: &Pat) -> bool {
    let mut facts = SyntaxFacts::default();
    facts.visit_pat(pat);
    facts.paths.iter().any(|path| path == "CoreEvent")
        && facts.paths.iter().any(|path| path == "Lost")
}

#[derive(Default)]
struct CoreLossFacts {
    removes_projection: bool,
}

impl<'ast> Visit<'ast> for CoreLossFacts {
    fn visit_expr_match(&mut self, expression: &'ast ExprMatch) {
        for arm in &expression.arms {
            if pat_has_core_lost(&arm.pat) {
                let arm_facts = expression_facts(&arm.body);
                self.removes_projection |= arm_facts
                    .method_calls
                    .iter()
                    .any(|call| call == "publish_removed");
            }
        }
        visit::visit_expr_match(self, expression);
    }
}

#[test]
fn lms_reconnect_can_republish_unchanged_players_as_snapshots() {
    requires_snapshot_bridge(
        include_str!("../src/adapters/lms.rs"),
        "publish",
        "publish_zone",
        "update_players_internal",
    );
}

#[test]
fn roon_core_loss_retires_every_projected_zone() {
    let file = parse(include_str!("../src/adapters/roon.rs"));
    let mut facts = CoreLossFacts::default();
    facts.visit_file(&file);
    assert!(
        facts.removes_projection,
        "CoreEvent::Lost must publish ZoneRemoved projections before reconnecting"
    );
}

#[test]
fn openhome_and_upnp_pollers_republish_cached_devices_as_snapshots() {
    requires_snapshot_bridge(
        include_str!("../src/adapters/openhome.rs"),
        "publish_zone",
        "publish_zone",
        "poll_device",
    );
    requires_snapshot_bridge(
        include_str!("../src/adapters/upnp.rs"),
        "publish_zone",
        "publish_zone",
        "poll_renderer",
    );
}

#[test]
fn every_production_stop_route_flushes_before_stopping() {
    for (name, source, function) in [
        (
            "settings disable",
            include_str!("../src/api/mod.rs"),
            "stop_adapter_and_flush_zones",
        ),
        (
            "LMS reconfiguration",
            include_str!("../src/api/mod.rs"),
            "lms_configure_handler",
        ),
        (
            "coordinator shutdown",
            include_str!("../src/coordinator.rs"),
            "stop_all",
        ),
    ] {
        let events = stop_events(named_body(&parse(source), function));
        let flush = events
            .iter()
            .position(|event| *event == "flush")
            .unwrap_or_else(|| panic!("{name} must publish AdapterStopping"));
        let stop = events
            .iter()
            .position(|event| *event == "stop")
            .unwrap_or_else(|| panic!("{name} must stop adapters"));
        assert!(flush < stop, "{name} must flush the projection before stop");
    }
}

#[test]
fn aggregator_subscribes_before_starting_any_adapter() {
    let facts = facts(named_body(&parse(include_str!("../src/main.rs")), "run"));
    let subscription = facts
        .method_calls
        .iter()
        .position(|call| call == "run_with_ready")
        .expect("startup must use the aggregator readiness barrier");
    let starts = facts
        .method_calls
        .iter()
        .position(|call| call == "start_all_enabled")
        .expect("startup must use AdapterCoordinator::start_all_enabled");
    assert!(
        subscription < starts,
        "adapter startup must not precede the aggregator subscription barrier"
    );
}

#[test]
fn controller_recognizes_every_prefixed_provider() {
    let file = parse(include_str!("../src/knobs/routes.rs"));
    let mut facts = PrefixFacts::default();
    facts.visit_file(&file);
    for prefix in ["roon:", "lms:", "openhome:", "upnp:", "hqplayer:"] {
        assert!(
            facts.prefixes.iter().any(|actual| actual == prefix),
            "controller does not recognize `{prefix}`"
        );
    }
    assert!(
        !facts.prefixes.iter().any(|prefix| prefix == "hqp:"),
        "hqp: is not a valid provider prefix"
    );
}
