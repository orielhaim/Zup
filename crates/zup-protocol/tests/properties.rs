//! What the parent/worker wire format must satisfy.
//!
//! Two processes speak this over a pipe, and the pipe is the only thing between a
//! session and a worker it did not start.
//!
//! 1. **Round trip.** `decode(encode(x)) == x`. A message that loses a field is a
//!    worker that completed a plan the parent believes it never sent.
//! 2. **The version is the version.** A newer worker must not be readable as an
//!    older one, in either direction.
//! 3. **The failure vocabulary is closed.** `Failed.kind` is a fixed set, and a
//!    kind outside it is a protocol error: the parent branches on it to decide
//!    whether to offer a retry, so an unknown kind read as a known one is the
//!    wrong advice, not a default.
//! 4. **Sequences do not repeat or go backwards**, so a replayed frame is
//!    refused and a worker cannot make a parent apply one plan twice.

use proptest::prelude::*;
use zup_protocol::{
    FAILURE_KINDS, PROTOCOL_VERSION, SequenceTracker, decode_payload, encode_payload,
};

fn check(data: &[u8]) {
    let Ok(envelope) = decode_payload(data) else {
        return;
    };
    assert_eq!(
        envelope.version, PROTOCOL_VERSION,
        "a decoded envelope kept a version this build does not speak"
    );
    let encoded = encode_payload(&envelope)
        .unwrap_or_else(|error| panic!("a decoded envelope must re-encode: {error}"));
    let reparsed = decode_payload(&encoded)
        .unwrap_or_else(|error| panic!("a re-encoded envelope must decode: {error}"));
    assert_eq!(
        envelope, reparsed,
        "the wire round trip changed the message"
    );

    if let zup_protocol::Message::Failed(failed) = &envelope.message {
        assert!(
            FAILURE_KINDS.contains(&failed.kind.as_str()),
            "`{}` is not a kind this protocol defines",
            failed.kind
        );
    }

    let mut wrong = envelope.clone();
    wrong.version = envelope.version.wrapping_add(1);
    if let Ok(bytes) = serde_json::to_vec(&wrong) {
        assert!(
            decode_payload(&bytes).is_err(),
            "an envelope for another protocol version was accepted"
        );
    }

    let mut tracker = SequenceTracker::new();
    let first = envelope.sequence;
    tracker
        .accept(first)
        .expect("the first sequence is always acceptable");
    assert!(
        tracker.accept(first).is_err(),
        "a repeated sequence was accepted"
    );
    if first > 0 {
        assert!(
            tracker.accept(0).is_err(),
            "a sequence that went backwards was accepted"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        max_shrink_iters: 4096,
        ..ProptestConfig::default()
    })]

    #[test]
    fn wire_bytes_hold_the_wire_property(data in prop::collection::vec(any::<u8>(), 0..4096)) {
        check(&data);
    }
}
