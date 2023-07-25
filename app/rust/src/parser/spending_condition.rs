use core::convert::TryFrom;

use nom::{
    bytes::complete::take,
    number::complete::{be_u8, be_u16, be_u32, be_u64},
};

use arrayvec::ArrayVec;

use crate::parser::c32;
use crate::parser::error::ParserError;
use crate::parser::parser_common::{
    HashMode, TransactionVersion, C32_ENCODED_ADDRS_LENGTH, PUBKEY_LEN, SIGNATURE_LEN,
};
use crate::{check_canary, zxformat};

// this includes:
// 16-byte origin fee and nonce
// 66-byte origin signature
const STANDARD_SINGLESIG_AUTH_LEN: usize = 82;

// according to the docs, de vector of fields should be cleared
// so:
// 16-byte origin fee and nonce
// 4-byte num auth fields
// 2-byte num signatures required
const STANDARD_MULTISIG_AUTH_LEN: usize = 22;

// This includes:
// - 1-byte hash mode
// - 20-byte public key hash
// - 8-byte nonce.
// - 8-byte fee rate.
const SPENDING_CONDITION_SIGNER_LEN: usize = 37;

// we take 65-byte signature + 1-byte signature public-key encoding type
const SINGLE_SPENDING_CONDITION_LEN: usize = 66;

#[repr(u8)]
#[derive(Clone, PartialEq, Copy)]
#[cfg_attr(test, derive(Debug))]
pub enum TransactionPublicKeyEncoding {
    // ways we can encode a public key
    Compressed = 0x00,
    Uncompressed = 0x01,
}

impl TransactionPublicKeyEncoding {
    // BIPs 141 and 143 make it very clear that P2WPKH scripts may be only derived
    // from compressed public-keys
    fn is_valid_hash_mode(self, mode: HashMode) -> bool {
        if mode == HashMode::P2WPKH && self != Self::Compressed {
            return false;
        }
        true
    }
}

impl From<&TransactionAuthFieldID> for TransactionPublicKeyEncoding {
    fn from(id: &TransactionAuthFieldID) -> Self {
        match id {
            TransactionAuthFieldID::PublicKeyCompressed | TransactionAuthFieldID::SignatureCompressed => Self::Compressed,
            TransactionAuthFieldID::PublicKeyUncompressed | TransactionAuthFieldID::SignatureUncompressed => Self::Uncompressed,
        }
    }
}

/// Transaction signatures are validated by calculating the public key from the signature, and
/// verifying that all public keys hash to the signing account's hash.  To do so, we must preserve
/// enough information in the auth structure to recover each public key's bytes.
///
/// An auth field can be a public key or a signature.  In both cases, the public key (either given
/// in-the-raw or embedded in a signature) may be encoded as compressed or uncompressed.
#[repr(u8)]
#[derive(Clone, PartialEq, Copy)]
#[cfg_attr(test, derive(Debug))]
pub enum TransactionAuthFieldID {
    // types of auth fields
    PublicKeyCompressed = 0x00,
    PublicKeyUncompressed = 0x01,
    SignatureCompressed = 0x02,
    SignatureUncompressed = 0x03,
}

// {FT} Replacer 37 with SPENDING_CONDITION_SIGNER_LEN
#[repr(C)]
#[derive(PartialEq, Clone)]
#[cfg_attr(test, derive(Debug))]
pub struct SpendingConditionSigner<'a> {
    pub data: &'a [u8; SPENDING_CONDITION_SIGNER_LEN],
}

impl<'a> SpendingConditionSigner<'a> {
    #[inline(never)]
    pub fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        let (raw, _) = take(SPENDING_CONDITION_SIGNER_LEN)(bytes)?;
        let data = arrayref::array_ref!(bytes, 0, SPENDING_CONDITION_SIGNER_LEN);
        Ok((raw, Self { data }))
    }

    pub fn hash_mode(&self) -> Result<HashMode, ParserError> {
        HashMode::try_from(self.data[0])
    }

    fn to_mainnet_address(
        &self,
        mode: HashMode,
    ) -> Result<arrayvec::ArrayVec<[u8; C32_ENCODED_ADDRS_LENGTH]>, ParserError> {
        c32::c32_address(mode.to_version_mainnet(), &self.data[1..21])
    }

    fn to_testnet_address(
        &self,
        mode: HashMode,
    ) -> Result<arrayvec::ArrayVec<[u8; C32_ENCODED_ADDRS_LENGTH]>, ParserError> {
        c32::c32_address(mode.to_version_testnet(), &self.data[1..21])
    }

    pub fn signer_address(
        &self,
        chain: TransactionVersion,
    ) -> Result<arrayvec::ArrayVec<[u8; C32_ENCODED_ADDRS_LENGTH]>, ParserError> {
        let mode = self.hash_mode()?;
        if chain == TransactionVersion::Testnet {
            self.to_testnet_address(mode)
        } else {
            self.to_mainnet_address(mode)
        }
    }

    pub fn pub_key_hash(&self) -> &[u8] {
        &self.data[1..21]
    }

    pub fn nonce(&self) -> Result<u64, ParserError> {
        be_u64::<'a, ParserError>(&self.data[21..])
            .map(|res| res.1)
            .map_err(|_| ParserError::parser_unexpected_value)
    }

    pub fn fee(&self) -> Result<u64, ParserError> {
        be_u64::<'a, ParserError>(&self.data[29..])
            .map(|res| res.1)
            .map_err(|_| ParserError::parser_unexpected_value)
    }

    #[inline(never)]
    pub fn nonce_str(&self) -> Result<ArrayVec<[u8; zxformat::MAX_STR_BUFF_LEN]>, ParserError> {
        let mut output = ArrayVec::from([0u8; zxformat::MAX_STR_BUFF_LEN]);
        let nonce = self.nonce()?;
        let len = zxformat::u64_to_str(&mut output[..zxformat::MAX_STR_BUFF_LEN], nonce)? as usize;
        unsafe {
            output.set_len(len);
        }
        Ok(output)
    }

    #[inline(never)]
    pub fn fee_str(&self) -> Result<ArrayVec<[u8; zxformat::MAX_STR_BUFF_LEN]>, ParserError> {
        let mut output = ArrayVec::from([0u8; zxformat::MAX_STR_BUFF_LEN]);
        let fee = self.fee()?;
        let len = zxformat::u64_to_str(output.as_mut(), fee)? as usize;
        unsafe {
            output.set_len(len);
        }
        Ok(output)
    }
}

#[repr(C)]
#[derive(PartialEq, Clone)]
#[cfg_attr(test, derive(Debug))]
pub struct SinglesigSpendingCondition<'a>(&'a [u8; SINGLE_SPENDING_CONDITION_LEN]);

/// Each field in a `MultisigSpendingCondition` can be:
///  - A pubkey if potential signer has not signed
///  - A signature with recoverable pubkey if signer has signed
#[derive(PartialEq, Clone)]
#[cfg_attr(test, derive(Debug))]
pub enum TransactionAuthField<'a> {
    PublicKey(TransactionPublicKeyEncoding, &'a [u8; PUBKEY_LEN]),
    Signature(TransactionPublicKeyEncoding, &'a [u8; SIGNATURE_LEN]),
}

/// A structure that encodes enough state to authenticate
/// a transaction's execution against a Stacks address.
/// public_keys + signatures_required determines the Principal.
/// nonce is the "check number" for the Principal.
#[derive(PartialEq, Clone)]
#[cfg_attr(test, derive(Debug))]
pub struct MultisigSpendingCondition<'a> {
    /// Keep to allow access to entire structure as raw bytes
    pub raw: &'a [u8],
    /// Public key or signature of each potential signer
    pub auth_fields: ArrayVec<TransactionAuthField<'a>, 16>,
    /// # of signatures from potential signer set for tx to be valid
    pub signatures_required: u16,
}

#[repr(C)]
#[derive(PartialEq, Clone)]
#[cfg_attr(test, derive(Debug))]
pub enum SpendingConditionSignature<'a> {
    Singlesig(SinglesigSpendingCondition<'a>),
    Multisig(MultisigSpendingCondition<'a>),
}

impl<'a> SpendingConditionSignature<'a> {
    fn clear_signature(&mut self) {
        match self {
            Self::Singlesig(ref mut singlesig) => singlesig.clear_signature(),
            Self::Multisig(ref mut multisig) => multisig.clear_signature(),
        }
    }

    pub fn required_signatures(self) -> Option<u16> {
        match self {
            Self::Multisig(ref multisig) => multisig.required_signatures().ok(),
            _ => None,
        }
    }
}

#[repr(C)]
#[derive(PartialEq, Clone)]
#[cfg_attr(test, derive(Debug))]
pub struct TransactionSpendingCondition<'a> {
    pub signer: SpendingConditionSigner<'a>,
    signature: SpendingConditionSignature<'a>,
}

impl<'a> SinglesigSpendingCondition<'a> {
    #[inline(never)]
    pub fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        // we take 65-byte signature + 1-byte signature public-key encoding type
        let len = SIGNATURE_LEN as usize + 1;
        let (raw, _) = take(len)(bytes)?;
        let data = arrayref::array_ref!(bytes, 0, SINGLE_SPENDING_CONDITION_LEN);
        check_canary!();
        Ok((raw, Self(data)))
    }

    pub fn key_encoding(&self) -> Result<TransactionPublicKeyEncoding, ParserError> {
        match self.0[0] {
            x if x == TransactionPublicKeyEncoding::Compressed as u8 => {
                Ok(TransactionPublicKeyEncoding::Compressed)
            }
            x if x == TransactionPublicKeyEncoding::Uncompressed as u8 => {
                Ok(TransactionPublicKeyEncoding::Uncompressed)
            }
            _ => Err(ParserError::parser_invalid_pubkey_encoding),
        }
    }

    fn clear_signature(&mut self) {
        let ptr = self.0.as_ptr();
        unsafe {
            let ptr = ptr as *mut u8;
            // Set the signature encoding type to Compressed
            ptr.write_bytes(TransactionPublicKeyEncoding::Compressed as u8, 1);
            // zeroize the signature
            ptr.write_bytes(0, SIGNATURE_LEN);
        }
    }
}

impl<'a> TransactionAuthField<'a> {
    #[inline(never)]
    pub fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        let (bytes, id) = be_u8(bytes)?;
        match id {
            TransactionAuthFieldID::PublicKeyCompressed
            | TransactionAuthFieldID::PublicKeyUncompressed => {
                let (bytes, buf) = take(PUBKEY_LEN)(bytes)?;
                Ok(bytes, Self::PublicKey(id.into(), buf))
            }
            TransactionAuthFieldID::SignatureCompressed
            | TransactionAuthFieldID::SignatureUncompressed => {
                let (bytes, buf) = take(SIGNATURE_LEN)(bytes)?;
                Ok(bytes, Self::Signature(id.into(), buf))
            }
            _ => return Err(nom::Err::Error(ParserError::parser_unexpected_value)),
        }
    }
}

impl<'a> MultisigSpendingCondition<'a> {
    #[inline(never)]
    pub fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        // first get the number of auth-fields
        let (mut end, num_fields) = be_u32(bytes)?;
        let mut auth_fields = ArrayVec::new();
        for i in 0..num_fields {
            let (e, tx) = TransactionAuthField::from_bytes(end)?;
            auth_fields[i] = tx;
            end = e;
        }

        // Get # of sigs required to sign tx, and check it's not too high
        let (end, signature_count) = be_u16(end)?;
        if signature_count > num_fields {
            return Err(nom::Err::Error(ParserError::parser_value_out_of_range));
        }

        // Keep reference to this entire section as raw, unparsed slice
        let taken = bytes.len() - end.len();
        let (bytes, raw) = take(taken)(bytes);

        Ok((
            bytes,
            Self {
                raw,
                auth_fields,
                signature_count,
            },
        ))
    }

    pub fn required_signatures(&self) -> u16 {
        self.signatures_required
    }

    pub fn num_fields(&self) -> u32 {
        let len = self.auth_fields.len();
        usize::try_from(len).apdu_expect("usize -> u32 failed");
    }

    fn clear_signature(&mut self) {
        let ptr = self.raw.as_ptr();
        // clear all the multisig data except for the last 2-bytes
        // which are the signature count
        let len = self.raw.len() - 2;
        unsafe {
            let ptr = ptr as *mut u8;
            // zeroize the auth fields
            ptr.write_bytes(0, len);
        }
    }

    // If it is a multisig sponsor
    // then clear it as a singlesig spending condition
    fn clear_as_singlesig(&mut self) {
        // TODO: check if it involves shrinking
        // the general transaction buffer
        todo!();
    }
}

impl<'a> TransactionSpendingCondition<'a> {
    #[inline(never)]
    pub fn from_bytes(bytes: &'a [u8]) -> nom::IResult<&[u8], Self, ParserError> {
        let (raw, signer) = SpendingConditionSigner::from_bytes(bytes)?;
        let hash_mode = signer.hash_mode()?;
        let (leftover, signature) = match hash_mode {
            HashMode::P2PKH | HashMode::P2WPKH => {
                let (raw, sig) = SinglesigSpendingCondition::from_bytes(raw)?;
                if !sig.key_encoding()?.is_valid_hash_mode(hash_mode) {
                    return Err(nom::Err::Error(ParserError::parser_invalid_pubkey_encoding));
                }
                (raw, SpendingConditionSignature::Singlesig(sig))
            }
            HashMode::P2WSH | HashMode::P2SH => {
                let sig = MultisigSpendingCondition::from_bytes(raw)?;
                (sig.0, SpendingConditionSignature::Multisig(sig.1))
            }
        };
        Ok((leftover, Self { signer, signature }))
    }

    #[inline(never)]
    pub fn signer_address(
        &self,
        chain: TransactionVersion,
    ) -> Result<arrayvec::ArrayVec<[u8; C32_ENCODED_ADDRS_LENGTH]>, ParserError> {
        self.signer.signer_address(chain)
    }

    pub fn signer_pub_key_hash(&self) -> &[u8] {
        self.signer.pub_key_hash()
    }

    #[inline(never)]
    pub fn nonce_str(&self) -> Result<ArrayVec<[u8; zxformat::MAX_STR_BUFF_LEN]>, ParserError> {
        self.signer.nonce_str()
    }

    #[inline(never)]
    pub fn fee_str(&self) -> Result<ArrayVec<[u8; zxformat::MAX_STR_BUFF_LEN]>, ParserError> {
        self.signer.fee_str()
    }

    pub fn nonce(&self) -> u64 {
        self.signer.nonce().unwrap_or(0)
    }

    pub fn fee(&self) -> u64 {
        self.signer.fee().unwrap_or(0)
    }

    pub fn is_singlesig(&self) -> bool {
        matches!(self.signature, SpendingConditionSignature::Singlesig(..))
    }

    pub fn is_multisig(&self) -> bool {
        matches!(self.signature, SpendingConditionSignature::Multisig(..))
    }

    pub fn num_auth_fields(&self) -> Option<u32> {
        match self.signature {
            SpendingConditionSignature::Multisig(ref sig) => Some(sig.num_fields()),
            _ => None,
        }
    }

    pub fn required_signatures(&self) -> Option<u16> {
        match self.signature {
            SpendingConditionSignature::Multisig(ref sig) => Some(sig.required_signatures()),
            _ => None,
        }
    }

    pub fn init_sighash(&self, buf: &mut [u8]) -> Result<usize, ParserError> {
        let buf_len = buf.len();

        if self.is_singlesig() && buf_len >= STANDARD_SINGLESIG_AUTH_LEN {
            // fills:
            // 16-byte origins fee and nonce
            // 66-byte origins signature and key encoding
            buf.iter_mut()
                .take(STANDARD_SINGLESIG_AUTH_LEN)
                .for_each(|v| *v = 0);

            return Ok(STANDARD_SINGLESIG_AUTH_LEN);
        } else if self.is_multisig() && buf_len >= STANDARD_MULTISIG_AUTH_LEN {
            // fills with zeroes
            // 16-byte fee and nonce
            // 4-byte num auth fields
            buf.iter_mut().take(20).for_each(|v| *v = 0);

            // append the signatures count at the end 2-bytes
            let count = self
                .required_signatures()
                .ok_or(ParserError::parser_no_data)?;
            buf[20..STANDARD_MULTISIG_AUTH_LEN].copy_from_slice(&count.to_be_bytes());
            return Ok(STANDARD_MULTISIG_AUTH_LEN);
        }
        Err(ParserError::parser_no_data)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::prelude::v1::*;

    #[test]
    fn test_spending_condition_p2pkh() {
        // p2pkh
        let hash = [0x11; 20];
        let sign_uncompressed = [0xff; 65];
        let sign_compressed = [0xfe; 65];

        let mut spending_condition_signer = vec![HashMode::P2PKH as u8];
        spending_condition_signer.extend_from_slice(hash.as_ref());
        spending_condition_signer.extend_from_slice(123usize.to_be_bytes().as_ref());
        spending_condition_signer.extend_from_slice(456usize.to_be_bytes().as_ref());

        let mut signature = vec![TransactionPublicKeyEncoding::Uncompressed as u8];
        signature.extend_from_slice(sign_uncompressed.as_ref());

        let spending_condition_p2pkh_uncompressed = TransactionSpendingCondition {
            signer: SpendingConditionSigner {
                data: arrayref::array_ref!(
                    spending_condition_signer,
                    0,
                    SPENDING_CONDITION_SIGNER_LEN
                ),
            },
            signature: SpendingConditionSignature::Singlesig(SinglesigSpendingCondition(
                arrayref::array_ref!(signature, 0, SINGLE_SPENDING_CONDITION_LEN),
            )),
        };

        let spending_condition_p2pkh_uncompressed_bytes = vec![
            // hash mode
            HashMode::P2PKH as u8,
            // signer
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            // nonce
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x7b,
            // fee rate
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0xc8,
            // key encoding,
            TransactionPublicKeyEncoding::Uncompressed as u8,
            // signature
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
        ];

        /*let spending_condition_signer = SpendingConditionSigner {
            signer: Hash160(hash.as_ref()),
            hash_mode: HashMode::P2PKH,
            nonce: 345,
            fee_rate: 456,
        };
        let spending_condition_p2pkh_compressed = TransactionSpendingCondition {
            signer: spending_condition_signer,
            signature: SpendingConditionSignature::Singlesig(SinglesigSpendingCondition {
                key_encoding: TransactionPublicKeyEncoding::Compressed,
                signature: MessageSignature(sign_compressed.as_ref()),
            }),
        };*/

        let mut spending_condition_signer = vec![HashMode::P2PKH as u8];
        spending_condition_signer.extend_from_slice(hash.as_ref());
        spending_condition_signer.extend_from_slice(345usize.to_be_bytes().as_ref());
        spending_condition_signer.extend_from_slice(456usize.to_be_bytes().as_ref());

        let mut signature = vec![TransactionPublicKeyEncoding::Compressed as u8];
        signature.extend_from_slice(sign_compressed.as_ref());

        let spending_condition_p2pkh_compressed = TransactionSpendingCondition {
            signer: SpendingConditionSigner {
                data: arrayref::array_ref!(
                    spending_condition_signer,
                    0,
                    SPENDING_CONDITION_SIGNER_LEN
                ),
            },
            signature: SpendingConditionSignature::Singlesig(SinglesigSpendingCondition(
                arrayref::array_ref!(signature, 0, SINGLE_SPENDING_CONDITION_LEN),
            )),
        };

        let spending_condition_p2pkh_compressed_bytes = vec![
            // hash mode
            HashMode::P2PKH as u8,
            // signer
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            // nonce
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0x59,
            // fee rate
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0xc8,
            // key encoding
            TransactionPublicKeyEncoding::Compressed as u8,
            // signature
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
        ];

        let (bytes, compressed) =
            TransactionSpendingCondition::from_bytes(&spending_condition_p2pkh_compressed_bytes)
                .unwrap();
        assert_eq!(spending_condition_p2pkh_compressed, compressed);
        assert_eq!(bytes.len(), 0);

        let (bytes, uncompressed) =
            TransactionSpendingCondition::from_bytes(&spending_condition_p2pkh_uncompressed_bytes)
                .unwrap();
        assert_eq!(spending_condition_p2pkh_uncompressed, uncompressed);
        assert_eq!(bytes.len(), 0);
    }

    #[test]
    fn test_spending_condition_p2wpkh() {
        let hash = [0x11; 20];
        let sign_compressed = [0xfe; 65];

        /* let spending_condition_signer = SpendingConditionSigner {
            signer: Hash160(hash.as_ref()),
            hash_mode: HashMode::P2WPKH,
            nonce: 345,
            fee_rate: 567,
        };
        let spending_condition_p2pwkh_compressed = TransactionSpendingCondition {
            signer: spending_condition_signer,
            signature: SpendingConditionSignature::Singlesig(SinglesigSpendingCondition {
                key_encoding: TransactionPublicKeyEncoding::Compressed,
                signature: MessageSignature(sign_compressed.as_ref()),
            }),
        };*/

        let mut spending_condition_signer = vec![HashMode::P2WPKH as u8];
        spending_condition_signer.extend_from_slice(hash.as_ref());
        spending_condition_signer.extend_from_slice(345usize.to_be_bytes().as_ref());
        spending_condition_signer.extend_from_slice(567usize.to_be_bytes().as_ref());

        let mut signature = vec![TransactionPublicKeyEncoding::Compressed as u8];
        signature.extend_from_slice(sign_compressed.as_ref());

        let spending_condition_p2wpkh_compressed = TransactionSpendingCondition {
            signer: SpendingConditionSigner {
                data: arrayref::array_ref!(
                    spending_condition_signer,
                    0,
                    SPENDING_CONDITION_SIGNER_LEN
                ),
            },
            signature: SpendingConditionSignature::Singlesig(SinglesigSpendingCondition(
                arrayref::array_ref!(signature, 0, SINGLE_SPENDING_CONDITION_LEN),
            )),
        };

        let spending_condition_p2wpkh_compressed_bytes = vec![
            // hash mode
            HashMode::P2WPKH as u8,
            // signer
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            // nonce
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0x59,
            // fee rate
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x02,
            0x37,
            // key encoding
            TransactionPublicKeyEncoding::Compressed as u8,
            // signature
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
        ];

        let (bytes, decoded) =
            TransactionSpendingCondition::from_bytes(&spending_condition_p2wpkh_compressed_bytes)
                .unwrap();
        assert_eq!(bytes.len(), 0);
        assert_eq!(spending_condition_p2wpkh_compressed, decoded);
    }

    #[test]
    fn test_invalid_spending_conditions() {
        let bad_hash_mode_bytes = vec![
            // singlesig
            // hash mode
            0xff,
            // signer
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            // nonce
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0xc8,
            // fee rate
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x02,
            0x37,
            // key encoding,
            TransactionPublicKeyEncoding::Compressed as u8,
            // signature
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
            0xfd,
        ];
        let bad_hash_mode = TransactionSpendingCondition::from_bytes(&bad_hash_mode_bytes);
        assert!(bad_hash_mode.is_err());

        let bad_p2wpkh_uncompressed_bytes = vec![
            // hash mode
            HashMode::P2WSH as u8,
            // signer
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            // nonce
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x7b,
            // fee rate
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x02,
            0x37,
            // public key uncompressed
            TransactionPublicKeyEncoding::Uncompressed as u8,
            // signature
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
        ];
        let bad_signature =
            TransactionSpendingCondition::from_bytes(&bad_p2wpkh_uncompressed_bytes);
        assert!(bad_signature.is_err());
    }

    #[test]
    fn tx_spending_condition_p2sh() {
        // p2sh

        let spending_condition_p2sh_uncompressed_bytes = vec![
            // hash mode
            HashMode::P2SH as u8,
            // signer
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            // nonce
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x7b,
            // fee rate
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0xc8,
            // fields length
            0x00,
            0x00,
            0x00,
            0x03,
            // field #1: signature
            TransactionAuthFieldID::SignatureUncompressed as u8,
            // field #1: signature
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            // field #2: signature
            TransactionAuthFieldID::SignatureUncompressed as u8,
            // filed #2: signature
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            // field #3: public key
            TransactionAuthFieldID::PublicKeyUncompressed as u8,
            // field #3: key (compressed)
            0x03,
            0xef,
            0x23,
            0x40,
            0x51,
            0x8b,
            0x58,
            0x67,
            0xb2,
            0x35,
            0x98,
            0xa9,
            0xcf,
            0x74,
            0x61,
            0x1f,
            0x8b,
            0x98,
            0x06,
            0x4f,
            0x7d,
            0x55,
            0xcd,
            0xb8,
            0xc1,
            0x07,
            0xc6,
            0x7b,
            0x5e,
            0xfc,
            0xbc,
            0x5c,
            0x77,
            // number of signatures required
            0x00,
            0x02,
        ];

        let (_raw, decoded) = TransactionSpendingCondition::from_bytes(
            spending_condition_p2sh_uncompressed_bytes.as_ref(),
        )
        .unwrap();

        assert_eq!(2, decoded.required_signatures().unwrap());
        assert_eq!(3, decoded.num_auth_fields().unwrap());

        assert_eq!(123, decoded.nonce());
        assert_eq!(456, decoded.fee());

        let spending_condition_p2sh_compressed_bytes = vec![
            // hash mode
            HashMode::P2SH as u8,
            // signer
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            // nonce
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0xc8,
            // fee rate
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x02,
            0x37,
            // fields length
            0x00,
            0x00,
            0x00,
            0x03,
            // field #1: signature
            TransactionAuthFieldID::SignatureCompressed as u8,
            // field #1: signature
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            // field #2: signature
            TransactionAuthFieldID::SignatureCompressed as u8,
            // filed #2: signature
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            // field #3: public key
            TransactionAuthFieldID::PublicKeyCompressed as u8,
            // field #3: key (compressed)
            0x03,
            0xef,
            0x23,
            0x40,
            0x51,
            0x8b,
            0x58,
            0x67,
            0xb2,
            0x35,
            0x98,
            0xa9,
            0xcf,
            0x74,
            0x61,
            0x1f,
            0x8b,
            0x98,
            0x06,
            0x4f,
            0x7d,
            0x55,
            0xcd,
            0xb8,
            0xc1,
            0x07,
            0xc6,
            0x7b,
            0x5e,
            0xfc,
            0xbc,
            0x5c,
            0x77,
            // number of signatures
            0x00,
            0x02,
        ];

        let (_raw, decoded) = TransactionSpendingCondition::from_bytes(
            spending_condition_p2sh_compressed_bytes.as_ref(),
        )
        .unwrap();
        assert_eq!(2, decoded.required_signatures().unwrap());
        assert_eq!(3, decoded.num_auth_fields().unwrap());

        assert_eq!(456, decoded.nonce());
        assert_eq!(567, decoded.fee());
    }

    #[test]
    fn tx_spending_condition_p2wsh() {
        let spending_condition_p2wsh_bytes = vec![
            // hash mode
            HashMode::P2WSH as u8,
            // signer
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            0x11,
            // nonce
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x01,
            0xc8,
            // fee rate
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x00,
            0x02,
            0x37,
            // fields length
            0x00,
            0x00,
            0x00,
            0x03,
            // field #1: signature
            TransactionAuthFieldID::SignatureCompressed as u8,
            // field #1: signature
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            0xff,
            // field #2: signature
            TransactionAuthFieldID::SignatureCompressed as u8,
            // filed #2: signature
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            0xfe,
            // field #3: public key
            TransactionAuthFieldID::PublicKeyCompressed as u8,
            // field #3: key (compressed)
            0x03,
            0xef,
            0x23,
            0x40,
            0x51,
            0x8b,
            0x58,
            0x67,
            0xb2,
            0x35,
            0x98,
            0xa9,
            0xcf,
            0x74,
            0x61,
            0x1f,
            0x8b,
            0x98,
            0x06,
            0x4f,
            0x7d,
            0x55,
            0xcd,
            0xb8,
            0xc1,
            0x07,
            0xc6,
            0x7b,
            0x5e,
            0xfc,
            0xbc,
            0x5c,
            0x77,
            // number of signatures
            0x00,
            0x02,
        ];

        let (_raw, decoded) =
            TransactionSpendingCondition::from_bytes(spending_condition_p2wsh_bytes.as_ref())
                .unwrap();
        assert_eq!(2, decoded.required_signatures().unwrap());
        assert_eq!(3, decoded.num_auth_fields().unwrap());

        assert_eq!(456, decoded.nonce());
        assert_eq!(567, decoded.fee());
    }
}
