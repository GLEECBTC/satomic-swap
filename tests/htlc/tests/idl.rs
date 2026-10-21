//! Guards the checked-in IDL against the program it claims to describe.
//!
//! KDF does not depend on this crate - it pins incompatible `solana-*` versions - so it hard-codes
//! the discriminators and builds the instructions by hand. The checked-in IDL is the contract
//! between the two, and these tests are what stop it drifting: renaming an instruction or an event
//! changes its discriminator silently, and the first symptom would be a swap that locks funds and
//! then cannot spend them.

use anchor_lang::Discriminator;
use satomic_swap::errors::SwapError;
use satomic_swap::events::{PaymentRefunded, PaymentSent, PaymentSpent};
use serde_json::Value;

const IDL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../idl/satomic_swap.json"
));

fn idl() -> Value {
    serde_json::from_str(IDL).expect("the checked-in IDL must be valid JSON")
}

/// The discriminator the IDL lists for a named entry under `key`.
fn discriminator(idl: &Value, key: &str, name: &str) -> Vec<u8> {
    let entry = idl[key]
        .as_array()
        .unwrap_or_else(|| panic!("IDL has no {key} array"))
        .iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("IDL has no {key} entry named {name}"));

    entry["discriminator"]
        .as_array()
        .unwrap_or_else(|| panic!("{name} has no discriminator"))
        .iter()
        .map(|byte| byte.as_u64().expect("discriminator byte") as u8)
        .collect()
}

#[test]
fn the_idl_describes_this_program() {
    assert_eq!(
        idl()["address"].as_str().expect("IDL address"),
        satomic_swap::ID.to_string(),
        "the IDL belongs to a different program id than the one this crate declares"
    );
}

/// Every instruction KDF encodes by hand has to match what the program compiled to.
#[test]
fn instruction_discriminators_match_the_program() {
    let idl = idl();

    for (name, expected) in [
        (
            "lamports_payment",
            satomic_swap::instruction::LamportsPayment::DISCRIMINATOR,
        ),
        (
            "receiver_spend_lamports",
            satomic_swap::instruction::ReceiverSpendLamports::DISCRIMINATOR,
        ),
        (
            "sender_refund_lamports",
            satomic_swap::instruction::SenderRefundLamports::DISCRIMINATOR,
        ),
        (
            "spl_payment",
            satomic_swap::instruction::SplPayment::DISCRIMINATOR,
        ),
        (
            "receiver_spend_spl",
            satomic_swap::instruction::ReceiverSpendSpl::DISCRIMINATOR,
        ),
        (
            "sender_refund_spl",
            satomic_swap::instruction::SenderRefundSpl::DISCRIMINATOR,
        ),
    ] {
        assert_eq!(
            discriminator(&idl, "instructions", name),
            expected,
            "{name}'s discriminator changed; regenerate the IDL and update KDF's copy"
        );
    }
}

/// The framework classifies a payment's fate by the event it emitted, so these are just
/// as load-bearing as the instruction discriminators.
#[test]
fn event_discriminators_match_the_program() {
    let idl = idl();

    for (name, expected) in [
        ("PaymentSent", PaymentSent::DISCRIMINATOR),
        ("PaymentSpent", PaymentSpent::DISCRIMINATOR),
        ("PaymentRefunded", PaymentRefunded::DISCRIMINATOR),
    ] {
        assert_eq!(
            discriminator(&idl, "events", name),
            expected,
            "{name}'s discriminator changed; regenerate the IDL and update KDF's copy"
        );
    }
}

/// KDF maps these codes to user-facing errors, and Anchor assigns them by declaration order - so
/// inserting a variant renumbers every one after it.
#[test]
fn error_codes_match_the_program() {
    let idl = idl();
    let errors = idl["errors"].as_array().expect("IDL errors");

    for (code, name) in [
        (SwapError::ZeroAmount, "ZeroAmount"),
        (SwapError::InvalidReceiver, "InvalidReceiver"),
        (SwapError::InvalidSecret, "InvalidSecret"),
        (SwapError::LockTimeNotExpired, "LockTimeNotExpired"),
        (SwapError::InvalidLockTime, "InvalidLockTime"),
        (
            SwapError::InsufficientVaultBalance,
            "InsufficientVaultBalance",
        ),
        (SwapError::LamportOverflow, "LamportOverflow"),
        (
            SwapError::TransferFeeNotSupported,
            "TransferFeeNotSupported",
        ),
    ] {
        let expected = code as u32 + anchor_lang::error::ERROR_CODE_OFFSET;
        let entry = errors
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("IDL has no error named {name}"));

        assert_eq!(
            entry["code"].as_u64().expect("error code") as u32,
            expected,
            "{name} is numbered {expected} by the program but differently in the IDL"
        );
    }

    assert_eq!(
        errors.len(),
        8,
        "an error was added or removed; check nothing after it was renumbered"
    );
}

/// KDF hand-builds each instruction's account list, so the order and the signer and writable flags
/// are as much a part of the contract as the discriminator - and they are where a hand-rolled
/// encoder actually goes wrong. A discriminator check alone would pass a reordered or re-flagged
/// account list and leave KDF building instructions the program rejects.
///
/// Each field gets a distinct key, so a meta's position identifies which field produced it.
#[test]
fn account_order_and_flags_match_the_program() {
    use anchor_lang::ToAccountMetas;
    use solana_pubkey::Pubkey;
    use std::collections::HashMap;

    let idl = idl();

    fn keys(names: &[&'static str]) -> HashMap<&'static str, Pubkey> {
        names
            .iter()
            .enumerate()
            .map(|(i, name)| (*name, Pubkey::new_from_array([i as u8 + 1; 32])))
            .collect()
    }

    let check = |instruction: &str,
                 keys: &HashMap<&'static str, Pubkey>,
                 metas: Vec<anchor_lang::prelude::AccountMeta>| {
        let accounts = idl["instructions"]
            .as_array()
            .expect("IDL instructions")
            .iter()
            .find(|ix| ix["name"] == instruction)
            .unwrap_or_else(|| panic!("IDL has no instruction {instruction}"))["accounts"]
            .as_array()
            .unwrap_or_else(|| panic!("{instruction} has no accounts"));

        assert_eq!(
            accounts.len(),
            metas.len(),
            "{instruction} takes a different number of accounts"
        );

        for (position, (account, meta)) in accounts.iter().zip(&metas).enumerate() {
            let name = account["name"].as_str().expect("account name");
            assert_eq!(
                Some(&meta.pubkey),
                keys.get(name),
                "{instruction}: position {position} is {name} in the IDL but a different account in the program"
            );
            assert_eq!(
                meta.is_signer,
                account["signer"].as_bool().unwrap_or(false),
                "{instruction}.{name}: signer flag differs"
            );
            assert_eq!(
                meta.is_writable,
                account["writable"].as_bool().unwrap_or(false),
                "{instruction}.{name}: writable flag differs"
            );
        }
    };

    let k = keys(&["sender", "receiver", "payment", "system_program"]);
    check(
        "lamports_payment",
        &k,
        satomic_swap::accounts::LamportsPayment {
            sender: k["sender"],
            receiver: k["receiver"],
            payment: k["payment"],
            system_program: k["system_program"],
        }
        .to_account_metas(None),
    );

    let k = keys(&["receiver", "sender", "payment"]);
    check(
        "receiver_spend_lamports",
        &k,
        satomic_swap::accounts::ReceiverSpendLamports {
            receiver: k["receiver"],
            sender: k["sender"],
            payment: k["payment"],
        }
        .to_account_metas(None),
    );

    let k = keys(&["sender", "receiver", "payment"]);
    check(
        "sender_refund_lamports",
        &k,
        satomic_swap::accounts::SenderRefundLamports {
            sender: k["sender"],
            receiver: k["receiver"],
            payment: k["payment"],
        }
        .to_account_metas(None),
    );

    let k = keys(&[
        "sender",
        "receiver",
        "mint",
        "sender_token_account",
        "payment",
        "vault",
        "token_program",
        "associated_token_program",
        "system_program",
    ]);
    check(
        "spl_payment",
        &k,
        satomic_swap::accounts::SplPayment {
            sender: k["sender"],
            receiver: k["receiver"],
            mint: k["mint"],
            sender_token_account: k["sender_token_account"],
            payment: k["payment"],
            vault: k["vault"],
            token_program: k["token_program"],
            associated_token_program: k["associated_token_program"],
            system_program: k["system_program"],
        }
        .to_account_metas(None),
    );

    let k = keys(&[
        "receiver",
        "sender",
        "mint",
        "receiver_token_account",
        "payment",
        "vault",
        "token_program",
    ]);
    check(
        "receiver_spend_spl",
        &k,
        satomic_swap::accounts::ReceiverSpendSpl {
            receiver: k["receiver"],
            sender: k["sender"],
            mint: k["mint"],
            receiver_token_account: k["receiver_token_account"],
            payment: k["payment"],
            vault: k["vault"],
            token_program: k["token_program"],
        }
        .to_account_metas(None),
    );

    let k = keys(&[
        "sender",
        "receiver",
        "mint",
        "sender_token_account",
        "payment",
        "vault",
        "token_program",
    ]);
    check(
        "sender_refund_spl",
        &k,
        satomic_swap::accounts::SenderRefundSpl {
            sender: k["sender"],
            receiver: k["receiver"],
            mint: k["mint"],
            sender_token_account: k["sender_token_account"],
            payment: k["payment"],
            vault: k["vault"],
            token_program: k["token_program"],
        }
        .to_account_metas(None),
    );
}
