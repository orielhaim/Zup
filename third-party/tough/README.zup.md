# Zup storage hook

Upstream: [Tough 0.24.0](https://github.com/awslabs/tough/tree/98d8eb8b2ce63515d9b4981c938ef6453c5b5771/tough).
Published archive SHA-256: `35b378d98765c2ae9cdc3e9963ea7e670da8cdd9ee39611b8d722083c7f1ac11`.

Zup needs protected native metadata publication and a cross-process repository lease. Upstream exposes a datastore path without a storage callback. This patch adds `MetadataStore` and atomic `MetadataChange` batches. Native implementations must retain their lease throughout the store's lifetime, publish each complete batch durably, and refuse uncertain outcomes.

Root updates publish the verified root and required timestamp/snapshot invalidations together. Root replacement no longer deletes the previous root first. Corrupt persisted clock evidence produces an error. Signature algorithms, canonicalization, metadata parsing, and transport verification are unchanged.

Modified upstream files: `src/lib.rs`, `src/datastore.rs`, `src/error.rs`. Added: `src/metadata_store.rs`. The path-based datastore remains an upstream interface; its filesystem writes do not provide Zup's native storage guarantees.

The delegated raw-metadata regression generates a fresh Ed25519 PKCS#8 key. Its published legacy fixture is rejected by the current crypto dependency; the same failure was reproduced in unmodified Tough with matched dependencies. Signature verification is unchanged.
