//! Checksum kinds referenced by repo metadata, and verification.

use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum ChecksumKind {
    Sha1,
    Sha256,
    Sha512,
}

impl ChecksumKind {
    /// Parse the `type=` attribute value used in repomd/primary XML.
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "sha" | "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct Checksum {
    pub kind: ChecksumKind,
    /// Lowercase hex digest.
    pub hex: String,
}

impl Checksum {
    /// Compute the digest of `data` and compare (case-insensitively) to the
    /// expected hex value.
    pub fn verify(&self, data: &[u8]) -> bool {
        let actual = match self.kind {
            ChecksumKind::Sha1 => hex::encode(Sha1::digest(data)),
            ChecksumKind::Sha256 => hex::encode(Sha256::digest(data)),
            ChecksumKind::Sha512 => hex::encode(Sha512::digest(data)),
        };
        actual.eq_ignore_ascii_case(&self.hex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_roundtrip() {
        // echo -n "abc" | sha256sum
        let c = Checksum {
            kind: ChecksumKind::Sha256,
            hex: "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".into(),
        };
        assert!(c.verify(b"abc"));
        assert!(!c.verify(b"abd"));
    }

    #[test]
    fn kind_parsing() {
        assert_eq!(ChecksumKind::parse("sha"), Some(ChecksumKind::Sha1));
        assert_eq!(ChecksumKind::parse("SHA256"), Some(ChecksumKind::Sha256));
        assert_eq!(ChecksumKind::parse("md5"), None);
    }
}
