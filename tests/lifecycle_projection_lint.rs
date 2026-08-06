//! Deterministic lifecycle/projection architecture checks.
//!
//! The adapter-boundary lint prevents new surface-to-adapter bypasses. This suite
//! protects the complementary invariant: producers may only publish after the
//! ZoneAggregator is receiving events, and lifecycle helpers must retire a
//! provider's projection before stopping it.

use syn::visit::{self, Visit};
use syn::{Expr, ExprMethodCall, ExprPath, File, ItemFn, Lit};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StartupEvent {
    AggregatorReady,
    HqplayerConnect,
    StartAdapters,
}

struct StartupVisitor {
    events: Vec<StartupEvent>,
}

impl<'ast> Visit<'ast> for StartupVisitor {
    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let receiver = expression_root_ident(call.receiver.as_ref());
        match (receiver.as_deref(), call.method.to_string().as_str()) {
            (Some("zone_aggregator"), "start") => self.events.push(StartupEvent::AggregatorReady),
            (Some("hqplayer"), "get_pipeline_status") => {
                self.events.push(StartupEvent::HqplayerConnect)
            }
            (_, "start_all_enabled") => self.events.push(StartupEvent::StartAdapters),
            _ => {}
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn expression_root_ident(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(ExprPath { path, .. }) if path.segments.len() == 1 => path
            .segments
            .first()
            .map(|segment| segment.ident.to_string()),
        Expr::MethodCall(call) => expression_root_ident(call.receiver.as_ref()),
        Expr::Paren(paren) => expression_root_ident(paren.expr.as_ref()),
        Expr::Reference(reference) => expression_root_ident(reference.expr.as_ref()),
        _ => None,
    }
}

fn startup_events(source: &str) -> Vec<StartupEvent> {
    let syntax: File = syn::parse_file(source).expect("main.rs must parse");
    let run = syntax
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Mod(module) => module.content.as_ref().and_then(|(_, items)| {
                items.iter().find_map(|item| match item {
                    syn::Item::Fn(function) if function.sig.ident == "run" => Some(function),
                    _ => None,
                })
            }),
            _ => None,
        })
        .expect("server::run must exist");
    let mut visitor = StartupVisitor { events: Vec::new() };
    visitor.visit_block(&run.block);
    visitor.events
}

fn assert_aggregator_precedes_producers(events: &[StartupEvent]) -> Result<(), String> {
    let aggregator = events
        .iter()
        .position(|event| *event == StartupEvent::AggregatorReady)
        .ok_or_else(|| "ZoneAggregator::start was not awaited".to_string())?;
    for producer in [StartupEvent::HqplayerConnect, StartupEvent::StartAdapters] {
        let index = events
            .iter()
            .position(|event| *event == producer)
            .ok_or_else(|| format!("missing producer event {producer:?}"))?;
        if index < aggregator {
            return Err(format!("{producer:?} happens before ZoneAggregator::start"));
        }
    }
    Ok(())
}

struct StartsWithVisitor {
    prefixes: Vec<String>,
}

impl<'ast> Visit<'ast> for StartsWithVisitor {
    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        if call.method == "starts_with" {
            if let Some(syn::Expr::Lit(literal)) = call.args.first() {
                if let Lit::Str(prefix) = &literal.lit {
                    self.prefixes.push(prefix.value());
                }
            }
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn zone_prefixes(source: &str) -> Vec<String> {
    let syntax: File = syn::parse_file(source).expect("routes.rs must parse");
    let mut visitor = StartsWithVisitor {
        prefixes: Vec::new(),
    };
    visitor.visit_file(&syntax);
    visitor.prefixes
}

struct StopFlushVisitor {
    events: Vec<&'static str>,
}

impl<'ast> Visit<'ast> for StopFlushVisitor {
    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        if call.method == "publish" && contains_adapter_stopping(call) {
            self.events.push("flush");
        }
        if call.method == "stop" {
            self.events.push("stop");
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn contains_adapter_stopping(call: &ExprMethodCall) -> bool {
    struct EventVisitor {
        found: bool,
    }
    impl<'ast> Visit<'ast> for EventVisitor {
        fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
            if segment.ident == "AdapterStopping" {
                self.found = true;
            }
            visit::visit_path_segment(self, segment);
        }
    }
    let mut visitor = EventVisitor { found: false };
    for argument in &call.args {
        visitor.visit_expr(argument);
    }
    visitor.found
}

fn stop_flush_events(source: &str) -> Vec<&'static str> {
    let syntax: File = syn::parse_file(source).expect("api/mod.rs must parse");
    let helper = syntax
        .items
        .iter()
        .find_map(|item| match item {
            syn::Item::Fn(ItemFn { sig, block, .. })
                if sig.ident == "stop_adapter_and_flush_zones" =>
            {
                Some(block)
            }
            _ => None,
        })
        .expect("stop_adapter_and_flush_zones must exist");
    let mut visitor = StopFlushVisitor { events: Vec::new() };
    visitor.visit_block(helper);
    visitor.events
}

#[test]
fn aggregator_subscription_precedes_every_startup_producer() {
    let source = include_str!("../src/main.rs");
    assert_aggregator_precedes_producers(&startup_events(source)).unwrap();
}

#[test]
fn controller_filters_the_full_prefixed_zone_vocabulary() {
    let prefixes = zone_prefixes(include_str!("../src/knobs/routes.rs"));
    for expected in ["roon:", "lms:", "openhome:", "upnp:", "hqplayer:"] {
        assert!(
            prefixes.contains(&expected.to_string()),
            "missing {expected}"
        );
    }
    assert!(
        !prefixes.contains(&"hqp:".to_string()),
        "hqp: is not a valid PrefixedZoneId provider"
    );
}

#[test]
fn adapter_stop_flushes_its_projection_before_cancellation() {
    assert_eq!(
        stop_flush_events(include_str!("../src/api/mod.rs")),
        ["flush", "stop"],
        "stopping a provider must first publish AdapterStopping for the aggregator"
    );
}

#[test]
fn startup_lint_rejects_a_producer_before_the_aggregator() {
    let events = [
        StartupEvent::HqplayerConnect,
        StartupEvent::AggregatorReady,
        StartupEvent::StartAdapters,
    ];
    assert!(assert_aggregator_precedes_producers(&events).is_err());
}

#[test]
fn startup_lint_accepts_an_aggregator_before_every_producer() {
    let events = [
        StartupEvent::AggregatorReady,
        StartupEvent::HqplayerConnect,
        StartupEvent::StartAdapters,
    ];
    assert_aggregator_precedes_producers(&events).unwrap();
}
