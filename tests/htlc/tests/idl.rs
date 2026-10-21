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

const IDL: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../idl/satomic_swap.json"));

fn idl() -> Value { serde_json::from_str(IDL).expect("the checked-in IDL must be valid JSON") }

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
        ("spl_payment", satomic_swap::instruction::SplPayment::DISCRIMINATOR),
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

/// The framework's watcher classifies a payment's fate by the event it emitted, so these are just
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
        (SwapError::InsufficientVaultBalance, "InsufficientVaultBalance"),
        (SwapError::LamportOverflow, "LamportOverflow"),
        (SwapError::TransferFeeNotSupported, "TransferFeeNotSupported"),
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
