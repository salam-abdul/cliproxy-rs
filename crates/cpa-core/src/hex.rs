//! Lowercase hex text (Go `hex.EncodeToString`).

/// Go `hex.EncodeToString`.
pub fn encode(bytes: impl AsRef<[u8]>) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 15)]));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn encodes_lowercase_pairs() {
        assert_eq!(super::encode([0x00, 0x0f, 0xa5, 0xff]), "000fa5ff");
        assert_eq!(super::encode([]), "");
    }
}
