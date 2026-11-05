//! Random bytes from the operating system.

/// Fills b with random bytes; a system that cannot give them cannot run runner-up safely.
pub fn fill(b: &mut [u8]) {
    getrandom::getrandom(b).expect("the operating system gave no random bytes");
}

/// n random bytes in lowercase hex.
pub fn hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    fill(&mut b);
    crate::release::hex(&b)
}
