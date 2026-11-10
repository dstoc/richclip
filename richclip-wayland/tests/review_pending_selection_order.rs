use richclip::backend::{CaptureSink, CapturedFormat, CapturedItem};
use richclip_wayland::daemon::{
    CaptureMessage, CaptureOrdering, RestoreSuppression, RestoreSuppressionEvent,
    restore_capture_fingerprint,
};
use uuid::Uuid;

fn item(bytes: &[u8]) -> CapturedItem {
    CapturedItem {
        formats: vec![CapturedFormat {
            mime: "text/plain".into(),
            bytes: bytes.to_vec(),
        }],
    }
}

#[test]
fn pending_clear_is_dispatched_before_restore_marker_is_armed() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let (selection_tx, selection_rx) = std::sync::mpsc::channel();
    let (barrier_tx, barrier_rx) = std::sync::mpsc::channel();
    let (barrier_started_tx, barrier_started_rx) = std::sync::mpsc::channel();

    let capture_ordering = CaptureOrdering::default();
    let capture_ordering_ready = capture_ordering.clone();
    let started_event_tx = tx.clone();
    let item_tx = tx.clone();
    let skipped_tx = tx.clone();
    let capture = std::thread::spawn(move || {
        let mut sink = CaptureSink::new_ordered(
            move |selection_id| {
                started_event_tx
                    .send(CaptureMessage::SelectionStarted { selection_id })
                    .unwrap();
            },
            move |selection_id, item| {
                item_tx
                    .send(CaptureMessage::OrderedItem { selection_id, item })
                    .unwrap();
            },
            move |selection_id| {
                skipped_tx
                    .send(CaptureMessage::OrderedSelectionSkipped { selection_id })
                    .unwrap();
            },
        )
        .with_ordering_barrier_handler(move |barrier| capture_ordering_ready.install(barrier));
        sink.capture_initialized(std::sync::Arc::new(move || {
            barrier_started_tx
                .send(())
                .map_err(|error| error.to_string())?;
            let (complete_tx, complete_rx) = std::sync::mpsc::channel();
            barrier_tx
                .send(complete_tx)
                .map_err(|error| error.to_string())?;
            complete_rx.recv().map_err(|error| error.to_string())
        }));

        // Selection C has already started and is blocked while it drains.
        let selection_id = sink.selection_started();
        started_tx.send(()).unwrap();
        finish_rx.recv().unwrap();
        sink.push_for_selection(selection_id, item(b"C"));

        // A capture-connection roundtrip completes only after already pending
        // selection notifications have been dispatched. Model that event-loop
        // behavior by draining the pending notification queue before acking.
        let barrier_ack = barrier_rx.recv().unwrap();
        while let Ok(pending_selection) = selection_rx.try_recv() {
            let selection_id = sink.selection_started();
            match pending_selection {
                None => sink.selection_skipped_for(selection_id),
                Some(item) => sink.push_for_selection(selection_id, item),
            }
        }
        barrier_ack.send(()).unwrap();

        for pending_selection in selection_rx {
            let selection_id = sink.selection_started();
            match pending_selection {
                None => sink.selection_skipped_for(selection_id),
                Some(item) => sink.push_for_selection(selection_id, item),
            }
        }
    });

    started_rx.recv().unwrap();
    // The clipboard clear happens during C's drain and is waiting on the
    // capture connection when restore A requests its ordering barrier.
    selection_tx.send(None).unwrap();
    let ordering = capture_ordering.clone();
    let restore = std::thread::spawn(move || ordering.synchronize());
    barrier_started_rx.recv().unwrap();
    finish_tx.send(()).unwrap();
    restore.join().unwrap().unwrap();

    let token = Uuid::now_v7();
    let restored = restore_capture_fingerprint(&[("text/plain".into(), b"A".to_vec())]);
    tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Arm {
        token,
        fingerprint: restored,
    }))
    .unwrap();
    tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Activate {
        token,
    }))
    .unwrap();

    // The restore selection follows the barrier and must consume its marker.
    selection_tx.send(Some(item(b"A"))).unwrap();
    drop(selection_tx);
    capture.join().unwrap();

    let suppression = RestoreSuppression::default();
    let mut outcomes = Vec::new();
    while let Ok(message) = rx.try_recv() {
        message.apply_suppression_updates(&suppression);
        if let CaptureMessage::OrderedItem { selection_id, item } = message {
            let formats: Vec<_> = item
                .formats
                .into_iter()
                .map(|format| (format.mime, format.bytes))
                .collect();
            outcomes.push(
                suppression
                    .observe_capture_for(selection_id, &restore_capture_fingerprint(&formats)),
            );
        }
    }
    assert_eq!(outcomes, vec![false, true]);
}
