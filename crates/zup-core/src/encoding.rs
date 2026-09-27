//! Base64 for binary trust material carried inside JSON documents.
//!
//! A TUF root is a few kilobytes of signed JSON. Inside a document that is
//! itself JSON, a byte array renders as one number per byte, which triples the
//! size of a thin artifact's trust block for no reason. Standard base64 with
//! padding is the smallest correct encoding, and this is a document format
//! rather than a transport, so there is nothing to negotiate.
//!
//! The values here are never secret: a root is public by definition, and the
//! signature over it is what is checked.

use base64::Engine;

/// Encode bytes as padded standard base64.
pub fn base64_encode(input: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(input)
}

/// Decode padded standard base64.
pub fn base64_decode(input: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::STANDARD.decode(input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_tuf_root() {
        let root = br#"{"_type":"root","signed":{"version":1}}"#;
        assert_eq!(base64_decode(&base64_encode(root)).expect("decodes"), root);
    }

    #[test]
    fn round_trips_every_tail_length() {
        for length in 0..=4usize {
            let bytes: Vec<u8> = (0..length).map(|index| index as u8 * 61).collect();
            assert_eq!(
                base64_decode(&base64_encode(&bytes)).expect("decodes"),
                bytes
            );
        }
    }

    #[test]
    fn refuses_a_malformed_value() {
        assert!(base64_decode("not base64!").is_err());
        assert!(base64_decode("YW5").is_err());
    }
}
