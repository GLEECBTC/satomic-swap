use anchor_lang::prelude::*;

/// The on-chain record of a locked payment.
///
/// Everything that identifies a payment — sender, receiver, mint, secret hash, lock time and
/// amount — is in the PDA seeds, so the address itself is the commitment and none of it has to be
/// stored or re-validated here. The account exists to (a) prove the payment was made and (b) hold
/// the funds, and it is closed when the payment is spent or refunded so the rent goes back.
#[account]
#[derive(InitSpace)]
pub struct Payment {
    /// Canonical bump of this PDA, stored so spend/refund don't have to re-derive it.
    pub bump: u8,
}
