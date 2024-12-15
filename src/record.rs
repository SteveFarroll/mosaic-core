use crate::{Address, Error, Id, Kind, PrivateKey, PublicKey, RecordFlags, Timestamp};
use base64::prelude::*;
use ed25519_dalek::Signature;
use std::ops::{Range, RangeFrom};

/// A `record` is a digitally signed datum generated by a user,
/// stored in and retrieved from a server, and used by an application,
//
// INVARIANTS:
//   at least 208 bytes long
//   no more than 1048576 bytes long
//   hash is correct
//   signature is correct
//   reserved flags are zero
//   208 + tags_padded_len() + payload_padded_len() == self.0.len()
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
        // (note we don't use fn full_hash() because we need to
        //  reuse the hasher to verify the signature)
        let mut truehash: [u8; 64] = [0; 64];
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.0[HASHABLE_RANGE]);
        hasher.finalize_xof().fill(&mut truehash[..]);

        // Compare the start of the true hash to the claimed hash
        if truehash[..40] != self.0[HASH_RANGE] {
            return Err(Error::HashMismatch);
        }

        // Verify the signature
        let signature = Signature::from_slice(&self.0[SIG_RANGE])?;
        let digest = crate::crypto::Blake3 { h: hasher };
        signing_public_key
            .0
            .verify_prehashed_strict(digest, Some(b"Mosaic"), &signature)?;

        // Verify the timestamp
        let _timestamp = Timestamp::from_bytes(self.0[TIMESTAMP_RANGE].try_into().unwrap())?;

        // Verify reserved flags are 0
        let flags = self.flags();
        if flags & RecordFlags::all() != RecordFlags::empty() {
            return Err(Error::ReservedFlagsUsed);
        }

        if self.0[70] != 0 || self.0[71] != 0 {
            return Err(Error::IdZerosAreNotZero);
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
        signing_private_key: &PrivateKey,
        address: Address,
        flags: RecordFlags,
        app_flags: u16,
        tags_bytes: &[u8],
        payload: &[u8],
    ) -> Result<Record, Error> {
        Self::new_replacement(
            signing_private_key,
            address,
            address.timestamp(),
            flags,
            app_flags,
            tags_bytes,
            payload,
        )
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
        signing_private_key: &PrivateKey,
        address: Address,
        timestamp: Timestamp,
        flags: RecordFlags,
        app_flags: u16,
        tags_bytes: &[u8],
        payload: &[u8],
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

        #[allow(clippy::cast_possible_truncation)]
        let payload_len = payload.len() as u32;
        bytes[LEN_P_RANGE].copy_from_slice(payload_len.to_le_bytes().as_slice());

        #[allow(clippy::cast_possible_truncation)]
        let tags_len = tags_bytes.len() as u16;
        bytes[LEN_T_RANGE].copy_from_slice(tags_len.to_le_bytes().as_slice());

        bytes[APPFLAGS_RANGE].copy_from_slice(app_flags.to_le_bytes().as_slice());
        bytes[TIMESTAMP_RANGE].copy_from_slice(timestamp.to_bytes().as_slice());

        bytes[FLAGS_RANGE].copy_from_slice(flags.bits().to_le_bytes().as_slice());

        bytes[ADDRESS_RANGE].copy_from_slice(address.as_bytes().as_slice());

        let public_key = signing_private_key.public();
        bytes[SIGNING_KEY_RANGE].copy_from_slice(public_key.as_bytes().as_slice());

        let mut truehash: [u8; 64] = [0; 64];
        let mut hasher = blake3::Hasher::new();
        hasher.update(&bytes[HASHABLE_RANGE]);
        hasher.finalize_xof().fill(&mut truehash[..]);
        bytes[HASH_RANGE].copy_from_slice(&truehash[..40]);

        bytes[BE_TIMESTAMP_RANGE].copy_from_slice(timestamp.to_be_bytes().as_slice());

        // Sign
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

    /// The full 64-byte BLAKE3 hash of the contents `[112:]`
    #[must_use]
    pub fn full_hash(&self) -> [u8; 64] {
        let mut truehash: [u8; 64] = [0; 64];
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.0[HASHABLE_RANGE]);
        hasher.finalize_xof().fill(&mut truehash[..]);
        truehash
    }

    /// Id
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn id(&self) -> Id {
        Id::from_bytes_no_verify(self.0[ID_RANGE].try_into().unwrap())
    }

    /// Address
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn address(&self) -> Address {
        Address::from_bytes_no_verify(self.0[ADDRESS_RANGE].try_into().unwrap())
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

    /// Kind
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn kind(&self) -> Kind {
        Kind(u16::from_le_bytes(self.0[KIND_RANGE].try_into().unwrap()))
    }

    /// Original Timestamp
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn original_timestamp(&self) -> Timestamp {
        Timestamp::from_bytes(self.0[ORIG_TIMESTAMP_RANGE].try_into().unwrap()).unwrap()
    }

    /// Nonce
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn nonce(&self) -> &[u8; 8] {
        self.0[NONCE_RANGE].try_into().unwrap()
    }

    /// Flags
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn flags(&self) -> RecordFlags {
        RecordFlags::from_bits_retain(u16::from_le_bytes(self.0[FLAGS_RANGE].try_into().unwrap()))
    }

    /// App Flags
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn app_flags(&self) -> u16 {
        u16::from_le_bytes(self.0[APPFLAGS_RANGE].try_into().unwrap())
    }

    /// Timestamp
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn timestamp(&self) -> Timestamp {
        Timestamp::from_bytes(self.0[TIMESTAMP_RANGE].try_into().unwrap()).unwrap()
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

const ID_RANGE: Range<usize> = 64..112;
const BE_TIMESTAMP_RANGE: Range<usize> = 64..70;
// const ZERO_RANGE: Range<usize> = 70..72;
const HASH_RANGE: Range<usize> = 72..112;

const SIGNING_KEY_RANGE: Range<usize> = 112..144;

const ADDRESS_RANGE: Range<usize> = 144..192;
const ORIG_TIMESTAMP_RANGE: Range<usize> = 144..150;
const KIND_RANGE: Range<usize> = 150..152;
const NONCE_RANGE: Range<usize> = 152..160;
const AUTHOR_KEY_RANGE: Range<usize> = 160..192;

const FLAGS_RANGE: Range<usize> = 192..194;
const TIMESTAMP_RANGE: Range<usize> = 194..200;
const APPFLAGS_RANGE: Range<usize> = 200..202;
const LEN_T_RANGE: Range<usize> = 202..204;
const LEN_P_RANGE: Range<usize> = 204..208;

const HEADER_LEN: usize = 208;
const HASHABLE_RANGE: RangeFrom<usize> = 112..;
const MAX_PAYLOAD_LEN: usize = 1_048_576 - HEADER_LEN;

impl std::fmt::Display for Record {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "id: {}", BASE64_STANDARD.encode(self.id()))?;
        writeln!(f, "  address: {}", BASE64_STANDARD.encode(self.address()))?;
        writeln!(f, "  author key: {}", self.author_public_key())?;
        writeln!(f, "  signing key: {}", self.signing_public_key())?;
        writeln!(f, "  timestamp: {}", self.timestamp())?;
        writeln!(f, "  kind: {}", self.kind())?;
        writeln!(f, "  flags: {} {}", self.flags(), self.app_flags())?;
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
            &signing_private_key,
            Address::new(
                master_public_key,
                Kind::KEY_SCHEDULE,
                Timestamp::now().unwrap(),
            ),
            RecordFlags::empty(),
            0,
            b"",
            b"hello world",
        )
        .unwrap();

        println!("{r1}");

        let r2 = Record::from_bytes(r1.as_bytes()).unwrap();

        println!("r2 built");

        assert_eq!(r1, r2);
    }
}
