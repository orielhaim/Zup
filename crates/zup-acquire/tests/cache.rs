//! The cache: what it guarantees about bytes on disk.
//!
//! Everything here is a claim about hostile or damaged local state, because a
//! cache is the one component whose input the machine itself produced and might
//! therefore be wrong.

mod common;

use common::*;
use std::fs;
use zup_acquire::{
    CachePolicy, CacheProbe, ContentCache, ContentCompression, ContentDescriptor, ContentKind,
    RESUME_RECORD_INTERVAL, Verify,
};
use zup_core::Sha256Digest;

#[test]
fn a_verified_blob_round_trips_through_the_cache() {
    let cache = TestCache::new();
    let cache = cache.open(CachePolicy::Keep);
    let logical = payload(1, 96 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let wire = wire_of(&logical, LEVEL);

    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer.write(&wire).expect("the wire form is accepted");
    let blob = writer.commit().expect("the blob verifies");

    assert_eq!(blob.descriptor.digest, descriptor.digest);
    assert_eq!(blob.wire_size, descriptor.compressed_size);
    assert_eq!(blob.read_to_end().expect("the blob decodes"), logical);
}

#[test]
fn a_blob_is_only_published_after_its_digest_matches() {
    let cache = TestCache::new();
    let cache = cache.open(CachePolicy::Keep);
    let logical = payload(2, 4096);
    let descriptor = payload_descriptor(&logical, LEVEL);
    // The right length on the wire, the wrong content.
    let wire = wire_of(&payload(99, 4096), LEVEL);

    let paths = cache.paths(&descriptor).expect("the blob has paths");
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer.write(&wire).expect("the wire form is accepted");
    let error = writer.commit().expect_err("a wrong digest cannot publish");
    assert!(
        matches!(error, zup_acquire::CacheError::DigestMismatch { .. }),
        "{error}"
    );

    assert!(
        !paths.final_path.exists(),
        "a refused blob is never published"
    );
    assert!(
        !paths.partial_path.exists(),
        "a refused blob leaves no partial"
    );
}

#[test]
fn a_truncated_blob_is_refused_and_leaves_nothing_behind() {
    let cache = TestCache::new();
    let cache = cache.open(CachePolicy::Keep);
    let logical = payload(3, 64 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let mut wire = wire_of(&logical, LEVEL);
    wire.truncate(wire.len() / 2);

    let paths = cache.paths(&descriptor).expect("the blob has paths");
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer
        .write(&wire)
        .expect("the short wire form is accepted");
    let error = writer.commit().expect_err("a short blob cannot publish");
    assert!(
        matches!(error, zup_acquire::CacheError::SizeMismatch { .. }),
        "{error}"
    );
    assert!(!paths.final_path.exists());
    assert!(!paths.partial_path.exists());
}

#[test]
fn a_blob_longer_than_its_descriptor_is_refused_mid_write() {
    let cache = TestCache::new();
    let cache = cache.open(CachePolicy::Keep);
    let logical = payload(4, 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    let error = writer
        .write(&vec![0u8; descriptor.compressed_size as usize + 1])
        .expect_err("a source cannot overrun the declared wire length");
    assert!(
        matches!(error, zup_acquire::CacheError::Overflow { .. }),
        "{error}"
    );
    writer.abandon();
}

#[test]
fn a_lying_descriptor_is_refused_before_a_byte_is_written() {
    let cache = TestCache::new();
    let cache = cache.open(CachePolicy::Keep);
    let logical = payload(5, 2048);
    let descriptor = lying_descriptor(&logical, LEVEL);
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    let writer = cache.writer(&descriptor).expect("the writer opens");
    let error = writer
        .commit()
        .expect_err("an empty transfer cannot publish");
    assert!(
        matches!(error, zup_acquire::CacheError::SizeMismatch { .. }),
        "{error}"
    );
    assert!(!paths.final_path.exists());
}

#[test]
fn a_partial_transfer_resumes_and_the_prefix_is_re_hashed() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let logical = compressible(6, 4 * 1024 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let wire = wire_of(&logical, LEVEL);
    assert!(
        wire.len() < logical.len(),
        "the fixture must be compressible for a resume to be meaningful"
    );
    let cut = (wire.len() / 3) as u64;

    // First process: write a prefix, record it, and die without committing.
    {
        let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
        let paths = cache.paths(&descriptor).expect("the blob has paths");
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer
            .write(&wire[..cut as usize])
            .expect("the prefix is accepted");
        drop(writer);
        assert!(paths.partial_path.exists(), "a partial is on disk");
        assert!(paths.resume_path.exists(), "a resume record is on disk");
        assert!(!paths.final_path.exists());
        // The record is a few hundred bytes, not a copy of anything.
        assert!(
            fs::metadata(&paths.resume_path)
                .expect("the record is readable")
                .len()
                < 4096,
            "resume metadata is bounded"
        );
    }

    // Second process: the writer continues where the first stopped.
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache reopens");
    assert_eq!(
        cache
            .probe(&descriptor, Verify::Full)
            .expect("the cache probes"),
        CacheProbe::Resumable { wire_offset: cut },
        "the partial is offered for resumption"
    );
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    assert_eq!(writer.wire_offset(), cut, "the writer continues at the cut");
    writer
        .write(&wire[cut as usize..])
        .expect("the remainder is accepted");
    let blob = writer.commit().expect("the resumed blob verifies");
    assert_eq!(blob.read_to_end().expect("the blob decodes"), logical);
}

#[test]
fn a_resume_record_for_another_descriptor_is_discarded() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let one = compressible(7, 1024 * 1024);
    let two = compressible(8, 1024 * 1024);
    let first = payload_descriptor(&one, LEVEL);
    let second = payload_descriptor(&two, LEVEL);
    let wire = wire_of(&one, LEVEL);

    {
        let mut writer = cache.writer(&first).expect("the writer opens");
        writer
            .write(&wire[..wire.len() / 2])
            .expect("the prefix lands");
        drop(writer);
    }
    // A writer for a different blob must not adopt the first one's partial.
    let paths = cache.paths(&second).expect("the blob has paths");
    assert!(!paths.partial_path.exists());
    let writer = cache.writer(&second).expect("the writer opens");
    assert_eq!(writer.wire_offset(), 0, "a foreign partial is not resumed");
    writer.abandon();
}

#[test]
fn a_tampered_partial_prefix_is_measured_and_refused() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let logical = compressible(9, 2 * 1024 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let wire = wire_of(&logical, LEVEL);
    let cut = (wire.len() / 2) as u64;

    {
        let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer
            .write(&wire[..cut as usize])
            .expect("the prefix lands");
        drop(writer);
    }
    // Something outside the process rewrites the middle of the partial.
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache reopens");
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&paths.partial_path)
            .expect("the partial opens");
        file.seek(SeekFrom::Start(cut / 2)).expect("the seek lands");
        file.write_all(&[0xff; 32]).expect("the tamper lands");
    }
    let writer = cache.writer(&descriptor).expect("the writer opens");
    assert_eq!(
        writer.wire_offset(),
        0,
        "a partial whose prefix does not measure is discarded, not resumed"
    );
    writer.abandon();
}

#[test]
fn a_partial_longer_than_its_record_is_cut_back() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let logical = compressible(10, 2 * 1024 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let wire = wire_of(&logical, LEVEL);
    let cut = (wire.len() / 2) as u64;

    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    {
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer
            .write(&wire[..cut as usize])
            .expect("the prefix lands");
        drop(writer);
    }
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    fs::write(&paths.partial_path, &wire[..(cut as usize) + 4096]).expect("the tail is extended");

    let writer = cache.writer(&descriptor).expect("the writer opens");
    assert_eq!(
        writer.wire_offset(),
        cut,
        "bytes past the record's offset are not trusted"
    );
    writer.abandon();
}

#[test]
fn a_stored_blob_whose_digest_no_longer_matches_is_treated_as_absent() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let logical = payload(11, 8192);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    fs::create_dir_all(paths.parent()).expect("the directory exists");
    // A same-length file with different content: the only attack a length check
    // alone would miss.
    fs::write(&paths.final_path, wire_of(&payload(12, 8192), LEVEL))
        .expect("the impostor is written");

    assert_eq!(
        cache
            .probe(&descriptor, Verify::Full)
            .expect("the cache probes"),
        CacheProbe::Absent,
        "a full verification refuses an impostor"
    );
    assert!(
        !paths.final_path.exists(),
        "an entry that does not match its own name is not an entry"
    );
}

#[test]
fn a_payload_blob_is_length_checked_and_a_document_is_fully_verified() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let logical = payload(13, 4096);
    let payload = payload_descriptor(&logical, LEVEL);
    let document_bytes = b"{\"schema\":1}".to_vec();
    let document = zup_acquire::ContentDescriptor::stored(
        ContentKind::Metadata,
        digest_of(&document_bytes),
        document_bytes.len() as u64,
    );

    assert_eq!(
        Verify::for_kind(ContentKind::Payload),
        Verify::WireLength,
        "payload is checked by length"
    );
    for descriptor in [payload, document] {
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer
            .write(&if descriptor.kind() == ContentKind::Payload {
                wire_of(&logical, LEVEL)
            } else {
                document_bytes.clone()
            })
            .expect("the bytes land");
        writer.commit().expect("the blob verifies");
    }
    assert!(
        cache
            .get(&document, Verify::Full)
            .expect("the probe runs")
            .is_some()
    );
    assert!(
        cache
            .get(&payload, Verify::WireLength)
            .expect("the probe runs")
            .is_some()
    );
}

#[test]
fn a_reader_refuses_to_return_bytes_it_cannot_prove() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let descriptor =
        zup_acquire::ContentDescriptor::stored(ContentKind::Metadata, digest_of(b"authentic"), 9);
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer.write(b"authentic").expect("the bytes land");
    writer.commit().expect("the blob verifies");

    // Corrupt the published file, then try to read it through a fresh handle.
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    fs::write(&paths.final_path, b"tampered!").expect("the tamper lands");
    let blob = zup_acquire::VerifiedBlob {
        descriptor,
        path: paths.final_path,
        wire_size: 9,
    };
    let error = blob
        .read_to_end()
        .expect_err("a tampered blob cannot be read as verified");
    assert!(
        matches!(error, zup_acquire::CacheError::DigestMismatch { .. }),
        "{error}"
    );
}

#[test]
fn a_link_inside_the_cache_is_refused() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let _outside = tempfile::tempdir().expect("a temporary directory");
    let logical = payload(14, 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let hex = descriptor.digest.to_hex();
    let fanout = dir.path().join("blobs").join("sha256").join(&hex[..2]);
    fs::create_dir_all(&fanout).expect("the fanout exists");

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(_outside.path(), fanout.join(&hex[2..]))
            .expect("the link is created");
        let cache = ContentCache::open(dir.path(), CachePolicy::Keep);
        assert!(
            cache.is_err(),
            "a link in the blob tree must stop the cache from opening"
        );
    }
    #[cfg(not(unix))]
    {
        // A directory where a blob belongs is the portable equivalent of a
        // link: the cache must not treat it as content.
        fs::create_dir_all(fanout.join(&hex[2..])).expect("the impostor directory exists");
        let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
        assert_eq!(
            cache
                .probe(&descriptor, Verify::Full)
                .expect("the probe runs"),
            CacheProbe::Absent
        );
    }
}

#[test]
fn a_second_writer_does_not_block_the_first_and_correctness_does_not_depend_on_it() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let logical = payload(15, 4096);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let wire = wire_of(&logical, LEVEL);

    let first = cache.writer(&descriptor).expect("the first writer opens");
    let error = cache
        .writer(&descriptor)
        .err()
        .expect("a second writer is told the blob is reserved");
    assert!(
        matches!(error, zup_acquire::CacheError::Reserved { .. }),
        "{error}"
    );
    drop(first);

    // Once the claim is released, the blob is available again.
    let mut writer = cache.writer(&descriptor).expect("the writer opens again");
    writer.write(&wire).expect("the bytes land");
    writer.commit().expect("the blob verifies");
}

#[test]
fn a_temporary_cache_drops_payload_but_protects_what_a_closure_needs() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Temporary).expect("the cache opens");
    let keep = payload(16, 1024);
    let drop_me = payload(17, 1024);
    let keep_descriptor = payload_descriptor(&keep, LEVEL);
    let drop_descriptor = payload_descriptor(&drop_me, LEVEL);
    for (logical, descriptor) in [(&keep, &keep_descriptor), (&drop_me, &drop_descriptor)] {
        let mut writer = cache.writer(descriptor).expect("the writer opens");
        writer
            .write(&wire_of(logical, LEVEL))
            .expect("the bytes land");
        writer.commit().expect("the blob verifies");
    }
    let removed = cache
        .enforce_policy(&[keep_descriptor.digest])
        .expect("the policy applies");
    assert_eq!(removed, 1);
    assert!(
        cache
            .get(&keep_descriptor, Verify::WireLength)
            .expect("the probe runs")
            .is_some()
    );
    assert!(
        cache
            .get(&drop_descriptor, Verify::WireLength)
            .expect("the probe runs")
            .is_none()
    );
}

#[test]
fn a_kept_cache_retains_everything() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let logical = payload(18, 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer
        .write(&wire_of(&logical, LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");
    assert_eq!(cache.enforce_policy(&[]).expect("the policy applies"), 0);
    assert!(
        cache
            .get(&descriptor, Verify::WireLength)
            .expect("the probe runs")
            .is_some()
    );
}

#[test]
fn the_cache_shares_content_across_applications_because_identity_is_cryptographic() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    // Two "applications" with a byte-identical file. Nothing in the cache names
    // an application, which is the point.
    let shared = payload(19, 20 * 1024);
    let descriptor = payload_descriptor(&shared, LEVEL);
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer
        .write(&wire_of(&shared, LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");
    assert_eq!(cache.digests().expect("the cache lists").len(), 1);
    assert_eq!(
        cache.stored_size().expect("the cache measures"),
        descriptor.compressed_size
    );
}

#[test]
fn a_stored_document_is_carried_without_compression() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let bytes = b"{\"schema\":1,\"variants\":[]}".to_vec();
    let descriptor = zup_acquire::ContentDescriptor::stored(
        ContentKind::Metadata,
        digest_of(&bytes),
        bytes.len() as u64,
    );
    assert_eq!(descriptor.compression, ContentCompression::None);
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer.write(&bytes).expect("the bytes land");
    let blob = writer.commit().expect("the document verifies");
    assert_eq!(blob.read_to_end().expect("the document decodes"), bytes);
}

#[test]
fn a_resume_record_interval_costs_a_bounded_amount_of_redownload() {
    // The record is rewritten on this interval, so an unclean exit costs at
    // most this many bytes. The number is part of the design, not an accident.
    // The record is rewritten on this interval, so an unclean exit costs at most
    // this many bytes. The bound is part of the design, not an accident.
    const { assert!(RESUME_RECORD_INTERVAL > 0) };
    const { assert!(RESUME_RECORD_INTERVAL <= 64 * 1024 * 1024) };
}

#[test]
fn a_digest_that_is_not_hex_is_not_a_cache_entry() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let fanout = dir.path().join("blobs").join("sha256").join("zz");
    fs::create_dir_all(&fanout).expect("the fanout exists");
    fs::write(fanout.join("not-a-digest"), b"x").expect("the file lands");
    fs::write(fanout.join(format!("{}.partial", "a".repeat(64))), b"x").expect("the file lands");
    assert!(cache.digests().expect("the cache lists").is_empty());
}

#[test]
fn a_descriptor_for_the_wrong_content_never_produces_a_readable_blob() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let real = payload(20, 4096);
    let claimed = payload(21, 4096);
    // A descriptor that describes content nobody is going to send.
    let descriptor = payload_descriptor(&claimed, LEVEL);
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer
        .write(&wire_of(&real, LEVEL))
        .expect("the bytes land");
    let error = writer
        .commit()
        .expect_err("mismatched content cannot publish");
    assert!(
        matches!(error, zup_acquire::CacheError::DigestMismatch { .. }),
        "{error}"
    );
    assert!(
        cache
            .get(&descriptor, Verify::Full)
            .expect("the probe runs")
            .is_none()
    );
}

#[test]
fn an_explicit_compression_field_is_required_to_read_a_blob() {
    // A payload whose compressed form happens to be the same length as its
    // logical form is not distinguishable by size alone, which is why the
    // compression is a field rather than an inference.
    let logical = payload(22, 64);
    let descriptor = payload_descriptor(&logical, LEVEL);
    assert!(descriptor.is_compressed());
    assert_eq!(descriptor.compression, ContentCompression::Zstandard);

    let document = ContentDescriptor::stored(ContentKind::Metadata, digest_of(&logical), 64);
    assert!(!document.is_compressed());
    assert_eq!(document.compression, ContentCompression::None);
}

#[test]
fn a_zero_length_or_oversized_descriptor_is_refused() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let empty = ContentDescriptor::stored(ContentKind::Payload, digest_of(b"x"), 0);
    assert!(empty.validate().is_err());
    let huge = ContentDescriptor::stored(
        ContentKind::Metadata,
        digest_of(b"x"),
        zup_acquire::MAX_METADATA_BYTES + 1,
    );
    assert!(huge.validate().is_err());
    assert!(cache.writer(&empty).is_err());
    assert!(cache.writer(&huge).is_err());
}

#[test]
fn a_compression_bomb_is_bounded_by_the_expansion_limit() {
    // 4 GiB of zeroes compresses to a few KiB. The descriptor's expansion
    // limit is what stops that from being a disk-filling attack, and it is
    // checked before a byte is written.
    let logical = vec![0u8; 8 * 1024 * 1024];
    let wire = wire_of(&logical, 19);
    let descriptor = payload_descriptor(&logical, 19);
    assert!(
        wire.len() < 4096,
        "the fixture must actually compress for this to mean anything"
    );
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    // The declared logical size is beyond what a payload may expand to from
    // this wire length, so the descriptor is refused outright.
    let mut lying = descriptor;
    lying.size = descriptor.compressed_size.saturating_mul(1_000_000);
    assert!(lying.validate().is_err());
    assert!(cache.writer(&lying).is_err());
}

#[test]
fn a_blob_survives_a_process_restart_because_it_is_named_by_its_content() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let logical = payload(23, 128 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    {
        let cache = ContentCache::open(dir.path(), CachePolicy::Auto).expect("the cache opens");
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer
            .write(&wire_of(&logical, LEVEL))
            .expect("the bytes land");
        writer.commit().expect("the blob verifies");
    }
    let cache = ContentCache::open(dir.path(), CachePolicy::Auto).expect("the cache reopens");
    let blob = cache
        .get(&descriptor, Verify::Full)
        .expect("the probe runs")
        .expect("the blob is still there");
    assert_eq!(blob.read_to_end().expect("the blob decodes"), logical);
    assert!(
        cache
            .enforce_policy(&[])
            .expect("auto keeps recent entries")
            == 0
    );
}

#[test]
fn a_reservation_from_a_dead_writer_is_reclaimed() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let mut cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    // A machine that wants to recover quickly from an installer that was killed
    // mid-write asks for a short claim lifetime rather than waiting one out.
    cache.set_reservation_stale(std::time::Duration::ZERO);
    let logical = payload(24, 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    fs::create_dir_all(paths.parent()).expect("the directory exists");
    // A claim left behind by a process that is gone.
    fs::write(&paths.lock_path, b"999999\n").expect("the stale claim is written");
    let mut writer = cache
        .writer(&descriptor)
        .expect("a stale claim is reclaimed");
    writer
        .write(&wire_of(&logical, LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");
}

#[test]
fn a_blob_is_reachable_only_through_its_own_digest() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let logical = payload(25, 4096);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer
        .write(&wire_of(&logical, LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    let hex = descriptor.digest.to_hex();
    assert_eq!(
        paths.final_path,
        dir.path()
            .join("blobs")
            .join("sha256")
            .join(&hex[..2])
            .join(&hex[2..]),
        "a blob's path is a function of its digest"
    );
    assert!(
        paths.final_path.starts_with(cache.root()),
        "a blob never leaves the cache root"
    );
}

#[test]
fn a_wrong_digest_never_resolves_to_a_present_blob() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let logical = payload(26, 4096);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer
        .write(&wire_of(&logical, LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");

    let other = ContentDescriptor::compressed(
        ContentKind::Payload,
        Sha256Digest::from_bytes([7u8; 32]),
        descriptor.compressed_size,
        descriptor.size,
    );
    assert!(
        cache
            .get(&other, Verify::Full)
            .expect("the probe runs")
            .is_none()
    );
}
