//! Deterministic lifecycle/projection architecture checks.
//!
//! The adapter-boundary lint prevents new surface-to-adapter bypasses. This
//! suite protects the recovery half of that contract using parsed Rust syntax:
//! every composed observer must be able to re-admit a full snapshot, Core loss
//! must remove its projection, and every coordinator/API stop route must cover
//! every observer before it retires a shared projection.

use syn::spanned::Spanned;
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
struct AwaitFacts {
    awaits_task: bool,
}

impl<'ast> Visit<'ast> for AwaitFacts {
    fn visit_expr_await(&mut self, expression: &'ast syn::ExprAwait) {
        let mut base = SyntaxFacts::default();
        base.visit_expr(&expression.base);
        self.awaits_task |= base.paths.iter().any(|path| path == "task");
        visit::visit_expr_await(self, expression);
    }
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

fn statement_facts(statement: &syn::Stmt) -> SyntaxFacts {
    let mut facts = SyntaxFacts::default();
    facts.visit_stmt(statement);
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
                .is_some_and(|segment| segment.ident == "stop_adapter_and_flush")
            {
                self.events.push("flush");
                self.events.push("stop");
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if call.method == "stop_adapter_and_flush" {
            self.events.push("flush");
            self.events.push("stop");
        }
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
    saw_lost: bool,
    collects_all_zone_ids: bool,
    clears_cache: bool,
    retires_each_zone: bool,
    bridge_removal: bool,
    bus_fallback: bool,
    restart_after_retirement: bool,
}

impl<'ast> Visit<'ast> for CoreLossFacts {
    fn visit_expr_match(&mut self, expression: &'ast ExprMatch) {
        for arm in &expression.arms {
            if !pat_has_core_lost(&arm.pat) {
                continue;
            }
            self.saw_lost = true;
            let mut arm_facts = SyntaxFacts::default();
            arm_facts.visit_expr(&arm.body);
            self.collects_all_zone_ids = arm_facts.method_calls.iter().any(|call| call == "keys")
                && arm_facts.method_calls.iter().any(|call| call == "map")
                && arm_facts.method_calls.iter().any(|call| call == "collect")
                && arm_facts.paths.iter().any(|path| path == "PrefixedZoneId")
                && arm_facts.paths.iter().any(|path| path == "roon");
            self.clears_cache = arm_facts
                .paths
                .iter()
                .any(|path| path == "clear_roon_runtime_state");
            self.retires_each_zone = arm_facts
                .paths
                .iter()
                .any(|path| path == "removed_zone_ids")
                && arm_facts
                    .method_calls
                    .iter()
                    .any(|call| call == "publish_removed");
            self.bridge_removal = arm_facts
                .method_calls
                .iter()
                .any(|call| call == "publish_removed");
            self.bus_fallback = arm_facts.paths.iter().any(|path| path == "ZoneRemoved");
            let Expr::Block(block) = arm.body.as_ref() else {
                continue;
            };
            let clear_line = block.block.stmts.iter().find_map(|statement| {
                let facts = statement_facts(statement);
                facts
                    .paths
                    .iter()
                    .any(|path| path == "clear_roon_runtime_state")
                    .then(|| statement.span().start().line)
            });
            let retirement_line = block.block.stmts.iter().find_map(|statement| {
                let facts = statement_facts(statement);
                (facts.paths.iter().any(|path| path == "removed_zone_ids")
                    && facts
                        .method_calls
                        .iter()
                        .any(|call| call == "publish_removed")
                    && facts.paths.iter().any(|path| path == "ZoneRemoved"))
                .then(|| statement.span().start().line)
            });
            let restart_line = block.block.stmts.iter().find_map(|statement| {
                let facts = statement_facts(statement);
                facts
                    .method_calls
                    .iter()
                    .any(|call| call == "store")
                    .then(|| statement.span().start().line)
            });
            self.restart_after_retirement = self.collects_all_zone_ids
                && self.clears_cache
                && self.retires_each_zone
                && self.bridge_removal
                && self.bus_fallback
                && matches!((clear_line, retirement_line, restart_line),
                    (Some(clear), Some(retirement), Some(restart)) if clear < retirement && retirement < restart);
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

/// LMS has two observer paths.  A CLI `client disconnect` is authoritative
/// membership evidence in its own right: waiting for the slowed-down poller
/// leaves a dead zone controllable for up to one poll interval.
#[test]
fn lms_cli_disconnect_retires_its_projection_immediately() {
    let cli = facts(named_body(
        &parse(include_str!("../src/adapters/lms.rs")),
        "handle_cli_event",
    ));
    assert!(
        cli.method_calls
            .iter()
            .any(|call| call == "publish_removed"),
        "the LMS CLI disconnect path must publish a reliable removal"
    );
    assert!(
        cli.paths.iter().any(|path| path == "ZoneRemoved"),
        "the LMS CLI disconnect path must preserve the legacy bus fallback"
    );
}

#[test]
fn roon_core_loss_retires_every_projected_zone() {
    let file = parse(include_str!("../src/adapters/roon.rs"));
    let mut facts = CoreLossFacts::default();
    facts.visit_block(named_body(&file, "run_roon_loop"));
    assert!(facts.saw_lost, "run_roon_loop must handle CoreEvent::Lost");
    assert!(
        facts.collects_all_zone_ids,
        "CoreEvent::Lost must collect every cached Roon zone ID"
    );
    assert!(
        facts.clears_cache,
        "CoreEvent::Lost must clear its operational cache"
    );
    assert!(
        facts.retires_each_zone,
        "CoreEvent::Lost must retire each collected zone"
    );
    assert!(
        facts.bridge_removal,
        "CoreEvent::Lost must use the reliable removal bridge"
    );
    assert!(
        facts.bus_fallback,
        "CoreEvent::Lost must fall back to BusEvent::ZoneRemoved"
    );
    assert!(
        facts.restart_after_retirement,
        "CoreEvent::Lost must retire projections before restart"
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
    for (name, source, function) in [(
        "shared stop and flush",
        include_str!("../src/coordinator.rs"),
        "stop_adapter_and_flush",
    )] {
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
fn lms_reconfiguration_cancels_both_observers_before_retiring_lms_projection() {
    let coordinator = parse(include_str!("../src/coordinator.rs"));
    let group_stop = named_body(&coordinator, "stop_adapters_then_flush");
    let events = stop_events(group_stop);
    let first_flush = events
        .iter()
        .position(|event| *event == "flush")
        .expect("group lifecycle must retire the shared projection");
    assert!(
        events[..first_flush]
            .iter()
            .filter(|event| **event == "stop")
            .count()
            >= 1,
        "group lifecycle must cancel its observers before retirement"
    );

    let api = parse(include_str!("../src/api/mod.rs"));
    let configure = facts(named_body(&api, "lms_configure_handler"));
    assert!(
        configure
            .method_calls
            .iter()
            .any(|call| call == "stop_adapter_and_companions_then_flush"),
        "LMS reconfiguration must use the paired-observer stop path"
    );
    assert!(
        configure
            .method_calls
            .iter()
            .any(|call| call == "start_adapter_and_companions"),
        "LMS reconfiguration must restart the CLI companion with the poller"
    );
    let settings = facts(named_body(&api, "api_settings_post_handler"));
    assert!(
        settings
            .method_calls
            .iter()
            .any(|call| call == "stop_adapter_and_companions_then_flush"),
        "settings disable must stop LMS's complete observer group"
    );
    assert!(
        settings
            .method_calls
            .iter()
            .any(|call| call == "start_adapter_and_companions"),
        "settings enable must start LMS's complete observer group"
    );

    let paired_stop = facts(named_body(
        &coordinator,
        "stop_adapter_and_companions_then_flush",
    ));
    assert!(
        paired_stop.paths.iter().any(|path| path == "companions"),
        "the LMS lifecycle must include the CLI observer"
    );
    assert!(
        paired_stop
            .method_calls
            .iter()
            .any(|call| call == "stop_adapters_then_flush"),
        "the LMS pair must be retired through the coordinator"
    );
    let shutdown = facts(named_body(&coordinator, "stop_all"));
    assert!(
        shutdown
            .method_calls
            .iter()
            .any(|call| call == "stop_adapter_and_companions_then_flush"),
        "coordinator shutdown must use the paired-observer stop path"
    );

    for (name, body) in [
        (
            "LMS poller",
            named_body(
                &parse(include_str!("../src/adapters/lms.rs")),
                "stop_internal",
            ),
        ),
        (
            "LMS CLI",
            named_body(&parse(include_str!("../src/adapters/lms.rs")), "stop"),
        ),
    ] {
        let stop = facts(body);
        let mut awaits = AwaitFacts::default();
        awaits.visit_block(body);
        assert!(
            stop.method_calls.iter().any(|call| call == "take") && awaits.awaits_task,
            "{name} stop must join its supervisor, not merely signal cancellation"
        );
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
