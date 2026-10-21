//! Helpers shared by the lamports and SPL test binaries.

use anchor_lang::{AnchorDeserialize, Discriminator};
use base64::{engine::general_purpose::STANDARD, Engine};

/// A rejected transaction, keeping the rendered error so a test can assert *why* it failed rather
/// than only that it did.
#[derive(Debug, PartialEq)]
pub struct TxFailure(pub String);

impl TxFailure {
    /// The program's own error code, or 0 when the transaction never reached the program.
    pub fn code(&self) -> u32 { custom_code(&self.0) }

    /// Whether the runtime rejected it because a required account did not sign.
    ///
    /// This is the shape an impersonation attempt takes: the attacker names the real counterparty
    /// so the PDA still resolves, but cannot produce that party's signature.
    pub fn missing_signature(&self) -> bool {
        self.0.contains("MissingRequiredSignature") || self.0.contains("SignatureFailure")
    }
}

/// Pulls the `Custom(n)` code out of a formatted transaction error, or 0 if it isn't one.
pub fn custom_code(rendered: &str) -> u32 {
    rendered
        .split("Custom(")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

/// Decodes every event of type `E` that a transaction emitted.
///
/// Anchor's `emit!` logs the event as `Program data: <base64>`, where the payload is the event's
/// 8-byte discriminator followed by its Borsh encoding. Matching on the discriminator is what makes
/// this specific to `E` rather than to whatever the program happened to log first.
pub fn decode_events<E: AnchorDeserialize + Discriminator>(logs: &[String]) -> Vec<E> {
    logs.iter()
        .filter_map(|line| line.strip_prefix("Program data: "))
        .filter_map(|payload| STANDARD.decode(payload).ok())
        .filter_map(|bytes| {
            let (discriminator, mut body) = bytes.split_at_checked(E::DISCRIMINATOR.len())?;
            if discriminator != E::DISCRIMINATOR {
                return None;
            }
            E::deserialize(&mut body).ok()
        })
        .collect()
}

/// The single event of type `E` the transaction emitted.
///
/// Panics if there is none, or more than one: a swap instruction that emitted its event twice would
/// be as wrong as one that emitted none, and the framework's watcher reads these to find payments.
pub fn expect_event<E: AnchorDeserialize + Discriminator>(logs: &[String]) -> E {
    let mut events = decode_events::<E>(logs);
    assert_eq!(
        events.len(),
        1,
        "expected exactly one {} event, found {}",
        std::any::type_name::<E>(),
        events.len()
    );

    events.remove(0)
}
