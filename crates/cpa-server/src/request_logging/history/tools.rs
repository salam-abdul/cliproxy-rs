//! The tool index: tool definitions, calls and results read from stored Claude bodies.
//! Only tool, MCP server and skill names and sizes leave the bodies; prompts, outputs,
//! tool inputs and results are never copied.

use sha2::{Digest, Sha256};

pub(crate) mod scan;
pub(crate) mod store;

/// The first 8 bytes of the SHA-256 of `bytes`, read as a big-endian `i64`: stable
/// across processes and Rust versions, so stored keys match on the next start.
pub(crate) fn hash(bytes: &[u8]) -> i64 {
    hash_parts(&[bytes])
}

/// [`hash`] of the concatenation of `parts`.
pub(crate) fn hash_parts(parts: &[&[u8]]) -> i64 {
    let mut sha = Sha256::new();
    for part in parts {
        sha.update(part);
    }
    let digest = sha.finalize();
    digest.first_chunk::<8>().map_or(0, |head| i64::from_be_bytes(*head))
}
