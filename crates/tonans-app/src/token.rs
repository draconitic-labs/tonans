//! The launch token.
//!
//! Every control route requires `Authorization: Bearer <token>`. The token is 256
//! bits from the operating system's CSPRNG, minted at launch and never written to
//! disk. It dies with the process.
//!
//! # Why a loopback socket needs a token at all
//!
//! Binding to `127.0.0.1` keeps the network out. It does not keep *the browser*
//! out: any page you happen to have open — an advertisement in a background tab —
//! can issue requests to `http://127.0.0.1:<port>` and walk the port range looking
//! for something that answers. Without a secret it cannot guess, such a page could
//! read your projects and write new ones.
//!
//! The token is what it cannot guess. Two more things back it up, in `server.rs`:
//! the `Host` header must be the address we actually bound (which defeats DNS
//! rebinding, where an attacker's domain resolves to 127.0.0.1 and the browser
//! considers it same-origin), and the token is compared in constant time.

use tonans_core::error::{Error, Result};

/// A fresh 256-bit token, hex-encoded.
pub fn mint() -> Result<String> {
    Ok(hex(&random_bytes::<32>()?))
}

/// A 128-bit opaque identifier: upload handles and project ids.
///
/// Not a secret, but unguessable, so that one browser tab cannot stumble onto
/// another's staged upload.
pub fn opaque_id() -> String {
    // A failure of the OS CSPRNG at this point is not recoverable and not
    // meaningfully reportable to a user mid-request; the process is already in a
    // state where nothing it does can be trusted.
    let bytes = random_bytes::<16>().expect("the operating system CSPRNG is unavailable");
    hex(&bytes)
}

fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|e| {
        Error::io(
            "reading from the operating system's random number generator",
            std::io::Error::other(e.to_string()),
        )
    })?;
    Ok(bytes)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Compare two tokens without leaking their contents through timing.
///
/// A naive `==` returns as soon as it finds a differing byte, so the time it takes
/// reveals how many leading bytes were right. Over enough requests that is enough
/// to reconstruct the token a byte at a time. This compares every byte, always.
#[must_use]
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    // The length is not a secret — the token is a fixed 64 hex characters — so an
    // early return here leaks nothing an attacker does not already know.
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (x, y) in a.iter().zip(b) {
        difference |= x ^ y;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_256_bits_of_hex() {
        let token = mint().unwrap();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn two_tokens_are_never_the_same() {
        let a = mint().unwrap();
        let b = mint().unwrap();
        assert_ne!(a, b, "the CSPRNG returned the same token twice");
        assert_ne!(opaque_id(), opaque_id());
    }

    #[test]
    fn constant_time_comparison_is_still_a_correct_comparison() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
        // The failure that matters: a token that is right in every byte but one.
        let token = mint().unwrap();
        let mut wrong = token.clone();
        wrong.replace_range(63..64, if token.ends_with('a') { "b" } else { "a" });
        assert!(!constant_time_eq(&token, &wrong));
    }
}
