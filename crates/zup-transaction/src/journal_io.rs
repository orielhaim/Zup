use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Store-level logical transfers, excluding F11 initialization, lock seals and filesystem metadata.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct JournalIo {
    pub reads: u64,
    pub writes: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
    pub largest_write: u64,
    pub peak_file_bytes: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    reads: AtomicU64,
    writes: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    largest_write: AtomicU64,
    peak_file_bytes: AtomicU64,
}

impl Counters {
    pub fn read(&self, size: usize) {
        self.reads.fetch_add(1, Relaxed);
        self.bytes_read.fetch_add(size as u64, Relaxed);
    }

    pub fn write(&self, size: usize) {
        self.writes.fetch_add(1, Relaxed);
        self.bytes_written.fetch_add(size as u64, Relaxed);
        self.largest_write.fetch_max(size as u64, Relaxed);
        self.file_size(size as u64);
    }

    pub fn file_size(&self, size: u64) {
        self.peak_file_bytes.fetch_max(size, Relaxed);
    }

    pub fn snapshot(&self) -> JournalIo {
        JournalIo {
            reads: self.reads.load(Relaxed),
            writes: self.writes.load(Relaxed),
            bytes_read: self.bytes_read.load(Relaxed),
            bytes_written: self.bytes_written.load(Relaxed),
            largest_write: self.largest_write.load(Relaxed),
            peak_file_bytes: self.peak_file_bytes.load(Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct JournalRevision {
    pub revision: u64,
    pub updated_at: jiff::Timestamp,
}
