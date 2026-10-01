//! The cache: what it guarantees about bytes on disk.
//!
//! Everything here is a claim about hostile or damaged local state, because a
//! cache is the one component whose input the machine itself produced and might
//! therefore be wrong.

mod common;

use common::*;
use std::fs;
use zup_acquire::{CachePolicy, CacheProbe, ContentCache, ContentDescriptor, ContentKind, Verify};

#[test]
fn a_blob_is_only_published_after_its_digest_matches() {
    let cache = TestCache::new();
    let cache = cache.open(CachePolicy::Keep);
    let logical = payload(2, 4096);
    let descriptor = payload_descriptor(&logical, LEVEL);
    // The right length on the wire, the wrong content: the only attack a length
    // check alone would miss.
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
    assert!(
        cache
            .get(&descriptor, Verify::Full)
            .expect("the probe runs")
            .is_none(),
        "a refused blob never resolves as present"
    );
}

/// Two ways a transfer falls short of what its descriptor promised: the body
/// stops early, or the descriptor claims bytes that were never sent. Both must
/// leave the cache with nothing readable.
#[test]
fn a_blob_short_of_its_descriptor_is_refused_and_leaves_nothing_behind() {
    let cache = TestCache::new();
    let cache = cache.open(CachePolicy::Keep);
    let logical = payload(3, 64 * 1024);

    for (overstated, wire) in [
        (false, {
            let mut wire = wire_of(&logical, LEVEL);
            wire.truncate(wire.len() / 2);
            wire
        }),
        (true, wire_of(&logical, LEVEL)),
    ] {
        let mut descriptor = payload_descriptor(&logical, LEVEL);
        if overstated {
            descriptor.compressed_size += 7;
        }
        let paths = cache.paths(&descriptor).expect("the blob has paths");
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer.write(&wire).expect("the wire form is accepted");
        let error = writer.commit().expect_err("a short blob cannot publish");
        assert!(
            matches!(error, zup_acquire::CacheError::SizeMismatch { .. }),
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

/// A partial on disk is only resumed when the cache can prove it is the prefix
/// the record describes. Three ways it cannot: the partial belongs to a
/// different blob, something rewrote the bytes inside the recorded prefix, or
/// the file grew past the offset the record vouched for. In each case the bytes
/// are discarded rather than trusted, because the alternative is a blob that
/// verifies against the wrong prefix.
#[test]
fn a_partial_the_cache_cannot_prove_is_discarded_rather_than_resumed() {
    let logical = compressible(9, 2 * 1024 * 1024);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let wire = wire_of(&logical, LEVEL);
    let cut = (wire.len() / 2) as u64;

    let leave_partial = |dir: &std::path::Path| {
        let cache = ContentCache::open(dir, CachePolicy::Keep).expect("the cache opens");
        let mut writer = cache.writer(&descriptor).expect("the writer opens");
        writer
            .write(&wire[..cut as usize])
            .expect("the prefix lands");
        drop(writer);
    };

    // A partial for a different blob must not be adopted.
    let foreign = tempfile::tempdir().expect("a temporary directory");
    {
        let cache = ContentCache::open(foreign.path(), CachePolicy::Keep).expect("the cache opens");
        let other = compressible(8, 1024 * 1024);
        let other = payload_descriptor(&other, LEVEL);
        let mut writer = cache.writer(&other).expect("the writer opens");
        writer
            .write(&wire_of(&compressible(8, 1024 * 1024), LEVEL)[..wire.len() / 2])
            .expect("the prefix lands");
        drop(writer);
    }
    let cache = ContentCache::open(foreign.path(), CachePolicy::Keep).expect("the cache reopens");
    assert!(
        !cache
            .paths(&descriptor)
            .expect("the blob has paths")
            .partial_path
            .exists()
    );
    let writer = cache.writer(&descriptor).expect("the writer opens");
    assert_eq!(writer.wire_offset(), 0, "a foreign partial is not resumed");
    writer.abandon();

    // Something outside the process rewrites the middle of the prefix.
    let tampered = tempfile::tempdir().expect("a temporary directory");
    leave_partial(tampered.path());
    {
        let cache =
            ContentCache::open(tampered.path(), CachePolicy::Keep).expect("the cache opens");
        let paths = cache.paths(&descriptor).expect("the blob has paths");
        use std::io::{Seek, SeekFrom, Write};
        let mut file = fs::OpenOptions::new()
            .write(true)
            .open(&paths.partial_path)
            .expect("the partial opens");
        file.seek(SeekFrom::Start(cut / 2)).expect("the seek lands");
        file.write_all(&[0xff; 32]).expect("the tamper lands");
    }
    let cache = ContentCache::open(tampered.path(), CachePolicy::Keep).expect("the cache reopens");
    let writer = cache.writer(&descriptor).expect("the writer opens");
    assert_eq!(
        writer.wire_offset(),
        0,
        "a partial whose prefix does not measure is discarded, not resumed"
    );
    writer.abandon();

    // The file grew past the offset the record vouched for.
    let extended = tempfile::tempdir().expect("a temporary directory");
    leave_partial(extended.path());
    {
        let cache =
            ContentCache::open(extended.path(), CachePolicy::Keep).expect("the cache opens");
        let paths = cache.paths(&descriptor).expect("the blob has paths");
        fs::write(&paths.partial_path, &wire[..(cut as usize) + 4096])
            .expect("the tail is extended");
    }
    let cache = ContentCache::open(extended.path(), CachePolicy::Keep).expect("the cache reopens");
    let writer = cache.writer(&descriptor).expect("the writer opens");
    assert_eq!(
        writer.wire_offset(),
        cut,
        "bytes past the record's offset are not trusted"
    );
    writer.abandon();
}
/// A same-length file with different content is the only attack a length check
/// alone would miss, so every entry point has to re-read the bytes: the probe
/// refuses the entry, the getter will not resolve it, and a handle that survived
/// the tamper still refuses to hand the bytes over. A blob is only ever returned
/// after it has been proved to be the content its name claims.
#[test]
fn a_published_blob_that_no_longer_hashes_to_its_name_is_refused_everywhere() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let logical = payload(11, 8192);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    fs::create_dir_all(paths.parent()).expect("the directory exists");

    let mut writer = cache.writer(&descriptor).expect("the writer opens");
    writer
        .write(&wire_of(&logical, LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");

    // Something outside the process rewrites the published file.
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
    assert!(
        cache
            .get(&descriptor, Verify::Full)
            .expect("the probe runs")
            .is_none(),
        "an impostor never resolves as present"
    );

    // A handle that was taken before the tamper still refuses the bytes: the
    // name is a claim about content, not a promise about a file on disk.
    fs::create_dir_all(paths.parent()).expect("the directory exists");
    fs::write(&paths.final_path, wire_of(&payload(12, 8192), LEVEL))
        .expect("the impostor is written again");
    let blob = zup_acquire::VerifiedBlob {
        descriptor,
        path: paths.final_path,
        wire_size: descriptor.compressed_size,
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

/// A descriptor whose stated sizes cannot describe content the cache will accept.
/// Every case is refused before a byte is written, so the bound cannot be
/// reached by sending first and checking later.
#[test]
fn a_descriptor_that_overstates_its_content_is_refused() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");

    for kind in [
        ContentKind::Payload,
        ContentKind::Runtime,
        ContentKind::Metadata,
        ContentKind::Catalog,
    ] {
        for size in [0, kind.size_limit() + 1] {
            let stored = ContentDescriptor::stored(kind, digest_of(b"x"), size);
            assert!(stored.validate().is_err(), "{kind:?} at {size} bytes");
            assert!(cache.writer(&stored).is_err(), "{kind:?} at {size} bytes");
        }
    }

    // 8 MiB of zeroes compresses to a few hundred bytes. A payload that declares
    // a million times that expansion is refused on the ratio, not on either
    // ceiling.
    let bomb =
        ContentDescriptor::compressed(ContentKind::Payload, digest_of(b"x"), 512, 512_000_000);
    assert!(bomb.validate().is_err(), "{bomb:?}");
    assert!(cache.writer(&bomb).is_err(), "{bomb:?}");
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

/// The published path is derived from the digest alone, so a blob's location on
/// disk is a function of what it contains and nothing else.
#[test]
fn a_blob_is_reachable_only_through_its_own_digest() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = ContentCache::open(dir.path(), CachePolicy::Keep).expect("the cache opens");
    let logical = payload(25, 4096);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    let hex = descriptor.digest.to_hex();
    assert_eq!(
        paths.final_path,
        dir.path()
            .join("blobs")
            .join("sha256")
            .join(&hex[..2])
            .join(&hex[2..]),
    );
}
