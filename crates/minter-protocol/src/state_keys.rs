//! Keys in the IAVL account store shared by execution and proof verification.

use crate::{Address, SigningHash};

pub fn account_key(address: Address, suffix: &[u8]) -> Vec<u8> {
    [b"a".as_slice(), &address, suffix].concat()
}

pub fn balance_key(address: Address, coin: u32) -> Vec<u8> {
    [b"a".as_slice(), &address, b"b", &coin.to_le_bytes()].concat()
}

pub fn used_check_key(hash: SigningHash) -> Vec<u8> {
    [b"t".as_slice(), &hash].concat()
}
