//! Lifecycle counterexamples that require the single-owner actor API (#324, revision 2).
//!
//! Every test here is deterministic by construction rather than by timing. The actor drains a
//! bounded command channel, so a test enqueues commands in the order it wants, *then* runs the
//! actor to completion. There is no sleep, no yield and no race to lose — the interleaving the
//! design has to survive is expressed directly as queue order.

use unified_hifi_control::adaptive::{
    admit_document_str, DocumentRevisions, ProducerDocument, ProducerEpoch, Timestamp,
};
use unified_hifi_control::producers::{AdaptiveRuntime, Admission, ProducerKey, ProducerPresence};

fn fixture(name: &str) -> ProducerDocument {
    let path = format!(
        "{}/tests/fixtures/adaptive/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    match admit_document_str(&raw) {
        Ok(document) => document,
        Err(refusal) => panic!("canonical fixture {name} is not admissible: {refusal}"),
    }
}

fn pipeline() -> ProducerDocument {
    fixture("hqplayer_pipeline_v1")
}

fn at(document: &ProducerDocument, epoch: u64, state: u64) -> ProducerDocument {
    let mut next = document.clone();
    next.producer.epoch = ProducerEpoch(epoch);
    next.revisions = DocumentRevisions::new(12, state);
    next
}

#[test]
#[should_panic(expected = "adapter namespace must be non-empty and contain no colon")]
fn an_adapter_run_cannot_use_an_unownable_namespace() {
    let (handle, _actor, _view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 1);
    let _ = handle.begin_run("bad:namespace");
}

/// Drain the actor to completion, asserting it finishes rather than hanging.
async fn drain(actor: unified_hifi_control::producers::ProducerActor) {
    tokio::time::timeout(std::time::Duration::from_secs(5), actor.run())
        .await
        .expect("the actor must finish draining once its command channel closes");
}

// =============================================================================
// I1: decision and mutation are one turn. Expressed as queue order.
// =============================================================================

#[tokio::test]
async fn a_publish_queued_before_its_run_ended_is_refused_when_drained_after() {
    // The TOCTOU the previous design could not close: the liveness check and the store
    // mutation were separated by an `.await`, so a run could end in between. Here the publish
    // is *already enqueued* when the run ends and a successor starts — the most hostile
    // ordering — and the actor still refuses it, because it evaluates liveness in the same
    // turn as the mutation it authorises.
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);

    let first = handle.begin_run("hqplayer");
    let document = pipeline();
    let key = ProducerKey::of(&document);
    handle
        .publish_detached(&first, document)
        .await
        .expect("enqueued");

    // Nothing has drained yet. The run ends and a successor begins.
    drop(first);
    let _second = handle.begin_run("hqplayer");

    drop(handle);
    drain(actor).await;

    assert!(
        view.snapshot(&key).is_none(),
        "a publish enqueued before its run ended was admitted after a successor started"
    );
    assert!(
        matches!(
            view.refusals().as_slice(),
            [(
                _,
                unified_hifi_control::producers::AdmissionRefusal::StaleAdapterRun { .. }
            )]
        ),
        "a stale-run refusal was not retained for detached publication: {:?}",
        view.refusals()
    );
}

#[tokio::test]
async fn a_replacement_run_cannot_change_content_without_advancing_revision() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);
    let worker = tokio::spawn(async move { actor.run().await });

    let first = handle.begin_run("hqplayer");
    let original = pipeline();
    let key = ProducerKey::of(&original);
    assert!(matches!(
        handle
            .publish(&first, original.clone())
            .await
            .expect("verdict"),
        Admission::Admitted(_)
    ));
    drop(first);

    let second = handle.begin_run("hqplayer");
    let mut contradictory = original.clone();
    contradictory.producer.product_version = Some("changed-without-revision".to_string());
    let verdict = handle
        .publish(&second, contradictory)
        .await
        .expect("verdict");
    assert!(
        matches!(
            verdict,
            Admission::Refused(
                unified_hifi_control::producers::AdmissionRefusal::NotAdvanced { .. }
            )
        ),
        "a replacement run changed content under the same revision: {verdict:?}"
    );
    assert_eq!(
        view.snapshot(&key)
            .expect("the original snapshot stands")
            .document
            .producer
            .product_version,
        original.producer.product_version
    );

    drop(second);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn a_removal_queued_before_its_run_ended_cannot_retire_the_successors_producer() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);

    let first = handle.begin_run("hqplayer");
    let document = pipeline();
    let key = ProducerKey::of(&document);

    // Beginning the successor supersedes the still-held first lease.
    let second = handle.begin_run("hqplayer");
    handle
        .publish_detached(&second, document.clone())
        .await
        .expect("enqueued");
    // A retirement attempted from that stale lease is rejected before queue commitment.
    let retirement = handle
        .retire_detached(&first, &document.producer.producer_id, ProducerEpoch(7))
        .await
        .expect("runtime open");
    assert!(matches!(
        retirement,
        unified_hifi_control::producers::RetirementOutcome::StaleAdapterRun { .. }
    ));

    drop(handle);
    drain(actor).await;

    assert!(
        view.snapshot(&key).is_some(),
        "a removal from a dead run retired the successor run's producer"
    );
    drop(first);
    drop(second);
}

#[tokio::test]
async fn a_retirement_committed_while_live_is_not_lost_when_the_lease_drops() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);
    let first = handle.begin_run("hqplayer");
    let document = pipeline();
    let key = ProducerKey::of(&document);

    // The bounded inbox accepts the retirement while run N is authoritative. It must remain
    // committed even if the lease drops before the actor gets CPU time.
    handle
        .retire_detached(&first, &document.producer.producer_id, ProducerEpoch(7))
        .await
        .expect("runtime open");
    drop(first);

    let successor = handle.begin_run("hqplayer");
    handle
        .publish_detached(&successor, document)
        .await
        .expect("successor publication accepted");
    drop(handle);
    drain(actor).await;

    assert!(
        view.snapshot(&key).is_none(),
        "a retirement accepted while live disappeared when its lease dropped"
    );
    drop(successor);
}

#[tokio::test]
async fn one_adapter_cannot_retire_another_adapters_producer_namespace() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let worker = tokio::spawn(async move { actor.run().await });
    let hqp = handle.begin_run("hqplayer");
    let roon = handle.begin_run("roon");
    let document = pipeline();
    let key = ProducerKey::of(&document);

    handle
        .publish(&hqp, document.clone())
        .await
        .expect("verdict");
    let outcome = handle
        .retire(
            &roon,
            &document.producer.producer_id,
            document.producer.epoch,
        )
        .await
        .expect("runtime open");
    assert!(
        !matches!(
            outcome,
            unified_hifi_control::producers::RetirementOutcome::Committed
                | unified_hifi_control::producers::RetirementOutcome::Retired { .. }
        ),
        "cross-adapter retirement was not refused: {outcome:?}"
    );
    assert!(view.snapshot(&key).is_some());

    drop(roon);
    drop(hqp);
    drop(handle);
    worker.await.expect("actor");
}

// =============================================================================
// I7: no ingress command is lost, even at capacity.
// =============================================================================

#[tokio::test]
async fn a_full_inbox_applies_backpressure_rather_than_losing_a_retirement() {
    // Capacity 1, deliberately. The publishes fill it and the retirement has to wait; nothing
    // is dropped and nothing is coalesced, so the retirement is applied in order.
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 1);

    let run = handle.begin_run("hqplayer");
    let document = pipeline();
    let key = ProducerKey::of(&document);

    let sender = handle.clone();
    let producer_id = document.producer.producer_id.clone();
    let feeder = tokio::spawn(async move {
        for step in 0..32_u64 {
            sender
                .publish_detached(&run, at(&pipeline(), 7, 4193 + step))
                .await
                .expect("enqueued under backpressure");
        }
        sender
            .retire(&run, &producer_id, ProducerEpoch(7))
            .await
            .expect("the retirement must not be dropped by a full inbox");
        drop(run);
        drop(sender);
    });

    let worker = tokio::spawn(async move { actor.run().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), feeder)
        .await
        .expect("the feeder must finish")
        .expect("the feeder must not panic");
    drop(handle);
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("the actor must finish")
        .expect("the actor must not panic");

    assert!(
        view.snapshot(&key).is_none(),
        "a retirement queued behind a full inbox was lost"
    );
}

#[tokio::test]
async fn a_restart_on_one_target_cannot_clear_retirement_for_an_unseen_target() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 8);
    let worker = tokio::spawn(async move { actor.run().await });
    let run = handle.begin_run("hqplayer");
    let base = pipeline();

    handle
        .retire(&run, &base.producer.producer_id, ProducerEpoch(7))
        .await
        .expect("retirement applied");

    let restarted = handle
        .publish(&run, at(&base, 8, 1))
        .await
        .expect("restart verdict");
    assert!(matches!(restarted, Admission::Admitted(_)));

    let mut unseen_straggler = at(&base, 7, 9999);
    unseen_straggler.target.role = unified_hifi_control::adaptive::TargetRole::Instance;
    unseen_straggler.target.zone_id = None;
    let refused = handle
        .publish(&run, unseen_straggler)
        .await
        .expect("straggler verdict");
    assert!(
        matches!(refused, Admission::Refused(_)),
        "a restart on one target cleared the producer-wide floor: {refused:?}"
    );
    assert!(
        matches!(
            view.refusals().as_slice(),
            [(
                _,
                unified_hifi_control::producers::AdmissionRefusal::ProducerRetired { .. }
            )]
        ),
        "a producer-retired refusal was not retained: {:?}",
        view.refusals()
    );

    drop(run);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn a_refused_future_epoch_cannot_raise_an_explicit_retirement_floor() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let worker = tokio::spawn(async move { actor.run().await });
    let run = handle.begin_run("hqplayer");
    let base = pipeline();
    let key = ProducerKey::of(&base);

    assert!(matches!(
        handle
            .publish(&run, base.clone())
            .await
            .expect("initial verdict"),
        Admission::Admitted(_)
    ));

    let mut malformed_future = at(&base, 99, 1);
    malformed_future.target.zone_id = Some("not-prefixed".to_string());
    assert!(matches!(
        handle
            .publish(&run, malformed_future)
            .await
            .expect("refusal verdict"),
        Admission::Refused(_)
    ));

    handle
        .retire(&run, &base.producer.producer_id, ProducerEpoch(7))
        .await
        .expect("retirement applied");

    let restart = handle
        .publish(&run, at(&base, 8, 1))
        .await
        .expect("restart verdict");
    assert!(
        matches!(restart, Admission::Admitted(_)),
        "a refused epoch raised the explicit retirement floor: {restart:?}"
    );
    assert_eq!(
        view.snapshot(&key)
            .expect("corrected restart admitted")
            .document
            .producer
            .epoch,
        ProducerEpoch(8)
    );

    drop(run);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn a_delayed_lower_epoch_retirement_cannot_remove_a_newer_restart() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let worker = tokio::spawn(async move { actor.run().await });
    let run = handle.begin_run("hqplayer");
    let restarted = at(&pipeline(), 9, 1);
    let key = ProducerKey::of(&restarted);

    assert!(matches!(
        handle
            .publish(&run, restarted.clone())
            .await
            .expect("restart verdict"),
        Admission::Admitted(_)
    ));
    handle
        .retire(&run, &restarted.producer.producer_id, ProducerEpoch(7))
        .await
        .expect("delayed retirement applied");

    assert_eq!(
        view.snapshot(&key)
            .expect("newer restart must remain")
            .document
            .producer
            .epoch,
        ProducerEpoch(9),
        "retirement through epoch 7 removed epoch-9 state"
    );

    drop(run);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn a_cross_namespace_refusal_cannot_poison_another_producers_retirement_floor() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let worker = tokio::spawn(async move { actor.run().await });
    let hqp = handle.begin_run("hqplayer");
    let roon = handle.begin_run("roon");
    let base = pipeline();
    let key = ProducerKey::of(&base);

    assert!(matches!(
        handle
            .publish(&hqp, base.clone())
            .await
            .expect("initial verdict"),
        Admission::Admitted(_)
    ));
    assert!(matches!(
        handle
            .publish(&roon, at(&base, 999, 1))
            .await
            .expect("ownership verdict"),
        Admission::Refused(
            unified_hifi_control::producers::AdmissionRefusal::ProducerOwnershipMismatch { .. }
        )
    ));

    handle
        .retire(&hqp, &base.producer.producer_id, ProducerEpoch(7))
        .await
        .expect("retirement applied");
    let restart = handle
        .publish(&hqp, at(&base, 8, 1))
        .await
        .expect("restart verdict");
    assert!(
        matches!(restart, Admission::Admitted(_)),
        "another adapter's refused epoch poisoned retirement authority: {restart:?}"
    );
    assert_eq!(
        view.snapshot(&key)
            .expect("corrected restart admitted")
            .document
            .producer
            .epoch,
        ProducerEpoch(8)
    );

    drop(roon);
    drop(hqp);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn a_wrong_owner_cannot_replace_the_owning_adapters_refusal() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let worker = tokio::spawn(async move { actor.run().await });
    let hqp = handle.begin_run("hqplayer");

    let mut owner_bad = pipeline();
    owner_bad.lanes[0].last_success = Some(Timestamp::new("not-an-instant"));
    assert!(matches!(
        handle
            .publish(&hqp, owner_bad)
            .await
            .expect("owner refusal"),
        Admission::Refused(
            unified_hifi_control::producers::AdmissionRefusal::LaneTimestampNotRfc3339 { .. }
        )
    ));

    // This later run has a numerically newer process-global id, but it does not own the
    // hqplayer namespace and therefore cannot displace the owner's diagnostic.
    let roon = handle.begin_run("roon");
    assert!(matches!(
        handle
            .publish(&roon, at(&pipeline(), 999, 1))
            .await
            .expect("ownership refusal"),
        Admission::Refused(
            unified_hifi_control::producers::AdmissionRefusal::ProducerOwnershipMismatch { .. }
        )
    ));
    assert!(matches!(
        view.refusals().as_slice(),
        [(
            _,
            unified_hifi_control::producers::AdmissionRefusal::LaneTimestampNotRfc3339 { .. }
        )]
    ));

    drop(roon);
    drop(hqp);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn a_wrong_owner_diagnostic_does_not_attach_to_the_owners_later_snapshot() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let worker = tokio::spawn(async move { actor.run().await });
    let roon = handle.begin_run("roon");
    let hqp = handle.begin_run("hqplayer");
    let document = pipeline();
    let key = ProducerKey::of(&document);

    assert!(matches!(
        handle
            .publish(&roon, document.clone())
            .await
            .expect("ownership verdict"),
        Admission::Refused(
            unified_hifi_control::producers::AdmissionRefusal::ProducerOwnershipMismatch { .. }
        )
    ));
    assert!(matches!(
        handle.publish(&hqp, document).await.expect("owner verdict"),
        Admission::Admitted(_)
    ));

    assert!(
        view.snapshot(&key)
            .expect("owner snapshot")
            .last_refusal
            .is_none(),
        "a command-authority failure was attached to the producer as a document refusal"
    );
    assert!(view.refusals().is_empty());

    drop(hqp);
    drop(roon);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn a_detached_wrong_owner_publication_fails_before_queueing() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);
    let roon = handle.begin_run("roon");
    let result = handle.publish_detached(&roon, pipeline()).await;
    assert!(matches!(
        result,
        Err(
            unified_hifi_control::producers::AdaptivePublishError::ProducerOwnershipMismatch { .. }
        )
    ));

    drop(roon);
    drop(handle);
    drain(actor).await;
    assert!(view.snapshots().is_empty());
    assert!(view.refusals().is_empty());
}

// =============================================================================
// I8: presence degrades when the admitting run is gone, at read time.
// =============================================================================

#[tokio::test]
async fn a_producer_whose_run_was_dropped_projects_last_known() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);

    let run = handle.begin_run("hqplayer");
    let document = pipeline();
    let key = ProducerKey::of(&document);
    handle
        .publish_detached(&run, document)
        .await
        .expect("enqueued");
    drop(handle);
    drain(actor).await;

    assert_eq!(
        view.snapshot(&key).expect("admitted").presence,
        ProducerPresence::Live,
        "a producer of a live run must be Live"
    );

    // The lease is dropped with no cleanup command drained afterwards.
    drop(run);

    assert_eq!(
        view.snapshot(&key).expect("still last-known").presence,
        ProducerPresence::LastKnown,
        "a producer whose admitting run is gone must not be presented as Live"
    );
}

#[tokio::test]
async fn a_producer_whose_run_task_was_aborted_projects_last_known() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);

    let document = pipeline();
    let key = ProducerKey::of(&document);
    let publisher = handle.clone();
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let worker = tokio::spawn(async move { actor.run().await });

    let task = tokio::spawn(async move {
        let run = publisher.begin_run("hqplayer");
        let admission = publisher.publish(&run, pipeline()).await.expect("verdict");
        assert!(matches!(admission, Admission::Admitted(_)), "admitted");
        ready_tx.send(()).expect("receiver alive");
        // Hold the lease until aborted.
        std::future::pending::<()>().await;
    });

    ready_rx.await.expect("the publisher reached its park");
    task.abort();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;

    drop(handle);
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("the actor must finish")
        .expect("the actor must not panic");

    assert_eq!(
        view.snapshot(&key).expect("admitted").presence,
        ProducerPresence::LastKnown,
        "an aborted publisher's producer was still presented as Live"
    );
}

#[tokio::test]
async fn a_producer_whose_run_task_panicked_projects_last_known() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);

    let document = pipeline();
    let key = ProducerKey::of(&document);
    let publisher = handle.clone();
    let worker = tokio::spawn(async move { actor.run().await });

    let task = tokio::spawn(async move {
        let run = publisher.begin_run("hqplayer");
        let admission = publisher.publish(&run, pipeline()).await.expect("verdict");
        assert!(matches!(admission, Admission::Admitted(_)), "admitted");
        panic!("the adapter fell over");
    });
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the task must finish");
    assert!(outcome.is_err(), "the task was expected to panic");

    drop(handle);
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("the actor must finish")
        .expect("the actor must not panic");

    assert_eq!(
        view.snapshot(&key).expect("admitted").presence,
        ProducerPresence::LastKnown,
        "a panicked publisher's producer was still presented as Live"
    );
}

// =============================================================================
// I5: cleanup and refusals are isolated by exact origin.
// =============================================================================

#[tokio::test]
async fn ending_a_dead_run_cannot_clear_the_live_runs_refusal() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);

    let dead = handle.begin_run("hqplayer");
    drop(dead);

    let live = handle.begin_run("hqplayer");
    let mut bad = pipeline();
    bad.target.zone_id = Some("not-prefixed".to_string());
    handle.publish_detached(&live, bad).await.expect("enqueued");

    drop(handle);
    drain(actor).await;

    assert_eq!(
        view.refusals().len(),
        1,
        "a dead run's cleanup cleared the live run's refusal: {:?}",
        view.refusals()
    );
    drop(live);
}

#[tokio::test]
async fn a_stale_run_cannot_overwrite_the_live_runs_refusal() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let worker = tokio::spawn(async move { actor.run().await });
    let old = handle.begin_run("hqplayer");
    let live = handle.begin_run("hqplayer");

    let mut live_bad = at(&pipeline(), 7, 4194);
    live_bad.lanes[0].last_success = Some(Timestamp::new("not-an-instant"));
    handle.publish(&live, live_bad).await.expect("live verdict");

    handle
        .publish_detached(&old, pipeline())
        .await
        .expect("stale command enqueued");

    // Wait for the queued detached refusal without using timing.
    let mut advanced = at(&pipeline(), 7, 4195);
    advanced.lanes[0].last_success = Some(Timestamp::new("still-not-an-instant"));
    handle
        .publish(&live, advanced)
        .await
        .expect("barrier verdict");
    let refusals = view.refusals();
    assert!(
        matches!(
            refusals.as_slice(),
            [(
                _,
                unified_hifi_control::producers::AdmissionRefusal::LaneTimestampNotRfc3339 { .. }
            )]
        ),
        "stale run displaced the live refusal: {refusals:?}"
    );

    drop(old);
    drop(live);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn a_verdict_is_returned_to_the_publisher_that_asked_for_one() {
    // The one desirable half of a synchronous API, without the dependency inversion: the
    // publisher holds a sender, not the aggregator, and still learns the outcome.
    let (handle, actor, _view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);
    let worker = tokio::spawn(async move { actor.run().await });

    let run = handle.begin_run("hqplayer");
    let admission = handle
        .publish(&run, pipeline())
        .await
        .expect("the verdict must come back");
    assert!(
        matches!(admission, Admission::Admitted(_)),
        "expected an admission verdict, got {admission:?}"
    );

    let mut bad = pipeline();
    bad.target.zone_id = Some("not-prefixed".to_string());
    let refused = handle.publish(&run, bad).await.expect("verdict");
    assert!(
        matches!(refused, Admission::Refused(_)),
        "expected a refusal verdict, got {refused:?}"
    );

    drop(run);
    drop(handle);
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("the actor must stop when its channel closes")
        .expect("the actor must not panic");
}

#[tokio::test]
async fn admitted_and_refused_notifications_are_egress_only_hints() {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 32);
    let mut events = view.subscribe();
    let worker = tokio::spawn(async move { actor.run().await });
    let run = handle.begin_run("hqplayer");

    handle.publish(&run, pipeline()).await.expect("verdict");
    assert!(matches!(
        events.recv().await.expect("admitted hint"),
        unified_hifi_control::producers::AdaptiveEvent::SnapshotAdmitted { .. }
    ));

    let mut bad = at(&pipeline(), 7, 4194);
    bad.target.zone_id = Some("not-prefixed".to_string());
    handle.publish(&run, bad).await.expect("verdict");
    assert!(matches!(
        events.recv().await.expect("refused hint"),
        unified_hifi_control::producers::AdaptiveEvent::SnapshotRefused { .. }
    ));

    drop(run);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn retirement_emits_a_view_invalidation_hint() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let mut events = view.subscribe();
    let worker = tokio::spawn(async move { actor.run().await });
    let run = handle.begin_run("hqplayer");
    let document = pipeline();

    handle
        .publish(&run, document.clone())
        .await
        .expect("publish");
    events.recv().await.expect("admission hint");
    handle
        .retire(
            &run,
            &document.producer.producer_id,
            document.producer.epoch,
        )
        .await
        .expect("retire");
    let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
        .await
        .expect("retirement changed the view without an egress hint")
        .expect("egress open");
    assert!(matches!(
        event,
        unified_hifi_control::producers::AdaptiveEvent::ProducerRetired { .. }
    ));

    drop(run);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn retirement_egress_reports_the_authoritative_applied_floor() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, view) = AdaptiveRuntime::build(shutdown, 32);
    let mut events = view.subscribe();
    let worker = tokio::spawn(async move { actor.run().await });
    let run = handle.begin_run("hqplayer");
    let producer_id = pipeline().producer.producer_id;

    handle
        .retire(&run, &producer_id, ProducerEpoch(9))
        .await
        .expect("initial retirement");
    events.recv().await.expect("initial retirement hint");

    handle
        .retire(&run, &producer_id, ProducerEpoch(7))
        .await
        .expect("lower retirement request");
    let event = events.recv().await.expect("retirement hint");
    assert!(matches!(
        event,
        unified_hifi_control::producers::AdaptiveEvent::ProducerRetired {
            retired_through: ProducerEpoch(9),
            ..
        }
    ));

    drop(run);
    drop(handle);
    worker.await.expect("actor");
}

#[tokio::test]
async fn the_actor_stops_on_shutting_down() {
    let shutdown = tokio_util::sync::CancellationToken::new();
    let (handle, actor, _view) = AdaptiveRuntime::build(shutdown.clone(), 32);
    let worker = tokio::spawn(async move { actor.run().await });

    shutdown.cancel();

    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .expect("the actor must stop on ShuttingDown")
        .expect("the actor must not panic");
    drop(handle);
}

#[tokio::test]
async fn shutdown_drains_commands_accepted_before_the_signal() {
    // Repeat to cover both select branch orders without relying on sleeps or scheduler timing.
    for _ in 0..32 {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (handle, actor, view) = AdaptiveRuntime::build(shutdown.clone(), 32);
        let run = handle.begin_run("hqplayer");
        let document = pipeline();
        let key = ProducerKey::of(&document);

        handle
            .publish_detached(&run, document)
            .await
            .expect("accepted before shutdown");
        shutdown.cancel();
        actor.run().await;

        assert!(
            view.snapshot(&key).is_some(),
            "shutdown discarded a command that ingress had already accepted"
        );
        drop(run);
        drop(handle);
    }
}
