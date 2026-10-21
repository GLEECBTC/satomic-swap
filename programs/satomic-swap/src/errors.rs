use anchor_lang::prelude::*;

#[error_code]
pub enum SwapError {
    #[msg("Payment amount must be greater than zero")]
    ZeroAmount,
    #[msg("Receiver must not be the default pubkey or the sender")]
    InvalidReceiver,
    #[msg("The provided secret does not hash to the payment's secret hash")]
    InvalidSecret,
    #[msg("The lock time has not expired yet, the payment cannot be refunded")]
    LockTimeNotExpired,
    #[msg("Lock time does not fit in a unix timestamp")]
    InvalidLockTime,
    #[msg("The payment account holds fewer lamports than the locked amount")]
    InsufficientVaultBalance,
    #[msg("Lamport arithmetic overflowed")]
    LamportOverflow,
    #[msg("Mints with a transfer fee are not supported: the vault would receive less than the agreed amount")]
    TransferFeeNotSupported,
}
