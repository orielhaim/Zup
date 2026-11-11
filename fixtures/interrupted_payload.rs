use std::io::Read;
use zup_bundle::{PayloadError, PayloadReader, PayloadSource};
use zup_core::{RelativePath, Sha256Digest};

pub struct InterruptedPayload(pub zup_bundle::DirectoryPayloadSource);

impl PayloadSource for InterruptedPayload {
    fn open(&self, path: &RelativePath, digest: &Sha256Digest, size: u64) -> Result<PayloadReader, PayloadError> {
        Ok(Box::new(InterruptedReader { inner: self.0.open(path, digest, size)?, read: false }))
    }
}

struct InterruptedReader {
    inner: PayloadReader,
    read: bool,
}

impl Read for InterruptedReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.read {
            zup_transaction::fault_injection::boundary(
                zup_transaction::fault_injection::Boundary::MaterializationStreamInterrupted,
                "generation:materialize:",
            );
        }
        let length = buffer.len().min(4);
        let count = self.inner.read(&mut buffer[..length])?;
        self.read |= count > 0;
        Ok(count)
    }
}
