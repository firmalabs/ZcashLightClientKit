use failure::format_err;
use ffi_helpers::panic::catch_panic;
use std::ffi::{CStr, CString, OsStr};
use std::os::raw::c_char;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::slice;
use std::str::FromStr;
use zcash_client_backend::{
    address::RecipientAddress,
    wallet::AccountId,
    data_api::{
        chain::{scan_cached_blocks, validate_chain},
        error::Error,
        wallet::{create_spend_to_address, decrypt_and_store_transaction},
        WalletRead, WalletWrite,
    },
    encoding::{
        decode_extended_full_viewing_key, decode_extended_spending_key,
        encode_extended_full_viewing_key, encode_extended_spending_key, encode_payment_address,
    },
    keys::spending_key,
    wallet::OvkPolicy,
};
use zcash_client_sqlite::{
    wallet::{
        init::{init_accounts_table, init_blocks_table, init_wallet_db},
        get_balance, 
        get_balance_at,
    },
    BlockDB, NoteId, WalletDB,
    chain::{UnspentTransactionOutput, get_all_utxos, get_confirmed_utxos_for_address}
};
use zcash_primitives::{
    block::BlockHash,
    consensus::{self,BlockHeight, BranchId, Parameters},
    note_encryption::Memo,
    transaction::components::{
            amount::DEFAULT_FEE,
            Amount, 
            OutPoint, 
            TxOut
    },
    legacy::Script,
    transaction::builder::Builder,
    transaction::Transaction,
    zip32::{ExtendedFullViewingKey},
};

#[cfg(feature = "mainnet")]
use zcash_primitives::consensus::{MainNetwork, MAIN_NETWORK};
#[cfg(not(feature = "mainnet"))]
use zcash_primitives::consensus::{TestNetwork, TEST_NETWORK};

use zcash_proofs::prover::LocalTxProver;

use std::convert::TryFrom;

// /////////////////////////////////////////////////////////////////////////////////////////////////
// Temporary Imports
use base58::ToBase58;
use sha2::{Digest, Sha256};
// use zcash_primitives::legacy::TransparentAddress;
use hdwallet::{ExtendedPrivKey, KeyIndex};
use secp256k1::{
    Secp256k1,
    key::{PublicKey, SecretKey},
};


// use crate::extended_key::{key_index::KeyIndex, ExtendedPrivKey, ExtendedPubKey, KeySeed};
// /////////////////////////////////////////////////////////////////////////////////////////////////


fn unwrap_exc_or<T>(exc: Result<T, ()>, def: T) -> T {
    match exc {
        Ok(value) => value,
        Err(_) => def,
    }
}

fn unwrap_exc_or_null<T>(exc: Result<T, ()>) -> T
where
    T: ffi_helpers::Nullable,
{
    match exc {
        Ok(value) => value,
        Err(_) => ffi_helpers::Nullable::NULL,
    }
}

#[cfg(feature = "mainnet")]
pub const NETWORK: MainNetwork = MAIN_NETWORK;

#[cfg(not(feature = "mainnet"))]
pub const NETWORK: TestNetwork = TEST_NETWORK;


fn wallet_db<P: consensus::Parameters>(params: &P,db_data: *const u8,
    db_data_len: usize) -> Result<WalletDB<P>, failure::Error> {
        
    let db_data = Path::new(OsStr::from_bytes(unsafe {
            slice::from_raw_parts(db_data, db_data_len)
        }));
    WalletDB::for_path(db_data, *params)
        .map_err(|e| format_err!("Error opening wallet database connection: {}", e))
}

fn block_db(cache_db: *const u8,
    cache_db_len: usize) -> Result<BlockDB, failure::Error> {
   
    let cache_db = Path::new(OsStr::from_bytes(unsafe {
        slice::from_raw_parts(cache_db, cache_db_len)
    }));
    BlockDB::for_path(cache_db)
    .map_err(|e| format_err!("Error opening block source database connection: {}", e))
 
}

/// Returns the length of the last error message to be logged.
#[no_mangle]
pub extern "C" fn zcashlc_last_error_length() -> i32 {
    ffi_helpers::error_handling::last_error_length()
}

/// Copies the last error message into the provided allocated buffer.
#[no_mangle]
pub unsafe extern "C" fn zcashlc_error_message_utf8(buf: *mut c_char, length: i32) -> i32 {
    ffi_helpers::error_handling::error_message_utf8(buf, length)
}

/// Clears the record of the last error message.
#[no_mangle]
pub extern "C" fn zcashlc_clear_last_error() {
    ffi_helpers::error_handling::clear_last_error()
}

/// Sets up the internal structure of the data database.
#[no_mangle]
pub extern "C" fn zcashlc_init_data_database(db_data: *const u8, db_data_len: usize) -> i32 {
    let res = catch_panic(|| {
        let db_data = Path::new(OsStr::from_bytes(unsafe {
            slice::from_raw_parts(db_data, db_data_len)
        }));
     
            WalletDB::for_path(db_data, NETWORK)
                    .map(|db| init_wallet_db(&db))
                    .map(|_| 1)
                    .map_err(|e| format_err!("Error while initializing data DB: {}", e))
    });
    unwrap_exc_or_null(res)
}

/// Initialises the data database with the given number of accounts using the given seed.
///
/// Returns the ExtendedSpendingKeys for the accounts. The caller should store these
/// securely for use while spending.
///
/// Call `zcashlc_vec_string_free` on the returned pointer when you are finished with it.
#[no_mangle]
pub extern "C" fn zcashlc_init_accounts_table(
    db_data: *const u8,
    db_data_len: usize,
    seed: *const u8,
    seed_len: usize,
    accounts: i32,
    capacity_ret: *mut usize,
) -> *mut *mut c_char {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;
        let seed = unsafe { slice::from_raw_parts(seed, seed_len) };
        let accounts = if accounts >= 0 {
            accounts as u32
        } else {
            return Err(format_err!("accounts argument must be positive"));
        };

        let extsks: Vec<_> = (0..accounts)
            .map(|account| spending_key(&seed, NETWORK.coin_type(), account))
            .collect();
        let extfvks: Vec<_> = extsks.iter().map(ExtendedFullViewingKey::from).collect();

        init_accounts_table(&db_data, &extfvks)
            .map(|_| {
                 // Return the ExtendedSpendingKeys for the created accounts.
                let mut v: Vec<_> = extsks
                .iter()
                .map(|extsk| {
                    let encoded =
                        encode_extended_spending_key(NETWORK.hrp_sapling_extended_spending_key(), extsk);
                    CString::new(encoded).unwrap().into_raw()
                })
                .collect();
                assert!(v.len() == accounts as usize);
                unsafe { *capacity_ret.as_mut().unwrap() = v.capacity() };
                let p = v.as_mut_ptr();
                std::mem::forget(v);
                return p;
            })
            .map_err(|e| format_err!("Error while initializing accounts: {}", e))

    });
    unwrap_exc_or_null(res)
}

/// Initialises the data database with the given extended full viewing keys
/// Call `zcashlc_vec_string_free` on the returned pointer when you are finished with it.
#[no_mangle]
pub extern "C" fn zcashlc_init_accounts_table_with_keys(
    db_data: *const u8,
    db_data_len: usize,
    extfvks: *const *const c_char,
    extfvks_len: usize,
) -> bool {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;

        let extfvks = unsafe { std::slice::from_raw_parts(extfvks, extfvks_len)
            .into_iter()
            .map(|s| CStr::from_ptr(*s).to_str().unwrap())
            .map( |vkstr|
                decode_extended_full_viewing_key(NETWORK.hrp_sapling_extended_full_viewing_key(), &vkstr)
                    .unwrap()
                    .unwrap()
            ).collect::<Vec<_>>() };
        
        match init_accounts_table(&db_data, &extfvks) {
            Ok(()) => Ok(true),
            Err(e) => Err(format_err!("Error while initializing accounts: {}", e)),
        }

    });
    unwrap_exc_or(res, false)
}

/// Derives Extended Spending Keys from the given seed into 'accounts' number of accounts.
/// Returns the ExtendedSpendingKeys for the accounts. The caller should store these
/// securely for use while spending.
///
/// Call `zcashlc_vec_string_free` on the returned pointer when you are finished with it.
#[no_mangle]
pub unsafe extern "C" fn zcashlc_derive_extended_spending_keys(
    seed: *const u8,
    seed_len: usize,
    accounts: i32,
    capacity_ret: *mut usize,
) -> *mut *mut c_char {
    let res = catch_panic(|| {
        let seed = slice::from_raw_parts(seed, seed_len);
        let accounts = if accounts > 0 {
            accounts as u32
        } else {
            return Err(format_err!("accounts argument must be greater than zero"));
        };

        let extsks: Vec<_> = (0..accounts)
            .map(|account| spending_key(&seed, NETWORK.coin_type(), account))
            .collect();

        // Return the ExtendedSpendingKeys for the created accounts.
        let mut v: Vec<_> = extsks
            .iter()
            .map(|extsk| {
                let encoded =
                    encode_extended_spending_key(NETWORK.hrp_sapling_extended_spending_key(), extsk);
                CString::new(encoded).unwrap().into_raw()
            })
            .collect();
        assert!(v.len() == accounts as usize);
        *capacity_ret.as_mut().unwrap() = v.capacity();
        let p = v.as_mut_ptr();
        std::mem::forget(v);
        Ok(p)
    });
    unwrap_exc_or_null(res)
}
/// Derives Extended Full Viewing Keys from the given seed into 'accounts' number of accounts.
/// Returns the Extended Full Viewing Keys for the accounts. The caller should store these
/// securely
///
/// Call `zcashlc_vec_string_free` on the returned pointer when you are finished with it.
#[no_mangle]
pub unsafe extern "C" fn zcashlc_derive_extended_full_viewing_keys(
    seed: *const u8,
    seed_len: usize,
    accounts: i32,
    capacity_ret: *mut usize,
) -> *mut *mut c_char {
    let res = catch_panic(|| {
        let seed = slice::from_raw_parts(seed, seed_len);
        let accounts = if accounts > 0 {
            accounts as u32
        } else {
            return Err(format_err!("accounts argument must be greater than zero"));
        };

        let extsks: Vec<_> = (0..accounts)
            .map(|account| ExtendedFullViewingKey::from(&spending_key(&seed, NETWORK.coin_type(), account)))
            .collect();

        // Return the ExtendedSpendingKeys for the created accounts.
        let mut v: Vec<_> = extsks
            .iter()
            .map(|extsk| {
                let encoded =
                    encode_extended_full_viewing_key(NETWORK.hrp_sapling_extended_full_viewing_key(), extsk);
                CString::new(encoded).unwrap().into_raw()
            })
            .collect();
        assert!(v.len() == accounts as usize);
        *capacity_ret.as_mut().unwrap() = v.capacity();
        let p = v.as_mut_ptr();
        std::mem::forget(v);
        Ok(p)
    });
    unwrap_exc_or_null(res)
}
/// derives a shielded address from the given seed. 
/// call zcashlc_string_free with the returned pointer when done using it
#[no_mangle]
pub unsafe extern "C" fn zcashlc_derive_shielded_address_from_seed(
    seed: *const u8,
    seed_len: usize,
    account_index: i32,
) -> *mut c_char {
    let res = catch_panic(|| {
        let seed = slice::from_raw_parts(seed, seed_len);
        let account_index = if account_index >= 0 {
            account_index as u32
        } else {
            return Err(format_err!("accounts argument must be greater than zero"));
        };
        let address_str = derive_shielded_address_from_seed(&NETWORK, &seed, account_index);
        Ok(CString::new(address_str).unwrap().into_raw())
    });
    unwrap_exc_or_null(res)
}

fn derive_shielded_address_from_seed<P: Parameters>(params: &P, seed: &[u8], account_index: u32) -> String {
    let address = spending_key(&seed, NETWORK.coin_type(), account_index)
            .default_address()
            .unwrap()
            .1;
    encode_payment_address(params.hrp_sapling_payment_address(), &address)
}

// fn derive_shielded_address_from_spending_k<P: Parameters>(params: &P, extsk: &ExtendedSpendingKey) -> String {
//     let address = extsk.default_address().unwrap().1;
//     encode_payment_address(params.hrp_sapling_payment_address(), &address)
// }

/// derives a shielded address from the given viewing key. 
/// call zcashlc_string_free with the returned pointer when done using it
#[no_mangle]
pub unsafe extern "C" fn zcashlc_derive_shielded_address_from_viewing_key(
    extfvk: *const c_char,
) -> *mut c_char {

    let res = catch_panic(|| {
        let extfvk_string = CStr::from_ptr(extfvk).to_str()?;
        let extfvk = match decode_extended_full_viewing_key(
            NETWORK.hrp_sapling_extended_full_viewing_key(),
            &extfvk_string,
        ) {
            Ok(Some(extfvk)) => extfvk,
            Ok(None) => {
                return Err(format_err!("Failed to parse viewing key string in order to derive the address. Deriving a viewing key from the string returned no results. Encoding was valid but type was incorrect."));
            }
            Err(e) => {
                return Err(format_err!(
                    "Error while deriving viewing key from string input: {}",
                    e
                ));
            }
        };
        let address = extfvk.default_address().unwrap().1;
        let address_str = encode_payment_address(NETWORK.hrp_sapling_payment_address(), &address);
        Ok(CString::new(address_str).unwrap().into_raw())
    });
    unwrap_exc_or_null(res)
}

/// derives a shielded address from the given extended full viewing key. 
/// call zcashlc_string_free with the returned pointer when done using it
#[no_mangle]
pub unsafe extern "C" fn zcashlc_derive_extended_full_viewing_key(
    extsk: *const c_char,
) -> *mut c_char {
    let res = catch_panic(|| {
        let extsk = CStr::from_ptr(extsk).to_str()?;
        let extfvk = match decode_extended_spending_key(NETWORK.hrp_sapling_extended_spending_key(), &extsk) {
            Ok(Some(extsk)) => ExtendedFullViewingKey::from(&extsk),
            Ok(None) => {
                return Err(format_err!("Deriving viewing key from spending key returned no results. Encoding was valid but type was incorrect."));
            },
            Err(e) => {
                return Err(format_err!(
                    "Error while deriving viewing key from spending key: {}",
                    e
                ));
            },
        };

        let encoded = encode_extended_full_viewing_key(NETWORK.hrp_sapling_extended_full_viewing_key(), &extfvk);

        Ok(CString::new(encoded).unwrap().into_raw())
    });
    unwrap_exc_or_null(res)
}

/// Initialises the data database with the given block.
///
/// This enables a newly-created database to be immediately-usable, without needing to
/// synchronise historic blocks.
#[no_mangle]
pub extern "C" fn zcashlc_init_blocks_table(
    db_data: *const u8,
    db_data_len: usize,
    height: i32,
    hash_hex: *const c_char,
    time: u32,
    sapling_tree_hex: *const c_char,
) -> i32 {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;
        let hash = {
            let mut hash = hex::decode(unsafe { CStr::from_ptr(hash_hex) }.to_str()?).unwrap();
            hash.reverse();
            BlockHash::from_slice(&hash)
        };
        let sapling_tree =
            hex::decode(unsafe { CStr::from_ptr(sapling_tree_hex) }.to_str()?).unwrap();

        match init_blocks_table(&db_data, BlockHeight::from_u32(height as u32), hash,time, &sapling_tree) {
            Ok(()) => Ok(1),
            Err(e) => Err(format_err!("Error while initializing blocks table: {}", e)),
        }
    });
    unwrap_exc_or_null(res)
}

/// Returns the address for the account.
///
/// Call `zcashlc_string_free` on the returned pointer when you are finished with it.
#[no_mangle]
pub extern "C" fn zcashlc_get_address(
    db_data: *const u8,
    db_data_len: usize,
    account: i32,
) -> *mut c_char {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;
        let account = if account >= 0 {
            account as u32
        } else {
            return Err(format_err!("accounts argument must be positive"));
        };

        let account = AccountId(account);

        match (&db_data).get_address(account) {
            Ok(Some(addr)) => {
                let addr_str = encode_payment_address(NETWORK.hrp_sapling_payment_address(), &addr);
                let c_str_addr = CString::new(addr_str).unwrap();
                Ok(c_str_addr.into_raw())
            },
            Ok(None) => Err(format_err!(
                "No payment address was available for account {:?}",
                account
            )),
            Err(e) => Err(format_err!("Error while fetching address: {}", e)),
        }
    });
    unwrap_exc_or_null(res)
}

/// Returns true when the address is valid and shielded.
/// Returns false in any other case
/// Errors when the provided address belongs to another network
#[no_mangle]
pub unsafe extern "C" fn zcashlc_is_valid_shielded_address(address: *const c_char) -> bool {
    let res = catch_panic(|| {
        let addr = CStr::from_ptr(address).to_str()?;
        Ok(is_valid_shielded_address(&addr))
    });
    unwrap_exc_or(res, false)
}

fn is_valid_shielded_address(address: &str) -> bool {
    match RecipientAddress::decode(&NETWORK, &address) {
        Some(addr) => match addr {
            RecipientAddress::Shielded(_) => true,
            RecipientAddress::Transparent(_) => false,
        },
        None => false,
    }
}

/// Returns true when the address is valid and transparent.
/// Returns false in any other case
#[no_mangle]
pub unsafe extern "C" fn zcashlc_is_valid_transparent_address(address: *const c_char) -> bool {
    let res = catch_panic(|| {
        let addr = CStr::from_ptr(address).to_str()?;
        Ok(is_valid_transparent_address(&addr))
    });
    unwrap_exc_or(res, false)
}

fn is_valid_transparent_address(address: &str) -> bool {
    match RecipientAddress::decode(&NETWORK, &address) {
        Some(addr) => match addr {
            RecipientAddress::Shielded(_) => false,
            RecipientAddress::Transparent(_) => true,
        },
        None => false,
    }
}

/// Returns the balance for the account, including all unspent notes that we know about.
#[no_mangle]
pub extern "C" fn zcashlc_get_balance(db_data: *const u8, db_data_len: usize, account: i32) -> i64 {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;

        let account = if account >= 0 {
            account as u32
        } else {
            return Err(format_err!("account argument must be positive"));
        };
        let account = AccountId(account);
        match (&db_data).get_balance(account) {
            Ok(balance) => Ok(balance.into()),
            Err(e) => Err(format_err!("Error while fetching balance: {}", e)),
        }
    });
    unwrap_exc_or(res, -1)
}

/// Returns the verified balance for the account, which ignores notes that have been
/// received too recently and are not yet deemed spendable.
#[no_mangle]
pub extern "C" fn zcashlc_get_verified_balance(
    db_data: *const u8,
    db_data_len: usize,
    account: i32,
) -> i64 {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;
        let account = if account >= 0 {
            account as u32
        } else {
            return Err(format_err!("account argument must be positive"));
        };
        let account = AccountId(account);
        (&db_data)
            .get_target_and_anchor_heights()
            .map_err(|e| format_err!("Error while fetching anchor height: {}", e))
            .and_then(|opt_anchor| {
                opt_anchor
                    .map(|(_, a)| a)
                    .ok_or(format_err!("Anchor height not available; scan required."))
            })
            .and_then(|anchor| {
                (&db_data)
                    .get_balance_at(account, anchor)
                    .map_err(|e| format_err!("Error while fetching verified balance: {}", e))
            })
            .map(|amount| amount.into())
    });
    unwrap_exc_or(res, -1)
}

/// Returns the memo for a received note, if it is known and a valid UTF-8 string.
///
/// The note is identified by its row index in the `received_notes` table within the data
/// database.
///
/// Call `zcashlc_string_free` on the returned pointer when you are finished with it.
#[no_mangle]
pub extern "C" fn zcashlc_get_received_memo_as_utf8(
    db_data: *const u8,
    db_data_len: usize,
    id_note: i64,
) -> *mut c_char {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;

        let memo = match (&db_data).get_received_memo_as_utf8(NoteId(id_note)) {
            Ok(memo) => memo.unwrap_or_default(),
            Err(e) => return Err(format_err!("Error while fetching memo: {}", e)),
        };

        Ok(CString::new(memo).unwrap().into_raw())
    });
    unwrap_exc_or_null(res)
}

/// Returns the memo for a sent note, if it is known and a valid UTF-8 string.
///
/// The note is identified by its row index in the `sent_notes` table within the data
/// database.
///
/// Call `zcashlc_string_free` on the returned pointer when you are finished with it.
#[no_mangle]
pub extern "C" fn zcashlc_get_sent_memo_as_utf8(
    db_data: *const u8,
    db_data_len: usize,
    id_note: i64,
) -> *mut c_char {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;

        let memo = (&db_data)
            .get_sent_memo_as_utf8(NoteId(id_note))
            .map(|memo| memo.unwrap_or_default())
            .map_err(|e| format_err!("Error while fetching memo: {}", e))?;

        Ok(CString::new(memo).unwrap().into_raw())
    });
    unwrap_exc_or_null(res)
}

/// Checks that the scanned blocks in the data database, when combined with the recent
/// `CompactBlock`s in the cache database, form a valid chain.
///
/// This function is built on the core assumption that the information provided in the
/// cache database is more likely to be accurate than the previously-scanned information.
/// This follows from the design (and trust) assumption that the `lightwalletd` server
/// provides accurate block information as of the time it was requested.
///
/// Returns:
/// - `-1` if the combined chain is valid.
/// - `upper_bound` if the combined chain is invalid.
///   `upper_bound` is the height of the highest invalid block (on the assumption that the
///   highest block in the cache database is correct).
/// - `0` if there was an error during validation unrelated to chain validity.
///
/// This function does not mutate either of the databases.
#[no_mangle]
pub extern "C" fn zcashlc_validate_combined_chain(
    db_cache: *const u8,
    db_cache_len: usize,
    db_data: *const u8,
    db_data_len: usize,
) -> i32 {
    let res = catch_panic(|| {
        let block_db = block_db(db_cache, db_cache_len)?;
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;

        let validate_from = (&db_data)
            .get_max_height_hash()
            .map_err(|e| format_err!("Error while validating chain: {}", e))?;
        
        let val_res = validate_chain(&NETWORK, &block_db, validate_from);

        if let Err(e) = val_res {
            match e.0 {
                Error::InvalidChain(upper_bound, _) => {
                    let upper_bound_u32 = u32::from(upper_bound);
                    Ok(upper_bound_u32 as i32)
                }
                _ => Err(format_err!("Error while validating chain: {}", e)),
            }
        } else {
            // All blocks are valid, so "highest invalid block height" is below genesis.
            Ok(-1)
        }
    });
    unwrap_exc_or_null(res)
}

/// Rewinds the data database to the given height.
///
/// If the requested height is greater than or equal to the height of the last scanned
/// block, this function does nothing.
#[no_mangle]
pub extern "C" fn zcashlc_rewind_to_height(
    db_data: *const u8,
    db_data_len: usize,
    height: i32,
) -> i32 {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;

        let mut update_ops = (&db_data)
            .get_update_ops()
            .map_err(|e| format_err!("Could not obtain a writable database connection: {}", e))?;

        let height = BlockHeight::try_from(height)?;
        (&mut update_ops)
            .transactionally(|ops| ops.rewind_to_height(height))
            .map(|_| 1)
            .map_err(|e| format_err!("Error while rewinding data DB to height {}: {}", height, e))
        
    });
    unwrap_exc_or_null(res)
}

/// Scans new blocks added to the cache for any transactions received by the tracked
/// accounts.
///
/// This function pays attention only to cached blocks with heights greater than the
/// highest scanned block in `db_data`. Cached blocks with lower heights are not verified
/// against previously-scanned blocks. In particular, this function **assumes** that the
/// caller is handling rollbacks.
///
/// For brand-new light client databases, this function starts scanning from the Sapling
/// activation height. This height can be fast-forwarded to a more recent block by calling
/// [`zcashlc_init_blocks_table`] before this function.
///
/// Scanned blocks are required to be height-sequential. If a block is missing from the
/// cache, an error will be signalled.
#[no_mangle]
pub extern "C" fn zcashlc_scan_blocks(
    db_cache: *const u8,
    db_cache_len: usize,
    db_data: *const u8,
    db_data_len: usize,
) -> i32 {
    let res = catch_panic(|| {
        let block_db = block_db(db_cache, db_cache_len)?;
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;

        match scan_cached_blocks(&NETWORK, &block_db, &db_data, None) {
            Ok(()) => Ok(1),
            Err(e) => Err(format_err!("Error while scanning blocks: {}", e)),
        }
    });
    unwrap_exc_or_null(res)
}

#[no_mangle]
pub extern "C" fn zcashlc_decrypt_and_store_transaction(
    db_data: *const u8,
    db_data_len: usize,
    tx: *const u8,
    tx_len: usize,
) -> i32 {
    let res = catch_panic(|| {
        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;
        let tx_bytes = unsafe { slice::from_raw_parts(tx, tx_len) };
        let tx = Transaction::read(&tx_bytes[..])?;

        match decrypt_and_store_transaction(&NETWORK, &db_data, &tx) {
            Ok(()) => Ok(1),
            Err(e) => Err(format_err!("Error while decrypting transaction: {}", e)),
        }
    });
    unwrap_exc_or_null(res)
}

/// Creates a transaction paying the specified address from the given account.
///
/// Returns the row index of the newly-created transaction in the `transactions` table
/// within the data database. The caller can read the raw transaction bytes from the `raw`
/// column in order to broadcast the transaction to the network.
///
/// Do not call this multiple times in parallel, or you will generate transactions that
/// double-spend the same notes.
#[no_mangle]
pub extern "C" fn zcashlc_create_to_address(
    db_data: *const u8,
    db_data_len: usize,
    account: i32,
    extsk: *const c_char,
    to: *const c_char,
    value: i64,
    memo: *const c_char,
    spend_params: *const u8,
    spend_params_len: usize,
    output_params: *const u8,
    output_params_len: usize,
) -> i64 {
    let res = catch_panic(|| {

        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;
        let account = if account >= 0 {
            account as u32
        } else {
            return Err(format_err!("account argument must be positive"));
        };
        let extsk = unsafe { CStr::from_ptr(extsk) }.to_str()?;
        let to = unsafe { CStr::from_ptr(to) }.to_str()?;
        let value =
            Amount::from_i64(value).map_err(|()| format_err!("Invalid amount, out of range"))?;
        if value.is_negative() {
            return Err(format_err!("Amount is negative"));
        }
        let memo = unsafe { CStr::from_ptr(memo) }.to_str()?;
        let spend_params = Path::new(OsStr::from_bytes(unsafe {
            slice::from_raw_parts(spend_params, spend_params_len)
        }));
        let output_params = Path::new(OsStr::from_bytes(unsafe {
            slice::from_raw_parts(output_params, output_params_len)
        }));

        let extsk =
            match decode_extended_spending_key(NETWORK.hrp_sapling_extended_spending_key(), &extsk)
            {
                Ok(Some(extsk)) => extsk,
                Ok(None) => {
                    return Err(format_err!("ExtendedSpendingKey is for the wrong network"));
                },
                Err(e) => {
                    return Err(format_err!("Invalid ExtendedSpendingKey: {}", e));
                },
            };

        let to = match RecipientAddress::decode(&NETWORK, &to) {
            Some(to) => to,
            None => {
                return Err(format_err!("PaymentAddress is for the wrong network"));
            },
        };

        let memo = Memo::from_str(&memo).map_err(|_| format_err!("Invalid memo"))?;

        let prover = LocalTxProver::new(spend_params, output_params);

        create_spend_to_address(
            &db_data,
            &NETWORK,
            prover,
            AccountId(account),
            &extsk,
            &to,
            value,
            Some(memo),
            OvkPolicy::Sender,
        )
        .map_err(|e| format_err!("Error while sending funds: {}", e))
    });
    unwrap_exc_or(res, -1)
}

#[no_mangle]
pub extern "C" fn zcashlc_branch_id_for_height(height: i32) -> i32 {
    let res = catch_panic(|| {
        let branch: BranchId = BranchId::for_height(&NETWORK, BlockHeight::from(height as u32));
        let branch_id: u32 = u32::from(branch);
        Ok(branch_id as i32)
    });
    unwrap_exc_or(res, -1)
}

/// Frees strings returned by other zcashlc functions.
#[no_mangle]
pub extern "C" fn zcashlc_string_free(s: *mut c_char) {
    unsafe {
        if s.is_null() {
            return;
        }
        CString::from_raw(s)
    };
}

/// Frees vectors of strings returned by other zcashlc functions.
#[no_mangle]
pub extern "C" fn zcashlc_vec_string_free(v: *mut *mut c_char, len: usize, capacity: usize) {
    unsafe {
        if v.is_null() {
            return;
        }
        assert!(len <= capacity);
        let v = Vec::from_raw_parts(v, len, capacity);
        v.into_iter().map(|s| CString::from_raw(s)).for_each(drop);
    };
}


/// TEST TEST 123 TEST 
/// 
/// 

/// Derives a transparent private key from seed
#[no_mangle]
pub unsafe extern "C" fn zcashlc_derive_transparent_private_key_from_seed(
    seed: *const u8,
    seed_len: usize,
) -> *mut c_char {

    let res = catch_panic(|| {
        let seed = slice::from_raw_parts(seed, seed_len);
        
        // modified from: https://github.com/adityapk00/zecwallet-light-cli/blob/master/lib/src/lightwallet.rs

        let ext_t_key = ExtendedPrivKey::with_seed(&seed).unwrap();
        let address_sk = ext_t_key
            .derive_private_key(KeyIndex::hardened_from_normalize_index(44).unwrap())
            .unwrap()
            .derive_private_key(
                KeyIndex::hardened_from_normalize_index(NETWORK.coin_type()).unwrap(),
            )
            .unwrap()
            .derive_private_key(KeyIndex::hardened_from_normalize_index(0).unwrap())
            .unwrap()
            .derive_private_key(KeyIndex::Normal(0))
            .unwrap()
            .derive_private_key(KeyIndex::Normal(0))
            .unwrap()
            .private_key;
        

        Ok(CString::new(address_sk.to_string()).unwrap().into_raw())
    });
    unwrap_exc_or_null(res)
}

/// Derives a transparent address from the given seed 
#[no_mangle]
pub unsafe extern "C" fn zcashlc_derive_transparent_address_from_seed(
    seed: *const u8,
    seed_len: usize,
) -> *mut c_char {

    let res = catch_panic(|| {
        let seed = slice::from_raw_parts(seed, seed_len);
        
        // modified from: https://github.com/adityapk00/zecwallet-light-cli/blob/master/lib/src/lightwallet.rs

        let ext_t_key = ExtendedPrivKey::with_seed(&seed).unwrap();
        let address_sk = ext_t_key
            .derive_private_key(KeyIndex::hardened_from_normalize_index(44).unwrap())
            .unwrap()
            .derive_private_key(
                KeyIndex::hardened_from_normalize_index(NETWORK.coin_type()).unwrap(),
            )
            .unwrap()
            .derive_private_key(KeyIndex::hardened_from_normalize_index(0).unwrap())
            .unwrap()
            .derive_private_key(KeyIndex::Normal(0))
            .unwrap()
            .derive_private_key(KeyIndex::Normal(0))
            .unwrap()
            .private_key;
        
        let private_key = match secp256k1::SecretKey::from_slice(&address_sk[..]) {
            Ok(pk) => pk,
            Err(e) => {
                return Err(format_err!("error converting secret key {}",e));
            },
        };
        let secp = Secp256k1::new();
        let pk = PublicKey::from_secret_key(&secp, &private_key);
        let mut hash160 = ripemd160::Ripemd160::new();
        hash160.update(Sha256::digest(&pk.serialize()[..].to_vec()));
        let address_string = hash160
            .finalize()
            .to_base58check(&NETWORK.b58_pubkey_address_prefix(), &[]);

        Ok(CString::new(address_string).unwrap().into_raw())
    });
    unwrap_exc_or_null(res)
}

/// Derives a transparent address from the given seed 
#[no_mangle]
pub unsafe extern "C" fn zcashlc_derive_transparent_address_from_secret_key(
    tsk: *const c_char,
) -> *mut c_char {
    let res = catch_panic(|| {

        let tsk = CStr::from_ptr(tsk).to_str()?;

         // grab secret private key for t-funds
         let sk = match secp256k1::key::SecretKey::from_str(&tsk) {
            Ok(sk) => sk,
            Err(e) => {
                return Err(format_err!("Invalid Transparent Secret key: {}", e));
            },
        };

        // derive the corresponding t-address
        
        Ok(CString::new(derive_transparent_address_from_secret_key(sk)).unwrap().into_raw())
    });
    unwrap_exc_or_null(res)
}

fn derive_transparent_address_from_secret_key(secret_key: SecretKey) -> String {
    let secp = Secp256k1::new();
    let pk = PublicKey::from_secret_key(&secp, &secret_key);
    let mut hash160 = ripemd160::Ripemd160::new();
    hash160.update(Sha256::digest(&pk.serialize()[..].to_vec()));
    hash160
        .finalize()
        .to_base58check(&NETWORK.b58_pubkey_address_prefix(), &[])
}
  
//
// Helper code from: https://github.com/adityapk00/zecwallet-light-cli/blob/master/lib/src/lightwallet.rs
//

/// A trait for converting a [u8] to base58 encoded string.
pub trait ToBase58Check {
    /// Converts a value of `self` to a base58 value, returning the owned string.
    /// The version is a coin-specific prefix that is added.
    /// The suffix is any bytes that we want to add at the end (like the "iscompressed" flag for
    /// Secret key encoding)
    fn to_base58check(&self, version: &[u8], suffix: &[u8]) -> String;
}
impl ToBase58Check for [u8] {
    fn to_base58check(&self, version: &[u8], suffix: &[u8]) -> String {
        let mut payload: Vec<u8> = Vec::new();
        payload.extend_from_slice(version);
        payload.extend_from_slice(self);
        payload.extend_from_slice(suffix);

        let checksum = double_sha256(&payload);
        payload.append(&mut checksum[..4].to_vec());
        payload.to_base58()
    }
}
pub fn double_sha256(payload: &[u8]) -> Vec<u8> {
    let h1 = Sha256::digest(&payload);
    let h2 = Sha256::digest(&h1);
    h2.to_vec()
}



//// Extremely Experimental: I'm not even a rust developer
/// 
/// psst, hey I have a Major in Social Sciences. Consider using something else

/// Creates a transaction paying the specified address from the given account.
///
/// Returns the row index of the newly-created transaction in the `transactions` table
/// within the data database. The caller can read the raw transaction bytes from the `raw`
/// column in order to broadcast the transaction to the network.
///
/// Do not call this multiple times in parallel, or you will generate transactions that
/// double-spend the same notes.
/// 
/// 

fn shield_funds<P: consensus::Parameters>(
    db_cache: &BlockDB,
    db_data: &WalletDB<P>,
    account: u32,
    tsk: &str,
    extsk: &str,
    memo: &str,
    spend_params: &Path,
    output_params: &Path, 
) -> Result<i64,failure::Error> {
    let anchor_and_height = match (&db_data).get_target_and_anchor_heights() {
        Ok(Some(h)) => h,
        Ok(None) => {
            return Err(format_err!("No anchor and target heights found"));
        },
        Err(e) => {
            return Err(format_err!("Error fetching anchor and target heights: {}", e));
        },
    };

    // grab secret private key for t-funds
    let sk = match secp256k1::key::SecretKey::from_str(&tsk) {
        Ok(sk) => sk,
        Err(e) => {
            return Err(format_err!("Invalid Transparent Secret key: {}", e));
        },
    };

    // derive the corresponding t-address
    let t_addr_str = derive_transparent_address_from_secret_key(sk);

    let t_addr = match RecipientAddress::decode(&NETWORK, &t_addr_str) {
        Some(to) => to,
        None => {
            return Err(format_err!("PaymentAddress is for the wrong network"));
        },
    };

    let extsk =
        match decode_extended_spending_key(NETWORK.hrp_sapling_extended_spending_key(), &extsk)
        {
            Ok(Some(extsk)) => extsk,
            Ok(None) => {
                return Err(format_err!("ExtendedSpendingKey is for the wrong network"));
            },
            Err(e) => {
                return Err(format_err!("Invalid ExtendedSpendingKey: {}", e));
            },
        };

    // derive own shielded address from the provided extended spending key
    let z_address = extsk.default_address().unwrap().1;

    let exfvk = ExtendedFullViewingKey::from(&extsk);

    let ovk = exfvk.fvk.ovk;

    let memo = match Memo::from_str(&memo) {
        Ok(memo) => memo,
        Err(_) => {
            return Err(format_err!("Invalid memo input"));
        }
    };

    // get latest height and anchor

    let latest_scanned_height = anchor_and_height.1;
    let latest_anchor = anchor_and_height.0;

    // get UTXOs from DB
    let utxos = match get_confirmed_utxos_for_address(&NETWORK, &db_cache, latest_anchor, &t_addr_str) {
        Ok(u) => u,
        Err(e) => {
            return Err(format_err!("Error getting UTXOs {}",e));
        },
    };

    let total_amount = match Amount::from_i64(utxos.iter().map(|u| i64::from(u.value)).sum::<i64>()) {
        Ok(a) => a,
        _ => {
            return Err(format_err!("error collecting total amount from UTXOs"));
        },
    };
    let fee = DEFAULT_FEE;
    let target_value = fee + total_amount;
    if fee >= total_amount {
        return Err(format_err!("Insufficient verified funds (have {}, need {:?}). NOTE: funds need {} confirmations before they can be spent.",
        u64::from(total_amount), target_value, anchor_and_height.0 + 1));
    }
    let amount_to_shield = total_amount - fee;

    let prover = LocalTxProver::new(spend_params, output_params);

    let mut builder = Builder::new(NETWORK, latest_scanned_height);

    for utxo in utxos.iter() {
        let outpoint = OutPoint::new(
            utxo.txid.0,
            utxo.index as u32
        );
    
        let coin = TxOut {
            value: utxo.value.clone(),
            script_pubkey: Script { 0: utxo.script.clone() },
        };

        match builder.add_transparent_input(sk.clone(), outpoint.clone(), coin.clone()) {
            Ok(_) => (),
            Err(e) => {
                return Err(format_err!("error adding transparent input {}",e));
            },
        }
    }

    // there are no sapling notes so we set the change manually
    builder.send_change_to(ovk, z_address.clone());
    
    // add the sapling output to shield the funds
    match builder.add_sapling_output(Some(ovk), z_address.clone(), amount_to_shield, Some(memo.clone())) {
        Ok(_) =>(),
        Err(e) => {
            return Err(format_err!("Failed to add sapling output {}", e));
        }
    };
    let consensus_branch_id = BranchId::for_height(&NETWORK, anchor_and_height.1);

   
    let (tx, tx_metadata) = match builder
        .build(consensus_branch_id, &prover) {
            Ok(t) => t,
            Err(e) => {
                return Err(format_err!("Error building transaction {}",e));
            },
        };
        

    // We only called add_sapling_output() once.
    let output_index = match tx_metadata.output_index(0) {
        Some(idx) => idx as i64,
        None => { 
            return Err(format_err!("Output 0 should exist in the transaction"));
        },
    };

    // Update the database atomically, to ensure the result is internally consistent.
    let mut db_update = match (&db_data).get_update_ops()  {
        Ok(d) => d,
        Err(e) => {
            return Err(format_err!("error updating database with created tx {}",e));
        },
    };
    db_update
        .transactionally(|up| {
            let created = time::OffsetDateTime::now_utc();
            let tx_ref = up.put_tx_data(&tx, Some(created))?;

            // Mark notes as spent.
            //
            // This locks the notes so they aren't selected again by a subsequent call to
            // create_spend_to_address() before this transaction has been mined (at which point the notes
            // get re-marked as spent).
            //
            // Assumes that create_spend_to_address() will never be called in parallel, which is a
            // reasonable assumption for a light client such as a mobile phone.
            for spend in &tx.shielded_spends {
                up.mark_spent(tx_ref, &spend.nullifier)?;
            }

            up.insert_sent_note(
                tx_ref,
                output_index as usize,
                AccountId(account),
                &RecipientAddress::from(z_address),
                amount_to_shield,
                Some(memo),
            )?;

            // Return the row number of the transaction, so the caller can fetch it for sending.
            Ok(tx_ref)
        })
        .map_err(|e| format_err!("error building updating data_db {}",e))
}
#[no_mangle]
pub extern "C" fn zcashlc_shield_funds(
    db_data: *const u8,
    db_data_len: usize,
    db_cache: *const u8,
    db_cache_len: usize,
    account: i32,
    tsk: *const c_char,
    extsk: *const c_char,
    memo: *const c_char,
    spend_params: *const u8,
    spend_params_len: usize,
    output_params: *const u8,
    output_params_len: usize,
) -> i64 {
    let res = catch_panic(|| {

        let db_data = wallet_db(&NETWORK, db_data, db_data_len)?;
        let db_cache = block_db(db_cache, db_cache_len)?;
        let account = if account >= 0 {
            account as u32
        } else {
            return Err(format_err!("account argument must be positive"));
        };
        let tsk = unsafe { CStr::from_ptr(tsk) }.to_str()?;
        let extsk = unsafe { CStr::from_ptr(extsk) }.to_str()?;
        let memo = unsafe { CStr::from_ptr(memo) }.to_str()?;
        let spend_params = Path::new(OsStr::from_bytes(unsafe {
            slice::from_raw_parts(spend_params, spend_params_len)
        }));
        let output_params = Path::new(OsStr::from_bytes(unsafe {
            slice::from_raw_parts(output_params, output_params_len)
        }));

       shield_funds(&db_cache, &db_data, account, &tsk, &extsk, &memo, &spend_params, &output_params)
    });
    unwrap_exc_or(res, -1)
}
