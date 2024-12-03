use crate::{Error, Kind, PrivateKey, PublicKey, RecordFlags, Timestamp};
use base64::prelude::*;
use ed25519_dalek::Signature;
use rand_core::{OsRng, RngCore};
use std::ops::{Range, RangeFrom};

/*
FIXME - add 6 bits (3 and 3) that specify the precise length of tags and payload
FIXME - honor zstd flag
FIXME - flags type should be better
FIXME - wrap address type
FIXME - maybe full_hash() vs id() which is just the first half
*/

/// A `record` is a digitally signed datum generated by a user,
/// stored in and retrieved from a server, and used by an application,
//
// INVARIANTS:
//   at least 192 bytes long
//   no more than 1048576 bytes long
//   hash is correct
//   signature is correct
//   reserved flags are zero
//   reserved areas are zero
//   192 + tags_padded_len() + payload_padded_len() == self.0.len()
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Record(Vec<u8>);

impl Record {
    /// Interpret a sequence of bytes as a `Record`. Checks validity.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any verification fails. See `verify()`
    pub fn from_bytes(bytes: &[u8]) -> Result<Record, Error> {
        let unverified = Record(bytes.to_vec());
        unverified.verify()?;
        Ok(unverified)
    }

    /// Interpret a vector of bytes as a `Record`. Checks validity.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any verification fails. See `verify()`
    pub fn from_vec(vec: Vec<u8>) -> Result<Record, Error> {
        let unverified = Record(vec);
        unverified.verify()?;
        Ok(unverified)
    }

    /// Verify invariants. You should not normally need to call this; all code paths
    /// that instantiate a `Record` object call this.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if the length is too short (<216) too long (>1048576),
    /// if the sum of the sections (header, tags, and payload) doesn't equal the
    /// length, if either public key is invalid, if the hash is wrong, if the
    /// signature is wrong, if the timestamp is out of range, or if any reserved
    /// area is not zeroed.
    #[allow(clippy::missing_panics_doc)]
    pub fn verify(&self) -> Result<(), Error> {
        // Verify all lengths
        if self.0.len() > 1_048_576 {
            return Err(Error::RecordTooLong);
        }
        if self.0.len() < HEADER_LEN {
            return Err(Error::RecordTooShort);
        }
        if HEADER_LEN + self.tags_padded_len() + self.payload_padded_len() != self.0.len() {
            return Err(Error::RecordSectionLengthMismatch);
        }

        // Verify PublicKey validity
        let signing_public_key =
            PublicKey::from_bytes(self.0[SIGNING_KEY_RANGE].try_into().unwrap())?;
        let _author_public_key =
            PublicKey::from_bytes(self.0[AUTHOR_KEY_RANGE].try_into().unwrap())?;

        // Compute the true hash
        let mut truehash: [u8; 32] = [0; 32];
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.0[HASHABLE_RANGE]);
        hasher.finalize_xof().fill(&mut truehash[..]);

        // Compare the true hash to the claimed hash
        if truehash != self.0[HASH_RANGE] {
            return Err(Error::HashMismatch);
        }

        // Verify the signature
        let signature = Signature::from_slice(&self.0[SIG_RANGE])?;
        let digest = crate::crypto::Blake3 { h: hasher };
        signing_public_key
            .0
            .verify_prehashed_strict(digest, Some(b"Mosaic"), &signature)?;

        // Verify the timestamp
        let _timestamp = Timestamp::from_slice(self.0[TIMESTAMP_RANGE].try_into().unwrap())?;

        // Verify reserved flags are 0
        let flags = self.flags();
        if flags & RecordFlags::all() != RecordFlags::empty() {
            return Err(Error::ReservedFlagsUsed);
        }

        // Verify reserved space is 0
        if self.0[RESERVED1_RANGE] != [0, 0] {
            return Err(Error::ReservedSpaceUsed);
        }
        if self.0[RESERVED2_RANGE] != [0, 0, 0, 0, 0, 0] {
            return Err(Error::ReservedSpaceUsed);
        }

        Ok(())
    }

    /// Create a new `Record` from component parts.
    ///
    /// Creates a new unique address.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any data is too long, if reserved flags are set,
    /// or if signing fails.
    #[allow(clippy::missing_panics_doc)]
    pub fn new(
        master_public_key: PublicKey,
        signing_private_key: &PrivateKey,
        kind: Kind,
        timestamp: Timestamp,
        flags: RecordFlags,
        tags_bytes: &[u8],
        payload: &[u8],
    ) -> Result<Record, Error> {
        let mut address: [u8; 48] = [0; 48];

        address[ADDR_AUTHOR_KEY_RANGE].copy_from_slice(master_public_key.as_bytes().as_slice());

        address[ADDR_TIMESTAMP_RANGE].copy_from_slice(timestamp.to_slice().as_slice());

        let mut nonce: [u8; 6] = [0; 6];
        OsRng.fill_bytes(&mut nonce);
        address[ADDR_NONCE_RANGE].copy_from_slice(nonce.as_slice());

        address[ADDR_KIND_RANGE].copy_from_slice(kind.0.to_le_bytes().as_slice());

        Self::new_replacement(&address, signing_private_key, tags_bytes, payload, flags)
    }

    /// Create a new `Record` from component parts, replacing an existing record
    /// at the same address
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any data is too long, if reserved flags are set,
    /// or if signing fails.
    #[allow(clippy::missing_panics_doc)]
    pub fn new_replacement(
        address: &[u8; 48],
        signing_private_key: &PrivateKey,
        tags_bytes: &[u8],
        payload: &[u8],
        flags: RecordFlags,
    ) -> Result<Record, Error> {
        if payload.len() > MAX_PAYLOAD_LEN {
            return Err(Error::RecordTooLong);
        }
        if tags_bytes.len() > 65_536 {
            return Err(Error::RecordTooLong);
        }

        let payload_padded_len = (payload.len() + 7) & !7;
        let tags_padded_len = (tags_bytes.len() + 7) & !7;

        let len = HEADER_LEN + payload_padded_len + tags_padded_len;
        if len > 1_048_576 {
            return Err(Error::RecordTooLong);
        }

        if flags & RecordFlags::all() != RecordFlags::empty() {
            return Err(Error::ReservedFlagsUsed);
        }

        let mut bytes = vec![0; len];

        let tag_end = HEADER_LEN + tags_padded_len;

        bytes[tag_end..tag_end + payload.len()].copy_from_slice(payload);

        bytes[HEADER_LEN..HEADER_LEN + tags_bytes.len()].copy_from_slice(tags_bytes);

        bytes[FLAGS_RANGE].copy_from_slice(flags.bits().to_le_bytes().as_slice());

        #[allow(clippy::cast_possible_truncation)]
        let payload_len = payload.len() as u32;
        bytes[LEN_P_RANGE].copy_from_slice(payload_len.to_le_bytes().as_slice());

        #[allow(clippy::cast_possible_truncation)]
        let tags_len = tags_bytes.len() as u16;
        bytes[LEN_T_RANGE].copy_from_slice(tags_len.to_le_bytes().as_slice());

        bytes[ADDRESS_RANGE].copy_from_slice(address);

        let public_key = signing_private_key.public();
        bytes[SIGNING_KEY_RANGE].copy_from_slice(public_key.as_bytes().as_slice());

        let mut hasher = blake3::Hasher::new();
        hasher.update(&bytes[HASHABLE_RANGE]);
        hasher.finalize_xof().fill(&mut bytes[HASH_RANGE]);
        let digest = crate::crypto::Blake3 { h: hasher };
        let sig = signing_private_key
            .0
            .sign_prehashed(digest, Some(b"Mosaic"))?;
        bytes[SIG_RANGE].copy_from_slice(sig.to_bytes().as_slice());

        let record = Record(bytes);

        if cfg!(debug_assertions) {
            record.verify()?;
        }

        Ok(record)
    }

    /// View a `Record` as a slice of bytes
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Signature
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn signature(&self) -> Signature {
        Signature::from_slice(&self.0[SIG_RANGE]).unwrap()
    }

    /// Hash
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn hash(&self) -> &[u8; 32] {
        self.0[HASH_RANGE].try_into().unwrap()
    }

    /// Signing `PublicKey`
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn signing_public_key(&self) -> PublicKey {
        PublicKey::from_bytes(self.0[SIGNING_KEY_RANGE].try_into().unwrap()).unwrap()
    }

    /// Author `PublicKey`
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn author_public_key(&self) -> PublicKey {
        PublicKey::from_bytes(self.0[AUTHOR_KEY_RANGE].try_into().unwrap()).unwrap()
    }

    /// Timestamp
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn timestamp(&self) -> Timestamp {
        Timestamp::from_slice(self.0[TIMESTAMP_RANGE].try_into().unwrap()).unwrap()
    }

    /// Kind
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn kind(&self) -> Kind {
        Kind(u32::from_le_bytes(self.0[KIND_RANGE].try_into().unwrap()))
    }

    /// Address
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn address(&self) -> &[u8; 48] {
        self.0[ADDRESS_RANGE].try_into().unwrap()
    }

    /// Flags
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn flags(&self) -> RecordFlags {
        RecordFlags::from_bits_retain(u16::from_le_bytes(self.0[FLAGS_RANGE].try_into().unwrap()))
    }

    /// Tags length
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn tags_len(&self) -> usize {
        u16::from_le_bytes(self.0[LEN_T_RANGE].try_into().unwrap()) as usize
    }

    /// Tags padded length
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn tags_padded_len(&self) -> usize {
        (self.tags_len() + 7) & !7
    }

    /// Tag bytes
    #[must_use]
    pub fn tags_bytes(&self) -> &[u8] {
        &self.0[HEADER_LEN..HEADER_LEN + self.tags_len()]
    }

    /// Payload length
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn payload_len(&self) -> usize {
        u32::from_le_bytes(self.0[LEN_P_RANGE].try_into().unwrap()) as usize
    }

    /// Tags padded length
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn payload_padded_len(&self) -> usize {
        (self.payload_len() + 7) & !7
    }

    /// Payload bytes
    ///
    /// These are the raw bytes. If Zstd is used, the caller is responsible for
    /// decompressing them.
    #[must_use]
    pub fn payload_bytes_raw(&self) -> &[u8] {
        let start = HEADER_LEN + self.tags_padded_len();
        &self.0[start..start + self.payload_len()]
    }
}

const SIG_RANGE: Range<usize> = 0..64;
const HASH_RANGE: Range<usize> = 64..96;
const SIGNING_KEY_RANGE: Range<usize> = 96..128;
const ADDRESS_RANGE: Range<usize> = 128..176;
const AUTHOR_KEY_RANGE: Range<usize> = 128..160;
const TIMESTAMP_RANGE: Range<usize> = 160..166;
//const NONCE_RANGE: Range<usize> = 166..172;
const KIND_RANGE: Range<usize> = 172..176;
const RESERVED1_RANGE: Range<usize> = 176..178;
const LEN_T_RANGE: Range<usize> = 178..180;
const LEN_P_RANGE: Range<usize> = 180..184;
const RESERVED2_RANGE: Range<usize> = 184..190;
const FLAGS_RANGE: Range<usize> = 190..192;
const HEADER_LEN: usize = 192;
const HASHABLE_RANGE: RangeFrom<usize> = 96..;
const MAX_PAYLOAD_LEN: usize = 1_048_576 - HEADER_LEN;

#[allow(clippy::eq_op)]
const ADDR_AUTHOR_KEY_RANGE: Range<usize> = 128 - 128..160 - 128;
const ADDR_TIMESTAMP_RANGE: Range<usize> = 160 - 128..166 - 128;
const ADDR_NONCE_RANGE: Range<usize> = 166 - 128..172 - 128;
const ADDR_KIND_RANGE: Range<usize> = 172 - 128..176 - 128;

impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "hash: {}", BASE64_STANDARD.encode(self.hash()))?;
        writeln!(f, "  address: {}", BASE64_STANDARD.encode(self.address()))?;
        writeln!(f, "  author key: {}", self.author_public_key())?;
        writeln!(f, "  signing key: {}", self.signing_public_key())?;
        writeln!(f, "  timestamp: {}", self.timestamp())?;
        writeln!(f, "  kind: {}", self.kind())?;
        writeln!(f, "  flags: {}", self.flags())?;
        writeln!(f, "  tags: {}", BASE64_STANDARD.encode(self.tags_bytes()))?;
        writeln!(
            f,
            "  payload: {}",
            BASE64_STANDARD.encode(self.payload_bytes_raw())
        )?;

        Ok(())
    }
}
#[cfg(test)]
mod test {
    use crate::*;

    #[test]
    fn test_padded_lengths_idea() {
        // This just tests the idea, not the actual code since it is so embedded.
        for (len, padded) in [(0, 0), (1, 8), (2, 8), (7, 8), (8, 8), (9, 16)] {
            let padded_len = (len + 7) & !7;
            assert_eq!(padded, padded_len);
        }
    }

    #[test]
    fn test_record() {
        use rand::rngs::OsRng;
        let mut csprng = OsRng;

        let master_private_key = PrivateKey::generate(&mut csprng);
        let master_public_key = master_private_key.public();

        let signing_private_key = PrivateKey::generate(&mut csprng);

        let r1 = Record::new(
            master_public_key,
            &signing_private_key,
            Kind::KEY_SCHEDULE,
            Timestamp::now().unwrap(),
            RecordFlags::empty(),
            b"",
            b"hello world",
        )
        .unwrap();

        println!("{}", r1);

        let r2 = Record::from_bytes(r1.as_bytes()).unwrap();

        println!("r2 built");

        assert_eq!(r1, r2);
    }
}
