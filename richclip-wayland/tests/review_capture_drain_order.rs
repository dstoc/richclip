use richclip::backend::{CaptureSink, CapturedFormat, CapturedItem};
use richclip_wayland::daemon::{
    CaptureMessage, RestoreSuppression, RestoreSuppressionEvent, restore_capture_fingerprint,
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
fn review_capture_started_before_restore_preserves_new_echo() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
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
        );
        let selection_id = sink.selection_started();
        // B's Selection handler is already draining the previous owner's data.
        started_tx.send(()).unwrap();
        finish_rx.recv().unwrap();
        sink.push_for_selection(selection_id, item(b"B"));
    });

    started_rx.recv().unwrap();
    let token = Uuid::now_v7();
    tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Arm {
        token,
        fingerprint: restore_capture_fingerprint(&[("text/plain".into(), b"A".to_vec())]),
    }))
    .unwrap();
    // before_selection has returned, but set_selection/flush/after_selection
    // has not finished. The older B capture completes during this interval.
    finish_tx.send(()).unwrap();
    capture.join().unwrap();
    tx.send(CaptureMessage::Restore(RestoreSuppressionEvent::Activate {
        token,
    }))
    .unwrap();
    tx.send(CaptureMessage::SelectionStarted { selection_id: 2 })
        .unwrap();
    tx.send(CaptureMessage::OrderedItem {
        selection_id: 2,
        item: item(b"A"),
    })
    .unwrap();

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
    assert_eq!(
        outcomes,
        vec![false, true],
        "capture B predates the restore; its delayed delivery must not retire A's marker"
    );
}
