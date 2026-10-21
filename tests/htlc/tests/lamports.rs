//! HTLC tests for the native SOL path.
//!
//! These cover the cases the original program got wrong: refunding before the lock time, spending
//! with the wrong secret, and substituting parameters that the PDA is supposed to bind.

use anchor_lang::{InstructionData, ToAccountMetas};
use litesvm::types::TransactionMetadata;
use litesvm::LiteSVM;
use solana_clock::Clock;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;

mod common;
use common::{assert_compute_units, cu, expect_event, TxFailure};
use satomic_swap::events::{PaymentRefunded, PaymentSent, PaymentSpent};

const PROGRAM_ID: Pubkey = satomic_swap::ID;
const AMOUNT: u64 = 2_000_000_000;
const LOCK_TIME: u64 = 1_800_000_000;

// Anchor numbers custom errors from 6000 in declaration order.
const E_INVALID_SECRET: u32 = 6002;
const E_LOCK_TIME_NOT_EXPIRED: u32 = 6003;
const E_ZERO_AMOUNT: u32 = 6000;
const E_INVALID_RECEIVER: u32 = 6001;

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
    fn send(&mut self, ix: Instruction, signer: &Keypair) -> Result<TransactionMetadata, TxFailure> {
        self.send_signed_by(ix, signer, signer)
    }

    /// Sends `ix` with `signer` as the authority but `payer` covering the fee, so a test can make
    /// one party attempt another party's instruction.
    fn send_signed_by(
        &mut self,
        ix: Instruction,
        payer: &Keypair,
        signer: &Keypair,
    ) -> Result<TransactionMetadata, TxFailure> {
        let mut signers = vec![payer];
        if signer.pubkey() != payer.pubkey() {
            signers.push(signer);
        }
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &signers,
            self.svm.latest_blockhash(),
        );
        match self.svm.send_transaction(tx) {
            Ok(meta) => Ok(meta),
            Err(failed) => Err(TxFailure(format!("{:?}", failed.err))),
        }
    }

    fn pay(&mut self) -> Pubkey { self.pay_with_logs().0 }

    fn pay_with_logs(&mut self) -> (Pubkey, TransactionMetadata) {
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
        let meta = self.send(ix, &sender).expect("payment should succeed");
        (payment, meta)
    }

    /// A payment with an arbitrary receiver and amount, for the rejection cases.
    fn pay_with(&mut self, receiver: Pubkey, amount: u64) -> Result<TransactionMetadata, TxFailure> {
        let payment = payment_pda(&self.sender.pubkey(), &receiver, &self.secret_hash, amount);
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::LamportsPayment {
                sender: self.sender.pubkey(),
                receiver,
                payment,
                system_program: solana_system_interface::program::ID,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::LamportsPayment {
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount,
            }
            .data(),
        };
        let sender = self.sender.insecure_clone();
        self.send(ix, &sender)
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

    /// Sends `ix` paid for and signed only by `payer`, leaving any other required signature
    /// missing. This is what impersonation actually looks like: the attacker names the real
    /// counterparty so the PDA still resolves, and then cannot sign for them.
    fn send_without_all_signatures(&mut self, ix: Instruction, payer: &Keypair) -> Result<TransactionMetadata, TxFailure> {
        let mut tx = Transaction::new_with_payer(&[ix], Some(&payer.pubkey()));
        tx.partial_sign(&[payer], self.svm.latest_blockhash());
        match self.svm.send_transaction(tx) {
            Ok(meta) => Ok(meta),
            Err(failed) => Err(TxFailure(format!("{:?}", failed.err))),
        }
    }

    fn warp_past_lock_time(&mut self) {
        let mut clock: Clock = self.svm.get_sysvar();
        clock.unix_timestamp = LOCK_TIME as i64 + 1;
        self.svm.set_sysvar(&clock);
        // Time moving forward means a new blockhash; without this a retried instruction is
        // byte-identical to the earlier one and gets rejected as AlreadyProcessed.
        self.svm.expire_blockhash();
    }
}

#[test]
fn receiver_can_spend_with_the_secret_and_rent_returns_to_the_sender() {
    let mut ctx = setup();
    let sender_before = ctx.svm.get_balance(&ctx.sender.pubkey()).unwrap();
    let (payment, meta) = ctx.pay_with_logs();

    // The framework finds payments by these events, since the account is closed once settled.
    let sent: PaymentSent = expect_event(&meta.logs);
    assert_eq!(sent.payment, payment);
    assert_eq!(sent.sender, ctx.sender.pubkey());
    assert_eq!(sent.receiver, ctx.receiver.pubkey());
    assert_eq!(sent.mint, None, "a lamports payment has no mint");
    assert_eq!(sent.amount, AMOUNT);
    assert_eq!(sent.lock_time, LOCK_TIME);
    assert_eq!(sent.secret_hash, ctx.secret_hash);

    // The payment holds the amount plus its own rent.
    let locked = ctx.svm.get_balance(&payment).unwrap();
    assert!(locked > AMOUNT, "payment account should hold amount + rent");

    let receiver_before = ctx.svm.get_balance(&ctx.receiver.pubkey()).unwrap();
    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT);
    let receiver = ctx.receiver.insecure_clone();
    let meta = ctx.send(ix, &receiver).expect("spend should succeed");

    // The counterparty claims the other side of the swap with this secret, so it has to be the
    // real preimage rather than whatever was passed in.
    let spent: PaymentSpent = expect_event(&meta.logs);
    assert_eq!(spent.payment, payment);
    assert_eq!(spent.receiver, ctx.receiver.pubkey());
    assert_eq!(spent.amount, AMOUNT);
    assert_eq!(spent.secret, ctx.secret);

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
        ctx.send(ix, &sender).unwrap_err().code(),
        E_LOCK_TIME_NOT_EXPIRED,
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
    let meta = ctx.send(ix, &sender).expect("refund should succeed after expiry");

    let refunded: PaymentRefunded = expect_event(&meta.logs);
    assert_eq!(refunded.payment, payment);
    assert_eq!(refunded.sender, ctx.sender.pubkey());
    assert_eq!(refunded.amount, AMOUNT);

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
    assert_eq!(
        ctx.send(ix, &receiver).unwrap_err().code(),
        E_INVALID_SECRET);
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

// --- Authorization ---
//
// Everything above tests the *secret*. These test the *signer*: knowing the secret must not be
// enough, or a watcher that learns it from a spend on the other chain could take these funds too.

#[test]
fn a_third_party_who_knows_the_secret_cannot_spend() {
    let mut ctx = setup();
    let payment = ctx.pay();
    let attacker = Keypair::new();
    ctx.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    // Naming the real receiver keeps the PDA correct, so only the missing signature stops this.
    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT);
    let failure = ctx
        .send_without_all_signatures(ix, &attacker)
        .expect_err("a third party must not be able to claim the payment");

    assert!(failure.missing_signature(), "expected a signature failure, got {failure:?}");
    assert!(ctx.svm.get_balance(&payment).unwrap() > AMOUNT, "funds stay locked");
}

#[test]
fn a_third_party_substituting_itself_as_receiver_finds_no_payment() {
    let mut ctx = setup();
    ctx.pay();
    let attacker = Keypair::new();
    ctx.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    // The receiver is a PDA seed, so an attacker who signs for itself addresses a payment that was
    // never made rather than the real one.
    let attacker_payment = payment_pda(&ctx.sender.pubkey(), &attacker.pubkey(), &ctx.secret_hash, AMOUNT);
    let ix = Instruction {
        program_id: PROGRAM_ID,
        accounts: satomic_swap::accounts::ReceiverSpendLamports {
            receiver: attacker.pubkey(),
            sender: ctx.sender.pubkey(),
            payment: attacker_payment,
        }
        .to_account_metas(None),
        data: satomic_swap::instruction::ReceiverSpendLamports {
            secret: ctx.secret,
            secret_hash: ctx.secret_hash,
            lock_time: LOCK_TIME,
            amount: AMOUNT,
        }
        .data(),
    };

    assert!(ctx.send(ix, &attacker).is_err(), "there is no payment at that address");
}

#[test]
fn the_sender_cannot_spend_its_own_payment() {
    let mut ctx = setup();
    let payment = ctx.pay();

    // The maker picks the secret, so it knows it: the signer check is the only thing keeping it
    // from taking its own payment back before the lock time.
    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT);
    let sender = ctx.sender.insecure_clone();
    let failure = ctx
        .send_without_all_signatures(ix, &sender)
        .expect_err("the sender must not be able to spend as the receiver");

    assert!(failure.missing_signature(), "expected a signature failure, got {failure:?}");
    assert!(ctx.svm.get_balance(&payment).unwrap() > AMOUNT, "funds stay locked");
}

#[test]
fn the_receiver_cannot_refund_even_after_expiry() {
    let mut ctx = setup();
    let payment = ctx.pay();
    ctx.warp_past_lock_time();

    // Expiry entitles the *sender* to a refund. If the receiver could trigger it, the refund would
    // still pay the sender, but the receiver could grief a swap it no longer wants to complete.
    let ix = ctx.refund_ix(payment);
    let receiver = ctx.receiver.insecure_clone();
    let failure = ctx
        .send_without_all_signatures(ix, &receiver)
        .expect_err("only the sender may refund");

    assert!(failure.missing_signature(), "expected a signature failure, got {failure:?}");
    assert!(ctx.svm.get_balance(&payment).unwrap() > AMOUNT, "funds stay locked");
}

#[test]
fn a_third_party_cannot_refund_after_expiry() {
    let mut ctx = setup();
    let payment = ctx.pay();
    ctx.warp_past_lock_time();
    let attacker = Keypair::new();
    ctx.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    let ix = ctx.refund_ix(payment);
    let failure = ctx
        .send_without_all_signatures(ix, &attacker)
        .expect_err("a third party must not be able to trigger a refund");

    assert!(failure.missing_signature(), "expected a signature failure, got {failure:?}");
    assert!(ctx.svm.get_balance(&payment).unwrap() > AMOUNT, "funds stay locked");
}

/// A control for the tests above: the same helper must let a properly signed call through, or
/// they would all be passing for the wrong reason.
#[test]
fn the_partial_signing_helper_still_admits_a_legitimate_spend() {
    let mut ctx = setup();
    let payment = ctx.pay();

    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT);
    let receiver = ctx.receiver.insecure_clone();

    ctx.send_without_all_signatures(ix, &receiver)
        .expect("the receiver is the only required signer, so this is fully signed");
    assert_eq!(ctx.svm.get_balance(&payment).unwrap_or(0), 0, "payment was settled");
}

// --- Payment constraints ---

/// A zero-amount payment would create an account holding nothing that the counterparty could still
/// treat as a locked payment.
#[test]
fn a_zero_amount_payment_is_rejected() {
    let mut ctx = setup();
    let receiver = ctx.receiver.pubkey();

    assert_eq!(ctx.pay_with(receiver, 0).unwrap_err().code(), E_ZERO_AMOUNT);
}

/// Paying yourself is not a swap, and it would let someone create a payment nobody else can
/// resolve while still looking like one to a watcher.
#[test]
fn a_payment_to_myself_is_rejected() {
    let mut ctx = setup();
    let sender = ctx.sender.pubkey();

    assert_eq!(ctx.pay_with(sender, AMOUNT).unwrap_err().code(), E_INVALID_RECEIVER);
}

/// The default pubkey has no known private key, so funds sent to it could only ever be refunded.
#[test]
fn a_payment_to_the_default_pubkey_is_rejected() {
    let mut ctx = setup();

    assert_eq!(
        ctx.pay_with(Pubkey::default(), AMOUNT).unwrap_err().code(),
        E_INVALID_RECEIVER
    );
}

/// Pins what each instruction costs, so KDF can request a compute limit that is neither too small
/// to succeed nor so large that the priority fee is wasted.
#[test]
fn lamports_instructions_stay_within_their_compute_budget() {
    let mut ctx = setup();
    let (payment, meta) = ctx.pay_with_logs();
    assert_compute_units("lamports_payment", meta.compute_units_consumed, cu::LAMPORTS_PAYMENT);

    let ix = ctx.spend_ix(payment, ctx.secret, AMOUNT);
    let receiver = ctx.receiver.insecure_clone();
    let meta = ctx.send(ix, &receiver).expect("spend should succeed");
    assert_compute_units(
        "receiver_spend_lamports",
        meta.compute_units_consumed,
        cu::RECEIVER_SPEND_LAMPORTS,
    );

    let mut ctx = setup();
    let payment = ctx.pay();
    ctx.warp_past_lock_time();
    let ix = ctx.refund_ix(payment);
    let sender = ctx.sender.insecure_clone();
    let meta = ctx.send(ix, &sender).expect("refund should succeed");
    assert_compute_units(
        "sender_refund_lamports",
        meta.compute_units_consumed,
        cu::SENDER_REFUND_LAMPORTS,
    );
}
