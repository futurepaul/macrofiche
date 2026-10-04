//! Object ids: SHA-1, 40 lowercase hex characters (contract section 4).

use std::fmt;

/// A git object id.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Oid([u8; 20]);

/// The all-zero id: "no object" in a ref update and in a webhook's
/// `before`.
pub const ZERO: Oid = Oid([0; 20]);
pub const HEX_LEN: usize = 40;

impl Oid {
    /// 40 lowercase hex characters, or `None`.
    pub fn parse(s: &str) -> Option<Oid> {
        let b = s.as_bytes();
        if b.len() != HEX_LEN {
            return None;
        }
        let mut out = [0u8; 20];
        for (i, pair) in b.as_chunks::<2>().0.iter().enumerate() {
            let hi = nibble(pair[0])?;
            let lo = nibble(pair[1])?;
            out[i] = hi << 4 | lo;
        }
        Some(Oid(out))
    }

    pub fn from_bytes(bytes: [u8; 20]) -> Oid {
        Oid(bytes)
    }

    pub fn is_zero(&self) -> bool {
        *self == ZERO
    }

    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }

    pub fn hex(&self) -> String {
        self.to_string()
    }
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Oid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Oid({self})")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Goal: ids round-trip, and anything not 40 lowercase hex is refused.
    // Method: valid ids, then each kind of invalid one.
    #[test]
    fn ids_parse_and_print() {
        let s = "3033acb8fd0d39124facfc02ff13d974de0e6568";
        assert_eq!(Oid::parse(s).unwrap().to_string(), s);
        assert!(Oid::parse(&"0".repeat(40)).unwrap().is_zero());
        for bad in ["", "3033", &s.to_uppercase(), &format!("{s}0"), &s.replace('3', "g"), &format!(" {}", &s[1..])] {
            assert_eq!(Oid::parse(bad), None, "{bad:?}");
        }
    }
}
