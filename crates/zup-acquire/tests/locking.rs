//! Writer exclusion, against real handles and a real second process.
//!
//! The lock is advisory: it coordinates cooperating Zup processes and is not a
//! security boundary. What it must do is exclude one writer of a digest from
//! another, for as long as the first is alive.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use common::*;
use zup_acquire::{CacheError, CachePolicy, ContentCache, ContentDescriptor};

const ROOT: &str = "ZUP_TEST_LOCK_ROOT";
const READY: &str = "ZUP_TEST_LOCK_READY";
const LENGTH: usize = 4096;

fn cache(root: &Path) -> ContentCache {
    ContentCache::open(root, CachePolicy::Keep).expect("the cache opens")
}

fn descriptor(seed: u8) -> ContentDescriptor {
    payload_descriptor(&payload(seed, LENGTH), LEVEL)
}

fn wait_for(marker: &Path, child: &mut Child) {
    for _ in 0..2_000 {
        if marker.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("the lock holder never reported that it holds the lock");
}

fn wait_until_unlocked(
    cache: &ContentCache,
    descriptor: &ContentDescriptor,
) -> zup_acquire::BlobWriter {
    for _ in 0..2_000 {
        if let Ok(writer) = cache.writer(descriptor) {
            return writer;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("the lock stayed held after the holder was gone");
}

#[test]
fn writer_lock_is_exclusive() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = cache(dir.path());
    let logical = payload(40, LENGTH);
    let descriptor = payload_descriptor(&logical, LEVEL);
    let paths = cache.paths(&descriptor).expect("the blob has paths");
    std::fs::create_dir_all(paths.parent()).expect("the shard directory exists");

    // A file left behind by a writer that is gone, in the shape an older build
    // wrote it in. A lock file is not a lock.
    std::fs::write(&paths.lock_path, b"999999\n").expect("the leftover is written");

    let first = cache
        .writer(&descriptor)
        .expect("a leftover lock file does not block the blob");
    let refused = cache
        .writer(&descriptor)
        .err()
        .expect("a second writer is refused");
    assert!(matches!(refused, CacheError::Locked { .. }), "{refused}");
    drop(first);

    assert!(
        paths.lock_path.is_file(),
        "the lock file outlives its writer: nothing is deleted to release a lock"
    );
    let mut writer = cache
        .writer(&descriptor)
        .expect("the blob is writable once the writer is dropped");
    writer
        .write(&wire_of(&logical, LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");
}

/// Two roles: the holder, when the parent has said which blob, and otherwise the
/// parent that kills it.
#[test]
fn lock_is_released_when_process_exits() {
    if let Some(root) = std::env::var_os(ROOT) {
        let cache = cache(Path::new(&root));
        let _held = cache
            .writer(&descriptor(41))
            .expect("the child takes the lock");
        std::fs::write(
            PathBuf::from(std::env::var_os(READY).expect("a ready marker")),
            b"held",
        )
        .expect("the ready marker is written");
        loop {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = cache(dir.path());
    let ready = dir.path().join("ready");
    let mut holder = Command::new(std::env::current_exe().expect("this test executable"))
        .args([
            "--exact",
            "lock_is_released_when_process_exits",
            "--test-threads",
            "1",
        ])
        .env(ROOT, dir.path())
        .env(READY, &ready)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("a second process starts");
    wait_for(&ready, &mut holder);

    let descriptor = descriptor(41);
    let refused = cache
        .writer(&descriptor)
        .err()
        .expect("a writer here is refused while another process holds the lock");
    assert!(matches!(refused, CacheError::Locked { .. }), "{refused}");

    // Killed rather than asked to leave, so the lock has to come back from
    // process death alone.
    holder.kill().expect("the holder is killed");
    holder.wait().expect("the holder is reaped");

    let mut writer = wait_until_unlocked(&cache, &descriptor);
    writer
        .write(&wire_of(&payload(41, LENGTH), LEVEL))
        .expect("the bytes land");
    writer.commit().expect("the blob verifies");
}

#[test]
fn different_blobs_do_not_contend() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let cache = cache(dir.path());
    let first = payload(43, LENGTH);
    let second = payload(44, LENGTH);
    let first_descriptor = payload_descriptor(&first, LEVEL);
    let second_descriptor = payload_descriptor(&second, LEVEL);

    let mut held = cache
        .writer(&first_descriptor)
        .expect("the first blob is writable");
    let mut other = cache
        .writer(&second_descriptor)
        .expect("a different blob is writable while the first is locked");
    other
        .write(&wire_of(&second, LEVEL))
        .expect("the bytes land beside the other blob");
    other.commit().expect("the second blob verifies");

    held.write(&wire_of(&first, LEVEL))
        .expect("the bytes land once the other blob is done");
    held.commit().expect("the first blob verifies");
}
