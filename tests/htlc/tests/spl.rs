//! HTLC tests for the SPL / Token-2022 path, including the transfer-fee rejection.

use anchor_lang::{InstructionData, ToAccountMetas};
use litesvm::types::TransactionMetadata;
use litesvm::LiteSVM;
use solana_account::Account as SolanaAccount;
use solana_clock::Clock;
use solana_instruction::Instruction;
use solana_keypair::Keypair;
use solana_program_pack::Pack;
use solana_pubkey::{pubkey, Pubkey};
use solana_signer::Signer;
use solana_transaction::Transaction;
use spl_token_2022_interface::extension::transfer_fee::instruction::initialize_transfer_fee_config;
use spl_token_2022_interface::extension::ExtensionType;
use spl_token_2022_interface::instruction::initialize_mint2;
use spl_token_2022_interface::state::{
    Account as TokenAccountState, AccountState, Mint as MintState,
};

mod common;
use common::{assert_compute_units, cu, expect_event, TxFailure};
use satomic_swap::events::{PaymentRefunded, PaymentSent, PaymentSpent};

const PROGRAM_ID: Pubkey = satomic_swap::ID;
const ATA_PROGRAM: Pubkey = pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
const SPL_TOKEN: Pubkey = pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
const TOKEN_2022: Pubkey = pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

const DECIMALS: u8 = 6;
const AMOUNT: u64 = 1_500_000;
const BALANCE: u64 = 10_000_000;
const LOCK_TIME: u64 = 1_800_000_000;

const E_INVALID_SECRET: u32 = 6002;
const E_LOCK_TIME_NOT_EXPIRED: u32 = 6003;
const E_TRANSFER_FEE_NOT_SUPPORTED: u32 = 6007;
const E_ZERO_AMOUNT: u32 = 6000;
const E_INVALID_RECEIVER: u32 = 6001;

struct Ctx {
    svm: LiteSVM,
    sender: Keypair,
    receiver: Keypair,
    mint: Pubkey,
    token_program: Pubkey,
    secret: [u8; 32],
    secret_hash: [u8; 32],
}

fn ata(owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), token_program.as_ref(), mint.as_ref()],
        &ATA_PROGRAM,
    )
    .0
}

/// Creates a real mint by sending the token program's own instructions, optionally with a
/// transfer-fee config so we can prove the rejection fires.
fn create_mint(
    svm: &mut LiteSVM,
    payer: &Keypair,
    token_program: Pubkey,
    fee_bps: Option<u16>,
) -> Pubkey {
    let mint = Keypair::new();
    let space = match fee_bps {
        Some(_) => ExtensionType::try_calculate_account_len::<MintState>(&[
            ExtensionType::TransferFeeConfig,
        ])
        .unwrap(),
        None => MintState::LEN,
    };

    let mut ixs = vec![solana_system_interface::instruction::create_account(
        &payer.pubkey(),
        &mint.pubkey(),
        100_000_000,
        space as u64,
        &token_program,
    )];
    if let Some(bps) = fee_bps {
        // The extension must be initialized before the mint itself.
        ixs.push(
            initialize_transfer_fee_config(
                &token_program,
                &mint.pubkey(),
                Some(&payer.pubkey()),
                Some(&payer.pubkey()),
                bps,
                u64::MAX,
            )
            .unwrap(),
        );
    }
    ixs.push(
        initialize_mint2(
            &token_program,
            &mint.pubkey(),
            &payer.pubkey(),
            None,
            DECIMALS,
        )
        .unwrap(),
    );

    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer, &mint],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx)
        .expect("mint creation should succeed");
    mint.pubkey()
}

/// Writes a token account straight into the SVM. Simpler than driving the token program, and it
/// keeps these tests about the swap rather than about SPL plumbing.
fn put_token_account(
    svm: &mut LiteSVM,
    owner: &Pubkey,
    mint: &Pubkey,
    token_program: &Pubkey,
    amount: u64,
) -> Pubkey {
    let address = ata(owner, mint, token_program);
    let mut data = vec![0u8; TokenAccountState::LEN];
    TokenAccountState::pack(
        TokenAccountState {
            mint: *mint,
            owner: *owner,
            amount,
            delegate: None.into(),
            state: AccountState::Initialized,
            is_native: None.into(),
            delegated_amount: 0,
            close_authority: None.into(),
        },
        &mut data,
    )
    .unwrap();
    svm.set_account(
        address,
        SolanaAccount {
            lamports: 100_000_000,
            data,
            owner: *token_program,
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();
    address
}

fn setup(token_program: Pubkey, fee_bps: Option<u16>) -> Ctx {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(PROGRAM_ID, "../../target/deploy/satomic_swap.so")
        .expect("run `anchor build` first");

    let sender = Keypair::new();
    let receiver = Keypair::new();
    svm.airdrop(&sender.pubkey(), 100_000_000_000).unwrap();
    svm.airdrop(&receiver.pubkey(), 100_000_000_000).unwrap();

    let mint = create_mint(&mut svm, &sender, token_program, fee_bps);
    put_token_account(&mut svm, &sender.pubkey(), &mint, &token_program, BALANCE);
    put_token_account(&mut svm, &receiver.pubkey(), &mint, &token_program, 0);

    let secret = [3u8; 32];
    let secret_hash = solana_sha256_hasher::hash(&secret).to_bytes();
    Ctx {
        svm,
        sender,
        receiver,
        mint,
        token_program,
        secret,
        secret_hash,
    }
}

impl Ctx {
    fn payment_pda(&self) -> Pubkey {
        Pubkey::find_program_address(
            &[
                b"payment_spl",
                self.sender.pubkey().as_ref(),
                self.receiver.pubkey().as_ref(),
                self.mint.as_ref(),
                &self.secret_hash,
                &LOCK_TIME.to_le_bytes(),
                &AMOUNT.to_le_bytes(),
            ],
            &PROGRAM_ID,
        )
        .0
    }

    fn send(
        &mut self,
        ix: Instruction,
        signer: &Keypair,
    ) -> Result<TransactionMetadata, TxFailure> {
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
            Err(failed) => {
                let rendered = format!("{:?}", failed.err);
                if std::env::var("DUMP_ERR").is_ok() {
                    eprintln!("raw tx error: {rendered}");
                }
                Err(TxFailure(rendered))
            }
        }
    }

    fn pay(&mut self) -> Result<(Pubkey, TransactionMetadata), TxFailure> {
        let payment = self.payment_pda();
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::SplPayment {
                sender: self.sender.pubkey(),
                receiver: self.receiver.pubkey(),
                mint: self.mint,
                sender_token_account: ata(&self.sender.pubkey(), &self.mint, &self.token_program),
                payment,
                vault: ata(&payment, &self.mint, &self.token_program),
                token_program: self.token_program,
                associated_token_program: ATA_PROGRAM,
                system_program: solana_system_interface::program::ID,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::SplPayment {
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount: AMOUNT,
            }
            .data(),
        };
        let sender = self.sender.insecure_clone();
        self.send(ix, &sender).map(|meta| (payment, meta))
    }

    fn spend(
        &mut self,
        payment: Pubkey,
        secret: [u8; 32],
    ) -> Result<TransactionMetadata, TxFailure> {
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::ReceiverSpendSpl {
                receiver: self.receiver.pubkey(),
                sender: self.sender.pubkey(),
                mint: self.mint,
                receiver_token_account: ata(
                    &self.receiver.pubkey(),
                    &self.mint,
                    &self.token_program,
                ),
                payment,
                vault: ata(&payment, &self.mint, &self.token_program),
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::ReceiverSpendSpl {
                secret,
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount: AMOUNT,
            }
            .data(),
        };
        let receiver = self.receiver.insecure_clone();
        self.send(ix, &receiver)
    }

    fn refund(&mut self, payment: Pubkey) -> Result<TransactionMetadata, TxFailure> {
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::SenderRefundSpl {
                sender: self.sender.pubkey(),
                receiver: self.receiver.pubkey(),
                mint: self.mint,
                sender_token_account: ata(&self.sender.pubkey(), &self.mint, &self.token_program),
                payment,
                vault: ata(&payment, &self.mint, &self.token_program),
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::SenderRefundSpl {
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount: AMOUNT,
            }
            .data(),
        };
        let sender = self.sender.insecure_clone();
        self.send(ix, &sender)
    }

    /// Sends `ix` paid for and signed only by `payer`, leaving any other required signature
    /// missing. This is what impersonation actually looks like: the attacker names the real
    /// counterparty so the PDA still resolves, and then cannot sign for them.
    /// A payment with an arbitrary receiver and amount, for the rejection cases.
    fn pay_with(
        &mut self,
        receiver: Pubkey,
        amount: u64,
    ) -> Result<TransactionMetadata, TxFailure> {
        let payment = Pubkey::find_program_address(
            &[
                b"payment_spl",
                self.sender.pubkey().as_ref(),
                receiver.as_ref(),
                self.mint.as_ref(),
                &self.secret_hash,
                &LOCK_TIME.to_le_bytes(),
                &amount.to_le_bytes(),
            ],
            &PROGRAM_ID,
        )
        .0;
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::SplPayment {
                sender: self.sender.pubkey(),
                receiver,
                mint: self.mint,
                sender_token_account: ata(&self.sender.pubkey(), &self.mint, &self.token_program),
                payment,
                vault: ata(&payment, &self.mint, &self.token_program),
                token_program: self.token_program,
                associated_token_program: ATA_PROGRAM,
                system_program: solana_system_interface::program::ID,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::SplPayment {
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount,
            }
            .data(),
        };
        let sender = self.sender.insecure_clone();
        self.send(ix, &sender)
    }

    fn send_without_all_signatures(
        &mut self,
        ix: Instruction,
        payer: &Keypair,
    ) -> Result<TransactionMetadata, TxFailure> {
        let mut tx = Transaction::new_with_payer(&[ix], Some(&payer.pubkey()));
        tx.partial_sign(&[payer], self.svm.latest_blockhash());
        match self.svm.send_transaction(tx) {
            Ok(meta) => Ok(meta),
            Err(failed) => Err(TxFailure(format!("{:?}", failed.err))),
        }
    }

    /// The spend instruction as the real receiver would send it.
    fn spend_ix(&self, payment: Pubkey, secret: [u8; 32]) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::ReceiverSpendSpl {
                receiver: self.receiver.pubkey(),
                sender: self.sender.pubkey(),
                mint: self.mint,
                receiver_token_account: ata(
                    &self.receiver.pubkey(),
                    &self.mint,
                    &self.token_program,
                ),
                payment,
                vault: ata(&payment, &self.mint, &self.token_program),
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::ReceiverSpendSpl {
                secret,
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount: AMOUNT,
            }
            .data(),
        }
    }

    /// The refund instruction as the real sender would send it.
    fn refund_ix(&self, payment: Pubkey) -> Instruction {
        Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::SenderRefundSpl {
                sender: self.sender.pubkey(),
                receiver: self.receiver.pubkey(),
                mint: self.mint,
                sender_token_account: ata(&self.sender.pubkey(), &self.mint, &self.token_program),
                payment,
                vault: ata(&payment, &self.mint, &self.token_program),
                token_program: self.token_program,
            }
            .to_account_metas(None),
            data: satomic_swap::instruction::SenderRefundSpl {
                secret_hash: self.secret_hash,
                lock_time: LOCK_TIME,
                amount: AMOUNT,
            }
            .data(),
        }
    }

    fn token_balance(&self, address: &Pubkey) -> u64 {
        let account = self
            .svm
            .get_account(address)
            .expect("token account should exist");
        TokenAccountState::unpack(&account.data[..TokenAccountState::LEN])
            .unwrap()
            .amount
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
fn spl_payment_locks_tokens_and_the_receiver_can_claim_them() {
    let mut ctx = setup(SPL_TOKEN, None);
    let sender_ata = ata(&ctx.sender.pubkey(), &ctx.mint, &SPL_TOKEN);
    let receiver_ata = ata(&ctx.receiver.pubkey(), &ctx.mint, &SPL_TOKEN);

    let (payment, meta) = ctx.pay().expect("payment should succeed");
    let vault = ata(&payment, &ctx.mint, &SPL_TOKEN);

    // The framework finds payments by these events, since the account is closed once settled.
    let sent: PaymentSent = expect_event(&meta.logs);
    assert_eq!(sent.payment, payment);
    assert_eq!(sent.sender, ctx.sender.pubkey());
    assert_eq!(sent.receiver, ctx.receiver.pubkey());
    assert_eq!(
        sent.mint,
        Some(ctx.mint),
        "an SPL payment must name its mint"
    );
    assert_eq!(sent.amount, AMOUNT);
    assert_eq!(sent.lock_time, LOCK_TIME);
    assert_eq!(sent.secret_hash, ctx.secret_hash);

    assert_eq!(
        ctx.token_balance(&sender_ata),
        BALANCE - AMOUNT,
        "tokens left the sender"
    );
    assert_eq!(
        ctx.token_balance(&vault),
        AMOUNT,
        "tokens are held by the vault"
    );

    let meta = ctx
        .spend(payment, ctx.secret)
        .expect("spend should succeed");

    // The counterparty claims the other side of the swap with this secret, so it has to be the
    // real preimage rather than whatever was passed in.
    let spent: PaymentSpent = expect_event(&meta.logs);
    assert_eq!(spent.payment, payment);
    assert_eq!(spent.receiver, ctx.receiver.pubkey());
    assert_eq!(spent.amount, AMOUNT);
    assert_eq!(spent.secret, ctx.secret);

    assert_eq!(
        ctx.token_balance(&receiver_ata),
        AMOUNT,
        "receiver got the tokens"
    );
    assert!(
        ctx.svm
            .get_account(&vault)
            .map(|a| a.data.is_empty())
            .unwrap_or(true),
        "vault should be closed"
    );
    assert!(
        ctx.svm
            .get_account(&payment)
            .map(|a| a.data.is_empty())
            .unwrap_or(true),
        "payment should be closed"
    );
}

#[test]
fn spl_refund_is_gated_by_the_lock_time() {
    let mut ctx = setup(SPL_TOKEN, None);
    let sender_ata = ata(&ctx.sender.pubkey(), &ctx.mint, &SPL_TOKEN);
    let (payment, _) = ctx.pay().expect("payment should succeed");

    assert_eq!(
        ctx.refund(payment).unwrap_err().code(),
        E_LOCK_TIME_NOT_EXPIRED,
        "refund must wait for expiry"
    );

    ctx.warp_past_lock_time();
    let meta = ctx
        .refund(payment)
        .expect("refund should succeed after expiry");

    let refunded: PaymentRefunded = expect_event(&meta.logs);
    assert_eq!(refunded.payment, payment);
    assert_eq!(refunded.sender, ctx.sender.pubkey());
    assert_eq!(refunded.amount, AMOUNT);

    assert_eq!(
        ctx.token_balance(&sender_ata),
        BALANCE,
        "sender got every token back"
    );
}

#[test]
fn spl_spend_with_the_wrong_secret_is_rejected() {
    let mut ctx = setup(SPL_TOKEN, None);
    let (payment, _) = ctx.pay().expect("payment should succeed");
    assert_eq!(
        ctx.spend(payment, [8u8; 32]).unwrap_err().code(),
        E_INVALID_SECRET
    );
}

#[test]
fn token_2022_mint_without_a_fee_works() {
    let mut ctx = setup(TOKEN_2022, None);
    let receiver_ata = ata(&ctx.receiver.pubkey(), &ctx.mint, &TOKEN_2022);

    let (payment, _) = ctx
        .pay()
        .expect("a plain Token-2022 mint should be accepted");
    ctx.spend(payment, ctx.secret)
        .expect("spend should succeed");
    assert_eq!(ctx.token_balance(&receiver_ata), AMOUNT);
}

#[test]
fn a_mint_that_charges_a_transfer_fee_is_rejected() {
    let mut ctx = setup(TOKEN_2022, Some(250));
    assert_eq!(
        ctx.pay().unwrap_err().code(),
        E_TRANSFER_FEE_NOT_SUPPORTED,
        "a fee would leave the vault short of the agreed amount"
    );
}

#[test]
fn a_fee_free_mint_that_could_still_add_one_is_rejected() {
    // Zero fee today, but the config authority can raise it before the payment is claimed.
    let mut ctx = setup(TOKEN_2022, Some(0));
    assert_eq!(ctx.pay().unwrap_err().code(), E_TRANSFER_FEE_NOT_SUPPORTED);
}

// --- Authorization ---
//
// The same gap as on the lamports path: knowing the secret must not be enough to move the funds.

#[test]
fn a_third_party_who_knows_the_secret_cannot_spend_spl() {
    let mut ctx = setup(SPL_TOKEN, None);
    let (payment, _) = ctx.pay().expect("payment should succeed");
    let vault = ata(&payment, &ctx.mint, &SPL_TOKEN);
    let attacker = Keypair::new();
    ctx.svm.airdrop(&attacker.pubkey(), 1_000_000_000).unwrap();

    // Naming the real receiver keeps the PDA correct, so only the missing signature stops this.
    let ix = ctx.spend_ix(payment, ctx.secret);
    let failure = ctx
        .send_without_all_signatures(ix, &attacker)
        .expect_err("a third party must not be able to claim the tokens");

    assert!(
        failure.missing_signature(),
        "expected a signature failure, got {failure:?}"
    );
    assert_eq!(
        ctx.token_balance(&vault),
        AMOUNT,
        "tokens stay in the vault"
    );
}

#[test]
fn the_sender_cannot_spend_its_own_spl_payment() {
    let mut ctx = setup(SPL_TOKEN, None);
    let (payment, _) = ctx.pay().expect("payment should succeed");
    let vault = ata(&payment, &ctx.mint, &SPL_TOKEN);

    let ix = ctx.spend_ix(payment, ctx.secret);
    let sender = ctx.sender.insecure_clone();
    let failure = ctx
        .send_without_all_signatures(ix, &sender)
        .expect_err("the sender must not be able to spend as the receiver");

    assert!(
        failure.missing_signature(),
        "expected a signature failure, got {failure:?}"
    );
    assert_eq!(
        ctx.token_balance(&vault),
        AMOUNT,
        "tokens stay in the vault"
    );
}

#[test]
fn the_receiver_cannot_refund_an_spl_payment_after_expiry() {
    let mut ctx = setup(SPL_TOKEN, None);
    let (payment, _) = ctx.pay().expect("payment should succeed");
    let vault = ata(&payment, &ctx.mint, &SPL_TOKEN);
    ctx.warp_past_lock_time();

    let ix = ctx.refund_ix(payment);
    let receiver = ctx.receiver.insecure_clone();
    let failure = ctx
        .send_without_all_signatures(ix, &receiver)
        .expect_err("only the sender may refund");

    assert!(
        failure.missing_signature(),
        "expected a signature failure, got {failure:?}"
    );
    assert_eq!(
        ctx.token_balance(&vault),
        AMOUNT,
        "tokens stay in the vault"
    );
}

/// The vault is an associated account of the payment PDA, so redirecting the transfer means
/// passing a token account the constraints do not accept.
#[test]
fn the_receiver_cannot_send_the_tokens_to_someone_else() {
    let mut ctx = setup(SPL_TOKEN, None);
    let (payment, _) = ctx.pay().expect("payment should succeed");
    let outsider = Keypair::new();
    let outsider_ata =
        put_token_account(&mut ctx.svm, &outsider.pubkey(), &ctx.mint, &SPL_TOKEN, 0);

    let mut ix = ctx.spend_ix(payment, ctx.secret);
    // accounts[3] is `receiver_token_account`, which is constrained to the receiver's authority.
    ix.accounts[3].pubkey = outsider_ata;

    let receiver = ctx.receiver.insecure_clone();
    assert!(
        ctx.send(ix, &receiver).is_err(),
        "the destination must belong to the receiver named in the seeds"
    );
}

// --- Payment constraints ---

#[test]
fn a_zero_amount_spl_payment_is_rejected() {
    let mut ctx = setup(SPL_TOKEN, None);
    let receiver = ctx.receiver.pubkey();

    assert_eq!(ctx.pay_with(receiver, 0).unwrap_err().code(), E_ZERO_AMOUNT);
}

#[test]
fn an_spl_payment_to_myself_is_rejected() {
    let mut ctx = setup(SPL_TOKEN, None);
    let sender = ctx.sender.pubkey();

    assert_eq!(
        ctx.pay_with(sender, AMOUNT).unwrap_err().code(),
        E_INVALID_RECEIVER
    );
}

#[test]
fn an_spl_payment_to_the_default_pubkey_is_rejected() {
    let mut ctx = setup(SPL_TOKEN, None);

    assert_eq!(
        ctx.pay_with(Pubkey::default(), AMOUNT).unwrap_err().code(),
        E_INVALID_RECEIVER
    );
}

/// Pins what each instruction costs, on both token programs: Token-2022's extension handling makes
/// a spend noticeably more expensive than classic SPL Token, and the limit KDF requests has to
/// cover whichever mint a swap actually uses.
#[test]
fn spl_instructions_stay_within_their_compute_budget() {
    for token_program in [SPL_TOKEN, TOKEN_2022] {
        let mut ctx = setup(token_program, None);
        let (payment, meta) = ctx.pay().expect("payment should succeed");
        assert_compute_units("spl_payment", meta.compute_units_consumed, cu::SPL_PAYMENT);

        let meta = ctx
            .spend(payment, ctx.secret)
            .expect("spend should succeed");
        assert_compute_units(
            "receiver_spend_spl",
            meta.compute_units_consumed,
            cu::RECEIVER_SPEND_SPL,
        );

        let mut ctx = setup(token_program, None);
        let (payment, _) = ctx.pay().expect("payment should succeed");
        ctx.warp_past_lock_time();
        let meta = ctx.refund(payment).expect("refund should succeed");
        assert_compute_units(
            "sender_refund_spl",
            meta.compute_units_consumed,
            cu::SENDER_REFUND_SPL,
        );
    }
}
