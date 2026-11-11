//! Content digests: `sha256:<hex>`, self-describing like container digests.
//!
//! Every hash records which algorithm made it, so adding an algorithm later
//! (BLAKE3 for very large files, say) is one new enum variant, and every
//! hash already stored stays readable.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HashAlgorithm {
    Sha256,
}

impl HashAlgorithm {
    /// The prefix used in digests, e.g. `sha256`.
    pub fn name(self) -> &'static str {
        match self {
            HashAlgorithm::Sha256 => "sha256",
        }
    }

    /// Length of the hex-encoded hash.
    pub fn hex_len(self) -> usize {
        match self {
            HashAlgorithm::Sha256 => 64,
        }
    }
}

impl FromStr for HashAlgorithm {
    type Err = String;

    fn from_str(text: &str) -> Result<HashAlgorithm, String> {
        match text {
            "sha256" => Ok(HashAlgorithm::Sha256),
            other => Err(format!("unknown hash algorithm `{other}` (known: sha256)")),
        }
    }
}

/// A hash plus the algorithm that made it. Construct with `Digest::new` or
/// by parsing, both of which check the hex, so a `Digest` is always valid.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Digest {
    algorithm: HashAlgorithm,
    hex: String,
}

impl Digest {
    pub fn new(algorithm: HashAlgorithm, hex: &str) -> Result<Digest, String> {
        let valid = hex.len() == algorithm.hex_len()
            && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !valid {
            return Err(format!(
                "a {} digest is {} lowercase hex characters, got `{hex}`",
                algorithm.name(),
                algorithm.hex_len()
            ));
        }
        Ok(Digest {
            algorithm,
            hex: hex.to_string(),
        })
    }

    /// From raw hash bytes, as a hashing library returns them.
    pub fn from_bytes(algorithm: HashAlgorithm, bytes: &[u8]) -> Result<Digest, String> {
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Digest::new(algorithm, &hex)
    }

    pub fn algorithm(&self) -> HashAlgorithm {
        self.algorithm
    }

    pub fn hex(&self) -> &str {
        &self.hex
    }
}

/// `Display` is Rust's `toString()`: it powers `format!("{digest}")` and
/// `.to_string()`.
impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.algorithm.name(), self.hex)
    }
}

/// `FromStr` is the reverse: it powers `"sha256:...".parse::<Digest>()`.
impl FromStr for Digest {
    type Err = String;

    fn from_str(text: &str) -> Result<Digest, String> {
        let (algorithm, hex) = text
            .split_once(':')
            .ok_or_else(|| format!("expected <algorithm>:<hex>, got `{text}`"))?;
        Digest::new(algorithm.parse()?, hex)
    }
}

// In JSON a digest is one string, "sha256:...". Rather than deriving,
// serde is told to go through Display and FromStr.
impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Digest, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";

    #[test]
    fn displays_and_parses_as_algorithm_colon_hex() {
        let digest = Digest::new(HashAlgorithm::Sha256, ABC).unwrap();
        let text = digest.to_string();
        assert_eq!(text, format!("sha256:{ABC}"));
        assert_eq!(text.parse::<Digest>().unwrap(), digest);
    }

    #[test]
    fn builds_from_raw_bytes() {
        let digest = Digest::from_bytes(HashAlgorithm::Sha256, &[0xab; 32]).unwrap();
        assert_eq!(digest.hex(), "ab".repeat(32));
    }

    #[test]
    fn is_a_plain_string_in_json() {
        let digest = Digest::new(HashAlgorithm::Sha256, ABC).unwrap();
        let json = serde_json::to_string(&digest).unwrap();
        assert_eq!(json, format!("\"sha256:{ABC}\""));
        assert_eq!(serde_json::from_str::<Digest>(&json).unwrap(), digest);
    }

    #[test]
    fn rejects_malformed_digests() {
        assert!("ba7816bf".parse::<Digest>().is_err(), "no algorithm");
        assert!(
            "md5:ba7816bf".parse::<Digest>().is_err(),
            "unknown algorithm"
        );
        assert!("sha256:ba7816bf".parse::<Digest>().is_err(), "too short");
        assert!(
            format!("sha256:{}", ABC.to_uppercase())
                .parse::<Digest>()
                .is_err(),
            "uppercase hex"
        );
        assert!(
            serde_json::from_str::<Digest>("\"sha256:nothex\"").is_err(),
            "JSON goes through the same checks"
        );
    }
}
