//! HTLC tests for the native SOL path.
//!
//! These cover the cases the original program got wrong: refunding before the lock time, spending
//! with the wrong secret, and substituting parameters that the PDA is supposed to bind.

use anchor_lang::{InstructionData, ToAccountMetas};
use litesvm::LiteSVM;
use solana_clock::Clock;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;

const PROGRAM_ID: Pubkey = satomic_swap::ID;
const AMOUNT: u64 = 2_000_000_000;
const LOCK_TIME: u64 = 1_800_000_000;

// Anchor numbers custom errors from 6000 in declaration order.
const E_INVALID_SECRET: u32 = 6002;
const E_LOCK_TIME_NOT_EXPIRED: u32 = 6003;

struct Ctx {
    svm: LiteSVM,
    sender: Keypair,
    receiver: Keypair,
    secret: [u8; 32],
    secret_hash: [u8; 32],
}

fn setup() -> Ctx {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(PROGRAM_ID, "../../target/deploy/satomic_swap.so")
        .expect("build the program with `anchor build` first");

    let sender = Keypair::new();
    let receiver = Keypair::new();
    svm.airdrop(&sender.pubkey(), 10_000_000_000).unwrap();
    svm.airdrop(&receiver.pubkey(), 10_000_000_000).unwrap();

    let secret = [7u8; 32];
    let secret_hash = solana_sha256_hasher::hash(&secret).to_bytes();

    Ctx { svm, sender, receiver, secret, secret_hash }
}

fn payment_pda(sender: &Pubkey, receiver: &Pubkey, secret_hash: &[u8; 32], amount: u64) -> Pubkey {
    Pubkey::find_program_address(
        &[
            b"payment",
            sender.as_ref(),
            receiver.as_ref(),
            secret_hash,
            &LOCK_TIME.to_le_bytes(),
            &amount.to_le_bytes(),
        ],
        &PROGRAM_ID,
    )
    .0
}

impl Ctx {
    fn send(&mut self, ix: Instruction, signer: &Keypair) -> Result<(), u32> {
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&signer.pubkey()),
            &[signer],
            self.svm.latest_blockhash(),
        );
        match self.svm.send_transaction(tx) {
            Ok(_) => Ok(()),
            Err(failed) => Err(custom_code(&format!("{:?}", failed.err))),
        }
    }

    fn pay(&mut self) -> Pubkey {
        let payment = payment_pda(
            &self.sender.pubkey(),
            &self.receiver.pubkey(),
            &self.secret_hash,
            AMOUNT,
        );
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::LamportsPayment {
                sender: self.sender.pubkey(),
                receiver: self.receiver.pubkey(),
                payment,
                system_program: solana_system_interface::program::ID,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::LamportsPayment {
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount: AMOUNT,
            }
            .data(),
        };
        let sender = self.sender.insecure_clone();
        self.send(ix, &sender).expect("payment should succeed");
        payment
    }

    fn spend_ix(&self, payment: Pubkey, secret: [u8; 32], amount: u64) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::ReceiverSpendLamports {
                receiver: self.receiver.pubkey(),
                sender: self.sender.pubkey(),
                payment,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::ReceiverSpendLamports {
                secret,
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount,
            }
            .data(),
        }
    }

    fn refund_ix(&self, payment: Pubkey) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::SenderRefundLamports {
                sender: self.sender.pubkey(),
                receiver: self.receiver.pubkey(),
                payment,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::SenderRefundLamports {
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount: AMOUNT,
            }
            .data(),
        }
    }

    fn warp_past_lock_time(&mut self) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = LOCK_TIME as i64 + 1;
        self.svm.set_sysvar(&clock);
    }
}

/// Pulls the `Custom(n)` code out of a formatted transaction error, or 0 if it isn't one.
fn custom_code(rendered: &str) -> u32 {
    rendered
        .split("Custom(")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
}

#[test]
fn receiver_can_spend_with_the_secret_and_rent_returns_to_the_sender() {
    let mut ctx = setup();
    let sender_before = ctx.svm.get_balance(&ctx.sender.pubkey()).unwrap();
    let payment = ctx.pay();

    // The payment holds the amount plus its own rent.
    let locked = ctx.svm.get_balance(&payment).unwrap();
    assert!(locked > AMOUNT, "payment account should hold amount + rent");

    let receiver_before = ctx.svm.get_balance(&ctx.receiver.pubkey()).unwrap();
    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT);
    let receiver = ctx.receiver.insecure_clone();
    ctx.send(ix, &receiver).expect("spend should succeed");

    let receiver_after = ctx.svm.get_balance(&ctx.receiver.pubkey()).unwrap();
    // The receiver pays the transaction fee, so it nets the amount minus that fee.
    assert!(receiver_after > receiver_before, "receiver received the locked amount");
    assert!(receiver_after - receiver_before <= AMOUNT, "receiver cannot receive more than the amount");
    assert!(AMOUNT - (receiver_after - receiver_before) < 100_000, "only fees were deducted");

    // The payment account is gone and its rent went back to the sender, so the sender is only out
    // the swap amount plus fees.
    assert_eq!(ctx.svm.get_balance(&payment).unwrap_or(0), 0, "payment account should be closed");
    let sender_after = ctx.svm.get_balance(&ctx.sender.pubkey()).unwrap();
    assert!(sender_before - sender_after >= AMOUNT, "sender paid at least the amount");
    assert!(sender_before - sender_after < AMOUNT + 100_000, "sender got the rent back");
}

#[test]
fn refund_before_lock_time_is_rejected() {
    let mut ctx = setup();
    let payment = ctx.pay();

    let ix = ctx.refund_ix(payment);
    let sender = ctx.sender.insecure_clone();
    assert_eq!(
        ctx.send(ix, &sender),
        Err(E_LOCK_TIME_NOT_EXPIRED),
        "the original program allowed this, which broke atomicity"
    );
    assert!(ctx.svm.get_balance(&payment).unwrap() > AMOUNT, "funds stay locked");
}

#[test]
fn refund_after_lock_time_returns_everything_to_the_sender() {
    let mut ctx = setup();
    let sender_before = ctx.svm.get_balance(&ctx.sender.pubkey()).unwrap();
    let payment = ctx.pay();
    ctx.warp_past_lock_time();

    let ix = ctx.refund_ix(payment);
    let sender = ctx.sender.insecure_clone();
    ctx.send(ix, &sender).expect("refund should succeed after expiry");

    assert_eq!(ctx.svm.get_balance(&payment).unwrap_or(0), 0, "payment account should be closed");
    let sender_after = ctx.svm.get_balance(&ctx.sender.pubkey()).unwrap();
    assert!(sender_before - sender_after < 100_000, "sender got amount and rent back, minus fees");
}

#[test]
fn spending_with_the_wrong_secret_is_rejected() {
    let mut ctx = setup();
    let payment = ctx.pay();

    let ix = ctx.spend_ix(payment, [9u8; 32], AMOUNT);
    let receiver = ctx.receiver.insecure_clone();
    assert_eq!(ctx.send(ix, &receiver), Err(E_INVALID_SECRET));
}

#[test]
fn claiming_a_different_amount_does_not_resolve_to_the_payment() {
    let mut ctx = setup();
    let payment = ctx.pay();

    // The amount is part of the seeds, so asking for more derives a different (empty) PDA and the
    // account passed in no longer matches.
    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT * 2);
    let receiver = ctx.receiver.insecure_clone();
    assert!(ctx.send(ix, &receiver).is_err(), "seed mismatch must be rejected");
    assert!(ctx.svm.get_balance(&payment).unwrap() > AMOUNT, "funds stay locked");
}

#[test]
fn a_payment_cannot_be_spent_twice() {
    let mut ctx = setup();
    let payment = ctx.pay();

    let receiver = ctx.receiver.insecure_clone();
    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT);
    ctx.send(ix, &receiver).expect("first spend should succeed");

    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT);
    assert!(ctx.send(ix, &receiver).is_err(), "the closed account must not be spendable again");
}
