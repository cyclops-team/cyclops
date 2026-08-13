//! The one hash that answers "did the operator edit this file".

/// FNV-1a 64, hex. Not cryptographic and does not need to be: the question
/// is "did the operator edit this file", not "is this an attack". The
/// constants and the 16-hex-digit format are load-bearing: hookset writes
/// them into receipts and manifests/themeseed compare seed tables against
/// them, so a copy that drifted would misread every artifact as edited.
pub(crate) fn fnv64(data: &[u8]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in data {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}
