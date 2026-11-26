//! The tool index: tool definitions, calls and results read from stored Claude bodies.
//! Only tool, MCP server and skill names and sizes leave the bodies; prompts, outputs,
//! tool inputs and results are never copied.

use sha2::{Digest, Sha256};

pub(crate) mod indexer;
pub(crate) mod read;
pub(crate) mod scan;
pub(crate) mod store;

/// The first 8 bytes of the SHA-256 of `bytes`, read as a big-endian `i64`: stable
/// across processes and Rust versions, so stored keys match on the next start.
pub(crate) fn hash(bytes: &[u8]) -> i64 {
    let mut hasher = Hasher::default();
    hasher.update(bytes);
    hasher.finish()
}

/// [`hash`] fed in pieces, for bytes that are not contiguous.
#[derive(Default)]
pub(crate) struct Hasher(Sha256);

impl Hasher {
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    pub(crate) fn finish(self) -> i64 {
        let digest: [u8; 32] = self.0.finalize().into();
        let [a, b, c, d, e, f, g, h, ..] = digest;
        i64::from_be_bytes([a, b, c, d, e, f, g, h])
    }
}

/// A length or count as stored in the index.
pub(crate) fn len_i64(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}
