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
fn selection_queued_after_barrier_before_restore_must_not_retire_echo_marker() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (barrier_tx, barrier_rx) = std::sync::mpsc::channel();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let (pending_tx, pending_rx) = std::sync::mpsc::channel();
    let ordering = CaptureOrdering::default();
    ordering.install(std::sync::Arc::new(move || {
        let (complete_tx, complete_rx) = std::sync::mpsc::channel();
        barrier_tx
            .send(complete_tx)
            .map_err(|error| error.to_string())?;
        complete_rx.recv().map_err(|error| error.to_string())
    }));

    let start_tx = tx.clone();
    let item_tx = tx.clone();
    let skipped_tx = tx.clone();
    let capture = std::thread::spawn(move || {
        let mut sink = CaptureSink::new_ordered(
            move |selection_id| {
                start_tx
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
        );

        // The capture queue is empty at the sync barrier. Complete it exactly
        // as the production loop does after dispatching its current batch.
        let complete_tx: std::sync::mpsc::Sender<()> = barrier_rx.recv().unwrap();
        complete_tx.send(()).unwrap();

        // Before the restore thread can arm/set_selection, another selection
        // starts draining. A clear notification then waits behind this drain.
        let selection_id = sink.selection_started();
        started_tx.send(()).unwrap();
        finish_rx.recv().unwrap();
        sink.push_for_selection(selection_id, item(b"C"));

        // Model the compositor-ordered notifications waiting during C's drain:
        // the clear precedes the restore's own selection echo.
        for pending in pending_rx {
            let selection_id = sink.selection_started();
            match pending {
                None => sink.selection_skipped_for(selection_id),
                Some(item) => sink.push_for_selection(selection_id, item),
            }
        }

        // The post-selection barrier completes only after the delayed drain
        // and all notifications queued behind it have been published.
        let complete_tx: std::sync::mpsc::Sender<()> = barrier_rx.recv().unwrap();
        complete_tx.send(()).unwrap();
    });

    // Model preemption between synchronize() and the subsequent Arm send in
    // handle_restore_item. The semaphore blocks only other restore threads.
    ordering.synchronize().unwrap();
    started_rx.recv().unwrap();
    pending_tx.send(None).unwrap();

    let token = Uuid::now_v7();
    tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Arm {
        token,
        fingerprint: restore_capture_fingerprint(&[("text/plain".into(), b"A".to_vec())]),
    }))
    .unwrap();
    pending_tx.send(Some(item(b"A"))).unwrap();
    drop(pending_tx);
    finish_tx.send(()).unwrap();
    ordering.synchronize().unwrap();
    tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Activate {
        token,
    }))
    .unwrap();
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
