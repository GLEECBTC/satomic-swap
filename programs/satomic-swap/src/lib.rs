//! HTLC atomic swap program for Solana.
//!
//! A payment locks funds in a PDA that only two parties can move:
//!   * the **receiver**, at any time, by revealing a secret that hashes to `secret_hash`;
//!   * the **sender**, but only once `lock_time` has passed.
//!
//! # Design notes
//!
//! Every parameter that identifies a payment — sender, receiver, mint, secret hash, lock time and
//! amount — is part of the PDA seeds, so *the address is the commitment*. Anchor re-derives it on
//! every instruction, which means a caller cannot substitute a different vault, amount or
//! counterparty: the account simply won't match. It also means nobody can squat a pending swap's
//! address without reproducing its exact parameters, which includes funding the full amount.
//!
//! The payment account holds the funds itself (for SOL) or owns the vault token account (for SPL),
//! and it is closed on spend or refund, so the rent returns to whoever paid it.
#![allow(unexpected_cfgs)]

use anchor_lang::prelude::*;
use anchor_spl::associated_token::AssociatedToken;
use anchor_spl::token_2022::spl_token_2022::extension::pausable::PausableConfig;
use anchor_spl::token_2022::spl_token_2022::extension::transfer_fee::TransferFeeConfig;
use anchor_spl::token_2022::spl_token_2022::extension::transfer_hook::TransferHook;
use anchor_spl::token_2022::spl_token_2022::extension::{
    BaseStateWithExtensions, ExtensionType, PodStateWithExtensions,
};
use anchor_spl::token_2022::spl_token_2022::pod::PodMint;
use anchor_spl::token_interface::{
    close_account, transfer_checked, CloseAccount, Mint, TokenAccount, TokenInterface,
    TransferChecked,
};
use solana_sha256_hasher::hash;

pub mod errors;
pub mod events;
pub mod state;

use errors::SwapError;
use events::{PaymentRefunded, PaymentSent, PaymentSpent};
use state::Payment;

declare_id!("54mDjQz6jS9sggCV1P5GoUzqHaBEueGVZqmaMPLt2tDx");

pub const PAYMENT_SEED: &[u8] = b"payment";
pub const PAYMENT_SPL_SEED: &[u8] = b"payment_spl";

#[program]
pub mod satomic_swap {
    use super::*;

    /// Locks `amount` lamports for `receiver`.
    pub fn lamports_payment(
        ctx: Context<LamportsPayment>,
        secret_hash: [u8; 32],
        lock_time: u64,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, SwapError::ZeroAmount);
        lock_time_as_timestamp(lock_time)?;
        require_keys_neq!(
            ctx.accounts.receiver.key(),
            Pubkey::default(),
            SwapError::InvalidReceiver
        );
        require_keys_neq!(
            ctx.accounts.receiver.key(),
            ctx.accounts.sender.key(),
            SwapError::InvalidReceiver
        );

        ctx.accounts.payment.bump = ctx.bumps.payment;

        // `init` funded the rent; this puts the swap amount into the same account.
        anchor_lang::system_program::transfer(
            CpiContext::new(
                ctx.accounts.system_program.key(),
                anchor_lang::system_program::Transfer {
                    from: ctx.accounts.sender.to_account_info(),
                    to: ctx.accounts.payment.to_account_info(),
                },
            ),
            amount,
        )?;

        emit!(PaymentSent {
            payment: ctx.accounts.payment.key(),
            sender: ctx.accounts.sender.key(),
            receiver: ctx.accounts.receiver.key(),
            mint: None,
            amount,
            lock_time,
            secret_hash,
        });
        Ok(())
    }

    /// Claims a lamports payment by revealing the secret. The rent goes back to the sender.
    #[allow(unused_variables)] // `lock_time` is consumed by the PDA seeds, not the body.
    pub fn receiver_spend_lamports(
        ctx: Context<ReceiverSpendLamports>,
        secret: [u8; 32],
        secret_hash: [u8; 32],
        lock_time: u64,
        amount: u64,
    ) -> Result<()> {
        require!(
            hash(&secret).to_bytes() == secret_hash,
            SwapError::InvalidSecret
        );

        let payment_ai = ctx.accounts.payment.to_account_info();
        let receiver_ai = ctx.accounts.receiver.to_account_info();

        let left = payment_ai
            .lamports()
            .checked_sub(amount)
            .ok_or(error!(SwapError::InsufficientVaultBalance))?;
        let received = receiver_ai
            .lamports()
            .checked_add(amount)
            .ok_or(error!(SwapError::LamportOverflow))?;
        **payment_ai.try_borrow_mut_lamports()? = left;
        **receiver_ai.try_borrow_mut_lamports()? = received;

        emit!(PaymentSpent {
            payment: ctx.accounts.payment.key(),
            receiver: ctx.accounts.receiver.key(),
            amount,
            secret,
        });
        Ok(())
    }

    /// Refunds a lamports payment once `lock_time` has passed. `close = sender` returns the amount
    /// together with the rent.
    #[allow(unused_variables)] // `secret_hash` is consumed by the PDA seeds, not the body.
    pub fn sender_refund_lamports(
        ctx: Context<SenderRefundLamports>,
        secret_hash: [u8; 32],
        lock_time: u64,
        amount: u64,
    ) -> Result<()> {
        require_lock_time_expired(lock_time)?;

        emit!(PaymentRefunded {
            payment: ctx.accounts.payment.key(),
            sender: ctx.accounts.sender.key(),
            amount,
        });
        Ok(())
    }

    /// Locks `amount` of an SPL (or Token-2022) mint for `receiver`.
    pub fn spl_payment(
        ctx: Context<SplPayment>,
        secret_hash: [u8; 32],
        lock_time: u64,
        amount: u64,
    ) -> Result<()> {
        require!(amount > 0, SwapError::ZeroAmount);
        lock_time_as_timestamp(lock_time)?;
        require_keys_neq!(
            ctx.accounts.receiver.key(),
            Pubkey::default(),
            SwapError::InvalidReceiver
        );
        require_keys_neq!(
            ctx.accounts.receiver.key(),
            ctx.accounts.sender.key(),
            SwapError::InvalidReceiver
        );

        ctx.accounts.payment.bump = ctx.bumps.payment;

        transfer_checked(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.sender_token_account.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.sender.to_account_info(),
                },
            ),
            amount,
            ctx.accounts.mint.decimals,
        )?;

        emit!(PaymentSent {
            payment: ctx.accounts.payment.key(),
            sender: ctx.accounts.sender.key(),
            receiver: ctx.accounts.receiver.key(),
            mint: Some(ctx.accounts.mint.key()),
            amount,
            lock_time,
            secret_hash,
        });
        Ok(())
    }

    /// Claims an SPL payment by revealing the secret. The receiver gets everything the vault holds,
    /// which is `amount` plus anything someone else deposited.
    pub fn receiver_spend_spl(
        ctx: Context<ReceiverSpendSpl>,
        secret: [u8; 32],
        secret_hash: [u8; 32],
        lock_time: u64,
        amount: u64,
    ) -> Result<()> {
        require!(
            hash(&secret).to_bytes() == secret_hash,
            SwapError::InvalidSecret
        );

        let sender_key = ctx.accounts.sender.key();
        let receiver_key = ctx.accounts.receiver.key();
        let mint_key = ctx.accounts.mint.key();
        let lock_time_bytes = lock_time.to_le_bytes();
        let amount_bytes = amount.to_le_bytes();
        let bump = [ctx.accounts.payment.bump];
        let signer_seeds: &[&[&[u8]]] = &[&[
            PAYMENT_SPL_SEED,
            sender_key.as_ref(),
            receiver_key.as_ref(),
            mint_key.as_ref(),
            secret_hash.as_ref(),
            &lock_time_bytes,
            &amount_bytes,
            &bump,
        ]];

        // The whole balance, not just `amount`: anyone can deposit into the vault, and a token
        // account that still holds tokens cannot be closed, so leaving any behind would make the
        // payment impossible to spend or refund.
        let balance = vault_balance(&ctx.accounts.vault, amount)?;
        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.receiver_token_account.to_account_info(),
                    authority: ctx.accounts.payment.to_account_info(),
                },
                signer_seeds,
            ),
            balance,
            ctx.accounts.mint.decimals,
        )?;

        // Returns the vault's rent to the sender, who paid it.
        close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.sender.to_account_info(),
                authority: ctx.accounts.payment.to_account_info(),
            },
            signer_seeds,
        ))?;

        emit!(PaymentSpent {
            payment: ctx.accounts.payment.key(),
            receiver: ctx.accounts.receiver.key(),
            amount,
            secret,
        });
        Ok(())
    }

    /// Refunds an SPL payment once `lock_time` has passed. The sender gets everything the vault
    /// holds, which is `amount` plus anything someone else deposited.
    pub fn sender_refund_spl(
        ctx: Context<SenderRefundSpl>,
        secret_hash: [u8; 32],
        lock_time: u64,
        amount: u64,
    ) -> Result<()> {
        require_lock_time_expired(lock_time)?;

        let sender_key = ctx.accounts.sender.key();
        let receiver_key = ctx.accounts.receiver.key();
        let mint_key = ctx.accounts.mint.key();
        let lock_time_bytes = lock_time.to_le_bytes();
        let amount_bytes = amount.to_le_bytes();
        let bump = [ctx.accounts.payment.bump];
        let signer_seeds: &[&[&[u8]]] = &[&[
            PAYMENT_SPL_SEED,
            sender_key.as_ref(),
            receiver_key.as_ref(),
            mint_key.as_ref(),
            secret_hash.as_ref(),
            &lock_time_bytes,
            &amount_bytes,
            &bump,
        ]];

        // The whole balance, for the same reason as in `receiver_spend_spl`.
        let balance = vault_balance(&ctx.accounts.vault, amount)?;
        transfer_checked(
            CpiContext::new_with_signer(
                ctx.accounts.token_program.key(),
                TransferChecked {
                    from: ctx.accounts.vault.to_account_info(),
                    mint: ctx.accounts.mint.to_account_info(),
                    to: ctx.accounts.sender_token_account.to_account_info(),
                    authority: ctx.accounts.payment.to_account_info(),
                },
                signer_seeds,
            ),
            balance,
            ctx.accounts.mint.decimals,
        )?;

        close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.sender.to_account_info(),
                authority: ctx.accounts.payment.to_account_info(),
            },
            signer_seeds,
        ))?;

        emit!(PaymentRefunded {
            payment: ctx.accounts.payment.key(),
            sender: ctx.accounts.sender.key(),
            amount,
        });
        Ok(())
    }
}

/// The refund path is the only thing the lock time gates: the receiver may spend with the secret at
/// any point, exactly like the UTXO HTLC scripts the framework already uses.
fn require_lock_time_expired(lock_time: u64) -> Result<()> {
    let lock_time = lock_time_as_timestamp(lock_time)?;
    let now = Clock::get()?.unix_timestamp;
    require!(now >= lock_time, SwapError::LockTimeNotExpired);
    Ok(())
}

/// The lock time as the `i64` that `Clock::unix_timestamp` is compared against.
///
/// Checked when the payment is made, not only on refund: a lock time that does not fit could never
/// expire, so a payment that carried one could never be refunded.
fn lock_time_as_timestamp(lock_time: u64) -> Result<i64> {
    i64::try_from(lock_time).map_err(|_| error!(SwapError::InvalidLockTime))
}

/// What the vault holds, which is at least `amount` and can be more.
///
/// A token account accepts deposits from anyone, so the surplus is whatever someone sent it, before
/// or after the payment. The vault cannot be closed while it holds any, so a spend or refund has to
/// move all of it.
fn vault_balance(vault: &InterfaceAccount<TokenAccount>, amount: u64) -> Result<u64> {
    require_gte!(vault.amount, amount, SwapError::InsufficientVaultBalance);
    Ok(vault.amount)
}

/// Runs `check` over a mint's Token-2022 extensions.
///
/// A classic SPL Token mint cannot carry extensions, so it passes. A Token-2022 mint whose data does
/// not parse fails, as does an account owned by anything else: when in doubt, refuse the mint.
fn check_mint_extensions(
    mint: &AccountInfo,
    check: impl FnOnce(&PodStateWithExtensions<PodMint>) -> bool,
) -> bool {
    if *mint.owner == anchor_spl::token::ID {
        return true;
    }
    if *mint.owner != anchor_spl::token_2022::ID {
        return false;
    }
    let Ok(data) = mint.try_borrow_data() else {
        return false;
    };
    let Ok(state) = PodStateWithExtensions::<PodMint>::unpack(&data) else {
        return false;
    };
    check(&state)
}

/// Whether this mint is safe to lock: it must not charge a transfer fee, nor be able to start.
///
/// With a fee, the vault receives less than `amount`, so either the receiver is short-changed or the
/// sender has to over-send — neither is something a swap can silently absorb. The fee is also
/// mutable: `TransferFeeConfig` carries an older and a newer fee plus an authority that can raise
/// it, so a mint that is fee-free today can charge one tomorrow.
///
/// Used as a constraint on the mint in [`SplPayment`], which runs before the vault is created, so a
/// rejected mint costs nothing. It is deliberately applied **only when the payment is made**:
/// re-checking on spend or refund would let a fee authority trap funds mid-swap.
pub fn mint_has_no_transfer_fee(mint: &AccountInfo) -> bool {
    check_mint_extensions(mint, |state| {
        let Ok(config) = state.get_extension::<TransferFeeConfig>() else {
            return true;
        };

        let newer = u16::from(config.newer_transfer_fee.transfer_fee_basis_points);
        let older = u16::from(config.older_transfer_fee.transfer_fee_basis_points);
        let mutable = Option::<Pubkey>::from(config.transfer_fee_config_authority).is_some();

        newer == 0 && older == 0 && !mutable
    })
}

/// Whether every Token-2022 extension on this mint leaves the vault to the swap alone: nobody but
/// the payment PDA may be able to move its tokens, or stop them moving.
///
/// This is an allowlist. An extension not named here, including any Token-2022 adds later, is
/// refused, because the failure modes are lopsided: refusing a harmless mint costs a swap that
/// never starts, while accepting a harmful one can cost the funds of a swap already under way.
///
/// * Never accepted: `PermanentDelegate`, which can move the vault's tokens at will.
/// * Accepted only if nobody can ever switch them on: `TransferHook` with no hook program and no
///   authority to set one — this program forwards no hook accounts, so a hook would make every
///   transfer out of the vault fail — and `Pausable`, unpaused and with no authority to pause it.
/// * `TransferFeeConfig` is left to [`mint_has_no_transfer_fee`], so it keeps its own error.
///
/// The mint's freeze authority is accepted, because the most widely held stablecoins have one. It
/// can freeze the vault, so a swap is only as safe as the issuer of the token it locks.
///
/// Like the transfer-fee check, this runs only when the payment is made. That is enough, because
/// extensions cannot be added to a mint once it exists, and an accepted configuration has no
/// authority left to change it.
pub fn mint_extensions_are_supported(mint: &AccountInfo) -> bool {
    check_mint_extensions(mint, |state| {
        let Ok(extensions) = state.get_extension_types() else {
            return false;
        };

        extensions.into_iter().all(|extension| match extension {
            // Descriptive, or affecting only how amounts are displayed.
            ExtensionType::MetadataPointer
            | ExtensionType::TokenMetadata
            | ExtensionType::GroupPointer
            | ExtensionType::TokenGroup
            | ExtensionType::GroupMemberPointer
            | ExtensionType::TokenGroupMember
            | ExtensionType::InterestBearingConfig
            | ExtensionType::ScaledUiAmount => true,
            // A mint can only be closed at zero supply, and the vault holds some of it.
            ExtensionType::MintCloseAuthority => true,
            // Sets the state of *new* accounts, which is no more than the freeze authority it
            // requires can already do.
            ExtensionType::DefaultAccountState => true,
            // Confidential balances need an account the owner has configured for them, and only
            // the payment PDA could configure the vault.
            ExtensionType::ConfidentialTransferMint => true,
            ExtensionType::TransferFeeConfig => true,
            ExtensionType::TransferHook => {
                state.get_extension::<TransferHook>().is_ok_and(|hook| {
                    Option::<Pubkey>::from(hook.authority).is_none()
                        && Option::<Pubkey>::from(hook.program_id).is_none()
                })
            }
            ExtensionType::Pausable => {
                state
                    .get_extension::<PausableConfig>()
                    .is_ok_and(|pausable| {
                        Option::<Pubkey>::from(pausable.authority).is_none()
                            && !bool::from(pausable.paused)
                    })
            }
            _ => false,
        })
    })
}

#[derive(Accounts)]
#[instruction(secret_hash: [u8; 32], lock_time: u64, amount: u64)]
pub struct LamportsPayment<'info> {
    #[account(mut)]
    pub sender: Signer<'info>,
    /// CHECK: only its key matters; it is bound into the PDA seeds and checked again on spend.
    pub receiver: UncheckedAccount<'info>,
    #[account(
        init,
        payer = sender,
        space = 8 + Payment::INIT_SPACE,
        seeds = [
            PAYMENT_SEED,
            sender.key().as_ref(),
            receiver.key().as_ref(),
            secret_hash.as_ref(),
            &lock_time.to_le_bytes(),
            &amount.to_le_bytes(),
        ],
        bump,
    )]
    pub payment: Account<'info, Payment>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(secret: [u8; 32], secret_hash: [u8; 32], lock_time: u64, amount: u64)]
pub struct ReceiverSpendLamports<'info> {
    #[account(mut)]
    pub receiver: Signer<'info>,
    /// CHECK: bound into the PDA seeds, and the rent is returned to it.
    #[account(mut)]
    pub sender: UncheckedAccount<'info>,
    #[account(
        mut,
        close = sender,
        seeds = [
            PAYMENT_SEED,
            sender.key().as_ref(),
            receiver.key().as_ref(),
            secret_hash.as_ref(),
            &lock_time.to_le_bytes(),
            &amount.to_le_bytes(),
        ],
        bump = payment.bump,
    )]
    pub payment: Account<'info, Payment>,
}

#[derive(Accounts)]
#[instruction(secret_hash: [u8; 32], lock_time: u64, amount: u64)]
pub struct SenderRefundLamports<'info> {
    #[account(mut)]
    pub sender: Signer<'info>,
    /// CHECK: only its key matters; bound into the PDA seeds.
    pub receiver: UncheckedAccount<'info>,
    #[account(
        mut,
        close = sender,
        seeds = [
            PAYMENT_SEED,
            sender.key().as_ref(),
            receiver.key().as_ref(),
            secret_hash.as_ref(),
            &lock_time.to_le_bytes(),
            &amount.to_le_bytes(),
        ],
        bump = payment.bump,
    )]
    pub payment: Account<'info, Payment>,
}

#[derive(Accounts)]
#[instruction(secret_hash: [u8; 32], lock_time: u64, amount: u64)]
pub struct SplPayment<'info> {
    #[account(mut)]
    pub sender: Signer<'info>,
    /// CHECK: only its key matters; bound into the PDA seeds.
    pub receiver: UncheckedAccount<'info>,
    #[account(
        constraint = mint_has_no_transfer_fee(&mint.to_account_info()) @ SwapError::TransferFeeNotSupported,
        constraint = mint_extensions_are_supported(&mint.to_account_info()) @ SwapError::MintExtensionNotSupported,
    )]
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = sender,
        token::token_program = token_program,
    )]
    pub sender_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        init,
        payer = sender,
        space = 8 + Payment::INIT_SPACE,
        seeds = [
            PAYMENT_SPL_SEED,
            sender.key().as_ref(),
            receiver.key().as_ref(),
            mint.key().as_ref(),
            secret_hash.as_ref(),
            &lock_time.to_le_bytes(),
            &amount.to_le_bytes(),
        ],
        bump,
    )]
    pub payment: Account<'info, Payment>,
    /// `init_if_needed`, because anyone may create an associated token account for any owner and
    /// this address follows from the payment's parameters, so with `init` creating it first would
    /// block the payment. Whatever an early creator deposits is swept out with the payment.
    #[account(
        init_if_needed,
        payer = sender,
        associated_token::mint = mint,
        associated_token::authority = payment,
        associated_token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
    pub associated_token_program: Program<'info, AssociatedToken>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(secret: [u8; 32], secret_hash: [u8; 32], lock_time: u64, amount: u64)]
pub struct ReceiverSpendSpl<'info> {
    #[account(mut)]
    pub receiver: Signer<'info>,
    /// CHECK: bound into the PDA seeds; receives the rent of both closed accounts.
    #[account(mut)]
    pub sender: UncheckedAccount<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = receiver,
        token::token_program = token_program,
    )]
    pub receiver_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        close = sender,
        seeds = [
            PAYMENT_SPL_SEED,
            sender.key().as_ref(),
            receiver.key().as_ref(),
            mint.key().as_ref(),
            secret_hash.as_ref(),
            &lock_time.to_le_bytes(),
            &amount.to_le_bytes(),
        ],
        bump = payment.bump,
    )]
    pub payment: Account<'info, Payment>,
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = payment,
        associated_token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}

#[derive(Accounts)]
#[instruction(secret_hash: [u8; 32], lock_time: u64, amount: u64)]
pub struct SenderRefundSpl<'info> {
    #[account(mut)]
    pub sender: Signer<'info>,
    /// CHECK: only its key matters; bound into the PDA seeds.
    pub receiver: UncheckedAccount<'info>,
    pub mint: InterfaceAccount<'info, Mint>,
    #[account(
        mut,
        token::mint = mint,
        token::authority = sender,
        token::token_program = token_program,
    )]
    pub sender_token_account: InterfaceAccount<'info, TokenAccount>,
    #[account(
        mut,
        close = sender,
        seeds = [
            PAYMENT_SPL_SEED,
            sender.key().as_ref(),
            receiver.key().as_ref(),
            mint.key().as_ref(),
            secret_hash.as_ref(),
            &lock_time.to_le_bytes(),
            &amount.to_le_bytes(),
        ],
        bump = payment.bump,
    )]
    pub payment: Account<'info, Payment>,
    #[account(
        mut,
        associated_token::mint = mint,
        associated_token::authority = payment,
        associated_token::token_program = token_program,
    )]
    pub vault: InterfaceAccount<'info, TokenAccount>,
    pub token_program: Interface<'info, TokenInterface>,
}
