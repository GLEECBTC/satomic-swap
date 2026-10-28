use anchor_lang::prelude::*;

/// Emitted when funds are locked. Indexers and the Komodo DeFi Framework use these
/// to find payments and their outcomes, since the payment account is closed once it is settled.
#[event]
pub struct PaymentSent {
    pub payment: Pubkey,
    pub sender: Pubkey,
    pub receiver: Pubkey,
    /// `None` for native SOL payments.
    pub mint: Option<Pubkey>,
    pub amount: u64,
    pub lock_time: u64,
    pub secret_hash: [u8; 32],
}

/// Emitted when the receiver claims a payment by revealing the secret.
#[event]
pub struct PaymentSpent {
    pub payment: Pubkey,
    pub receiver: Pubkey,
    pub amount: u64,
    /// The revealed preimage. The counterparty needs this to claim the other side of the swap.
    pub secret: [u8; 32],
}

/// Emitted when the sender reclaims a payment after the lock time expired.
#[event]
pub struct PaymentRefunded {
    pub payment: Pubkey,
    pub sender: Pubkey,
    pub amount: u64,
}
