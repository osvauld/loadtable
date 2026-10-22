//! Lowercase hex, the text form of keys and node ids.

/// Lowercase hex of `bytes`.
pub fn encode(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(D.get(usize::from(b >> 4)).copied().unwrap_or(b'0')));
        s.push(char::from(D.get(usize::from(b & 15)).copied().unwrap_or(b'0')));
    }
    s
}

/// Decodes lowercase hex of an exact length `N`.
pub fn decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    let b = text.as_bytes();
    if b.len() != N * 2 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        let hi = nib(*b.get(2 * i)?)?;
        let lo = nib(*b.get(2 * i + 1)?)?;
        *o = hi << 4 | lo;
    }
    Some(out)
}
