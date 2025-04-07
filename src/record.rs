use crate::{Address, Error, Id, InnerError, Kind, PublicKey, RecordFlags, SecretKey, Timestamp};
use base64::prelude::*;
use ed25519_dalek::Signature;
use std::ops::{Deref, DerefMut, Range, RangeFrom};

macro_rules! padded_len {
    ($len:expr) => {
        ((($len) + 7) & !7)
    };
}

/// A `Record` is a digitally signed datum generated by a user,
/// stored in and retrieed from a server, and used by an application,
/// and unsized (borrowed).
///
/// See also `OwnedRecord` for the owned variant.
// INVARIANTS:
//   at least 208 bytes long
//   no more than 1_048_576 bytes long
//   hash is correct
//   signature is correct
//   reserved flags are zero
//   208 + tags_padded_len() + payload_padded_len() == self.0.len()
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Record([u8]);

impl Record {
    // View a slice of bytes as a Record
    fn from_inner<S: AsRef<[u8]> + ?Sized>(s: &S) -> &Record {
        unsafe { &*(std::ptr::from_ref::<[u8]>(s.as_ref()) as *const Record) }
    }

    // View a mutable slice of bytes as a Record
    fn from_inner_mut(inner: &mut [u8]) -> &mut Record {
        // SAFETY: Record is just a wrapper around [u8],
        // therefore converting &mut [u8] to &mut Record is safe.
        unsafe { &mut *(std::ptr::from_mut::<[u8]>(inner) as *mut Record) }
    }

    /// Interpret a sequence of bytes as a `Record`. Checks validity of the length
    /// only.
    ///
    /// # Errors
    ///
    /// Errors if the input is not long enough or if the length is more than `1_048_576`
    /// bytes.
    ///
    /// # Safety
    ///
    /// Be sure the input is a valid Record. Consider calling `verify()` afterwards to
    /// be sure.
    #[allow(clippy::missing_panics_doc)]
    pub unsafe fn from_bytes(input: &[u8]) -> Result<&Record, Error> {
        if input.len() < HEADER_LEN {
            return Err(InnerError::EndOfInput.into());
        }
        let unpadded_tag_len = u16::from_le_bytes(input[LEN_T_RANGE].try_into().unwrap()) as usize;
        let padded_tag_len = padded_len!(unpadded_tag_len);
        let unpadded_payload_len =
            u32::from_le_bytes(input[LEN_P_RANGE].try_into().unwrap()) as usize;
        let padded_payload_len = padded_len!(unpadded_payload_len);

        let len = HEADER_LEN + padded_tag_len + padded_payload_len;
        if len > 1_048_576 {
            return Err(InnerError::RecordTooLong.into());
        }

        let unverified = Self::from_inner(&input[..len]);
        Ok(unverified)
    }

    /// Write a new `Record` to the buffer
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any data is too long, if the buffer is too small,
    /// if reserved flags are set, or if signing fails.
    pub fn write_record<'a>(
        buffer: &'a mut [u8],
        signing_secret_key: &SecretKey,
        parts: &RecordParts,
    ) -> Result<&'a Record, Error> {
        let address = match parts.deterministic_key {
            Some(key) => Address::new_deterministic(signing_secret_key.public(), parts.kind, key),
            None => Address::new_random(signing_secret_key.public(), parts.kind),
        };

        Self::write_replacement_record(
            buffer,
            signing_secret_key,
            address,
            parts.timestamp,
            parts.flags,
            parts.app_flags,
            parts.tags_bytes,
            parts.payload,
        )
    }

    /// Write a new `Record` to the buffer from component parts with the given address.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any data is too long, if the buffer is too small,
    /// if reserved flags are set, or if signing fails.
    #[allow(clippy::too_many_arguments)]
    pub fn write_replacement_record<'a>(
        buffer: &'a mut [u8],
        signing_secret_key: &SecretKey,
        address: Address,
        timestamp: Timestamp,
        flags: RecordFlags,
        app_flags: u16,
        tags_bytes: &[u8],
        payload: &[u8],
    ) -> Result<&'a Record, Error> {
        if tags_bytes.len() > 65_536 {
            return Err(InnerError::RecordTooLong.into());
        }
        let padded_tags_len = padded_len!(tags_bytes.len());
        let padded_payload_len = padded_len!(payload.len());
        let len = HEADER_LEN + padded_tags_len + padded_payload_len;
        if len > 1_048_576 {
            return Err(InnerError::RecordTooLong.into());
        }
        if buffer.len() < len {
            return Err(InnerError::EndOfOutput.into());
        }

        if flags & RecordFlags::all() != RecordFlags::empty() {
            return Err(InnerError::ReservedFlagsUsed.into());
        }

        let tag_end = HEADER_LEN + padded_tags_len;

        buffer[tag_end..tag_end + payload.len()].copy_from_slice(payload);

        buffer[HEADER_LEN..HEADER_LEN + tags_bytes.len()].copy_from_slice(tags_bytes);

        #[allow(clippy::cast_possible_truncation)]
        let payload_len = payload.len() as u32;
        buffer[LEN_P_RANGE].copy_from_slice(payload_len.to_le_bytes().as_slice());

        #[allow(clippy::cast_possible_truncation)]
        let tags_len = tags_bytes.len() as u16;
        buffer[LEN_T_RANGE].copy_from_slice(tags_len.to_le_bytes().as_slice());

        buffer[APPFLAGS_RANGE].copy_from_slice(app_flags.to_le_bytes().as_slice());
        buffer[TIMESTAMP_RANGE].copy_from_slice(timestamp.to_bytes().as_slice());

        buffer[FLAGS_RANGE].copy_from_slice(flags.bits().to_le_bytes().as_slice());

        buffer[ADDRESS_RANGE].copy_from_slice(address.as_bytes().as_slice());

        let public_key = signing_secret_key.public();
        buffer[SIGNING_KEY_RANGE].copy_from_slice(public_key.as_bytes().as_slice());

        let mut truehash: [u8; 64] = [0; 64];
        let mut hasher = blake3::Hasher::new();
        let _ = hasher.update(&buffer[HASHABLE_RANGE]);
        hasher.finalize_xof().fill(&mut truehash[..]);
        buffer[HASH_RANGE].copy_from_slice(&truehash[..40]);

        buffer[BE_TIMESTAMP_RANGE].copy_from_slice(timestamp.to_be_bytes().as_slice());

        // Sign
        let digest = crate::crypto::Blake3 { h: hasher };
        let sig = signing_secret_key
            .to_signing_key()
            .sign_prehashed(digest, Some(b"Mosaic"))?;
        buffer[SIG_RANGE].copy_from_slice(sig.to_bytes().as_slice());

        let record = Record::from_inner(&buffer[..len]);
        if cfg!(debug_assertions) {
            record.verify()?;
        }

        Ok(record)
    }

    /// Verify invariants. You should not normally need to call this; all code paths
    /// that instantiate a `Record` object call this.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if the length is too short (<`216`) too long (>`1_048_576`),
    /// if the sum of the sections (header, tags, and payload) doesn't equal the
    /// length, if either public key is invalid, if the hash is wrong, if the
    /// signature is wrong, if the timestamp is out of range, or if any reserved
    /// area is not zeroed.
    #[allow(clippy::missing_panics_doc)]
    pub fn verify(&self) -> Result<(), Error> {
        // Verify all lengths
        if self.0.len() > 1_048_576 {
            return Err(InnerError::RecordTooLong.into());
        }
        if self.0.len() < HEADER_LEN {
            return Err(InnerError::RecordTooShort.into());
        }
        if HEADER_LEN + self.tags_padded_len() + self.payload_padded_len() != self.0.len() {
            return Err(InnerError::RecordSectionLengthMismatch.into());
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
        let _ = hasher.update(&self.0[HASHABLE_RANGE]);
        hasher.finalize_xof().fill(&mut truehash[..]);

        // Compare the start of the true hash to the claimed hash
        if truehash[..40] != self.0[HASH_RANGE] {
            return Err(InnerError::HashMismatch.into());
        }

        // Verify the signature
        let signature = Signature::from_slice(&self.0[SIG_RANGE])?;
        let digest = crate::crypto::Blake3 { h: hasher };
        signing_public_key
            .to_verifying_key()
            .verify_prehashed_strict(digest, Some(b"Mosaic"), &signature)?;

        // Verify the timestamp
        let _timestamp = Timestamp::from_bytes(self.0[TIMESTAMP_RANGE].try_into().unwrap())?;

        // Verify reserved flags are 0
        let flags = self.flags();
        if flags & RecordFlags::all() != RecordFlags::empty() {
            return Err(InnerError::ReservedFlagsUsed.into());
        }

        if self.0[70] != 0 || self.0[71] != 0 {
            return Err(InnerError::IdZerosAreNotZero.into());
        }

        Ok(())
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
        let _ = hasher.update(&self.0[HASHABLE_RANGE]);
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
        padded_len!(self.tags_len())
    }

    /// Tags area bytes
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

    /// Payload padded length
    #[allow(clippy::missing_panics_doc)]
    #[must_use]
    pub fn payload_padded_len(&self) -> usize {
        padded_len!(self.payload_len())
    }

    /// Payload area bytes
    ///
    /// These are the raw bytes. If Zstd is used, the caller is responsible for
    /// decompressing them.
    #[must_use]
    pub fn payload_bytes(&self) -> &[u8] {
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
const NONCE_RANGE: Range<usize> = 144..158;
const KIND_RANGE: Range<usize> = 158..160;
const AUTHOR_KEY_RANGE: Range<usize> = 160..192;

const FLAGS_RANGE: Range<usize> = 192..194;
const TIMESTAMP_RANGE: Range<usize> = 194..200;
const APPFLAGS_RANGE: Range<usize> = 200..202;
const LEN_T_RANGE: Range<usize> = 202..204;
const LEN_P_RANGE: Range<usize> = 204..208;

const HEADER_LEN: usize = 208;
const HASHABLE_RANGE: RangeFrom<usize> = 112..;

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
            BASE64_STANDARD.encode(self.payload_bytes())
        )?;

        Ok(())
    }
}

/// An `OwnedRecord` is a digitally signed datum generated by a user,
/// stored in and retrieved from a server, and used by an application,
/// and owned.
///
/// See also `Record` for the borrowed variant.
// INVARIANTS:
//   at least 208 bytes long
//   no more than 1_048_576 bytes long
//   hash is correct
//   signature is correct
//   reserved flags are zero
//   208 + tags_padded_len() + payload_padded_len() == self.0.len()
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OwnedRecord(Vec<u8>);

impl OwnedRecord {
    /// Interpret a vector of bytes as a `Record`. Checks validity.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any verification fails. See `verify()`
    pub fn from_vec(vec: Vec<u8>) -> Result<OwnedRecord, Error> {
        let unverified = OwnedRecord(vec);
        unverified.verify()?;
        Ok(unverified)
    }

    /// Create a new `OwnedRecord` from component parts.
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any data is too long, if reserved flags are set,
    /// or if signing fails.
    #[allow(clippy::missing_panics_doc)]
    pub fn new(signing_secret_key: &SecretKey, parts: &RecordParts) -> Result<OwnedRecord, Error> {
        let address = match parts.deterministic_key {
            Some(key) => Address::new_deterministic(signing_secret_key.public(), parts.kind, key),
            None => Address::new_random(signing_secret_key.public(), parts.kind),
        };

        Self::new_replacement(
            signing_secret_key,
            address,
            parts.timestamp,
            parts.flags,
            parts.app_flags,
            parts.tags_bytes,
            parts.payload,
        )
    }

    /// Create a new `OwnedRecord` from component parts, replacing an existing record
    /// at the same address
    ///
    /// # Errors
    ///
    /// Returns an `Err` if any data is too long, if reserved flags are set,
    /// or if signing fails.
    #[allow(clippy::missing_panics_doc)]
    pub fn new_replacement(
        signing_secret_key: &SecretKey,
        address: Address,
        timestamp: Timestamp,
        flags: RecordFlags,
        app_flags: u16,
        tags_bytes: &[u8],
        payload: &[u8],
    ) -> Result<OwnedRecord, Error> {
        if tags_bytes.len() > 65_536 {
            return Err(InnerError::RecordTooLong.into());
        }
        let padded_tags_len = padded_len!(tags_bytes.len());
        let padded_payload_len = padded_len!(payload.len());
        let len = HEADER_LEN + padded_tags_len + padded_payload_len;
        if len > 1_048_576 {
            return Err(InnerError::RecordTooLong.into());
        }
        let mut buffer = vec![0; len];
        let _ = Record::write_replacement_record(
            &mut buffer,
            signing_secret_key,
            address,
            timestamp,
            flags,
            app_flags,
            tags_bytes,
            payload,
        )?;
        Ok(OwnedRecord(buffer))
    }
}

impl Deref for OwnedRecord {
    type Target = Record;

    fn deref(&self) -> &Self::Target {
        Record::from_inner(&self.0)
    }
}

impl DerefMut for OwnedRecord {
    fn deref_mut(&mut self) -> &mut Self::Target {
        Record::from_inner_mut(&mut self.0)
    }
}

impl std::fmt::Display for OwnedRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&**self, f)
    }
}

/// The parts of a Record
#[derive(Debug)]
pub struct RecordParts<'a> {
    /// The kind of record
    pub kind: Kind,

    /// Optionally a deterministic key for the Address
    pub deterministic_key: Option<&'a [u8]>,

    /// The time
    pub timestamp: Timestamp,

    /// The flags
    pub flags: RecordFlags,

    /// Application flags
    pub app_flags: u16,

    /// The tags
    pub tags_bytes: &'a [u8],

    /// The payload
    pub payload: &'a [u8],
}

impl RecordParts<'_> {
    /// Compute the length of the record that would be created from these parts
    #[must_use]
    pub fn record_len(&self) -> usize {
        let padded_tags_len = padded_len!(self.tags_bytes.len());
        let padded_payload_len = padded_len!(self.payload.len());
        HEADER_LEN + padded_tags_len + padded_payload_len
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

        let signing_secret_key = SecretKey::generate(&mut csprng);

        let r1 = OwnedRecord::new(
            &signing_secret_key,
            &RecordParts {
                kind: Kind::KEY_SCHEDULE,
                deterministic_key: None,
                timestamp: Timestamp::now().unwrap(),
                flags: RecordFlags::empty(),
                app_flags: 0,
                tags_bytes: b"",
                payload: b"hello world",
            },
        )
        .unwrap();

        println!("{r1}");

        let r2 = unsafe { Record::from_bytes(r1.as_bytes()).unwrap() };

        println!("r2 built");

        assert_eq!(*r1, *r2);
    }
}
