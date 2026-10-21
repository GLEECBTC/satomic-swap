//! HTLC tests for the SPL / Token-2022 path, including the transfer-fee rejection.

use anchor_lang::{InstructionData, ToAccountMetas};
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
use spl_token_2022_interface::state::{Account as TokenAccountState, AccountState, Mint as MintState};

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
fn create_mint(svm: &mut LiteSVM, payer: &Keypair, token_program: Pubkey, fee_bps: Option<u16>) -> Pubkey {
    let mint = Keypair::new();
    let space = match fee_bps {
        Some(_) => {
            ExtensionType::try_calculate_account_len::<MintState>(&[ExtensionType::TransferFeeConfig])
                .unwrap()
        },
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
    ixs.push(initialize_mint2(&token_program, &mint.pubkey(), &payer.pubkey(), None, DECIMALS).unwrap());

    let tx = Transaction::new_signed_with_payer(
        &ixs,
        Some(&payer.pubkey()),
        &[payer, &mint],
        svm.latest_blockhash(),
    );
    svm.send_transaction(tx).expect("mint creation should succeed");
    mint.pubkey()
}

/// Writes a token account straight into the SVM. Simpler than driving the token program, and it
/// keeps these tests about the swap rather than about SPL plumbing.
fn put_token_account(svm: &mut LiteSVM, owner: &Pubkey, mint: &Pubkey, token_program: &Pubkey, amount: u64) -> Pubkey {
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
        SolanaAccount { lamports: 100_000_000, data, owner: *token_program, executable: false, rent_epoch: 0 },
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
    Ctx { svm, sender, receiver, mint, token_program, secret, secret_hash }
}

fn custom_code(rendered: &str) -> u32 {
    rendered
        .split("Custom(")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|n| n.trim().parse().ok())
        .unwrap_or(0)
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

    fn send(&mut self, ix: Instruction, signer: &Keypair) -> Result<(), u32> {
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&signer.pubkey()),
            &[signer],
            self.svm.latest_blockhash(),
        );
        match self.svm.send_transaction(tx) {
            Ok(_) => Ok(()),
            Err(failed) => {
                let rendered = format!("{:?}", failed.err);
                if std::env::var("DUMP_ERR").is_ok() {
                    eprintln!("raw tx error: {rendered}");
                }
                Err(custom_code(&rendered))
            },
        }
    }

    fn pay(&mut self) -> Result<Pubkey, u32> {
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
        self.send(ix, &sender).map(|_| payment)
    }

    fn spend(&mut self, payment: Pubkey, secret: [u8; 32]) -> Result<(), u32> {
        let ix = Instruction {
            program_id: PROGRAM_ID,
            accounts: satomic_swap::accounts::ReceiverSpendSpl {
                receiver: self.receiver.pubkey(),
                sender: self.sender.pubkey(),
                mint: self.mint,
                receiver_token_account: ata(&self.receiver.pubkey(), &self.mint, &self.token_program),
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

    fn refund(&mut self, payment: Pubkey) -> Result<(), u32> {
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

    fn token_balance(&self, address: &Pubkey) -> u64 {
        let account = self.svm.get_account(address).expect("token account should exist");
        TokenAccountState::unpack(&account.data[..TokenAccountState::LEN]).unwrap().amount
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

    let payment = ctx.pay().expect("payment should succeed");
    let vault = ata(&payment, &ctx.mint, &SPL_TOKEN);

    assert_eq!(ctx.token_balance(&sender_ata), BALANCE - AMOUNT, "tokens left the sender");
    assert_eq!(ctx.token_balance(&vault), AMOUNT, "tokens are held by the vault");

    ctx.spend(payment, ctx.secret).expect("spend should succeed");

    assert_eq!(ctx.token_balance(&receiver_ata), AMOUNT, "receiver got the tokens");
    assert!(ctx.svm.get_account(&vault).map(|a| a.data.is_empty()).unwrap_or(true), "vault should be closed");
    assert!(ctx.svm.get_account(&payment).map(|a| a.data.is_empty()).unwrap_or(true), "payment should be closed");
}

#[test]
fn spl_refund_is_gated_by_the_lock_time() {
    let mut ctx = setup(SPL_TOKEN, None);
    let sender_ata = ata(&ctx.sender.pubkey(), &ctx.mint, &SPL_TOKEN);
    let payment = ctx.pay().expect("payment should succeed");

    assert_eq!(ctx.refund(payment), Err(E_LOCK_TIME_NOT_EXPIRED), "refund must wait for expiry");

    ctx.warp_past_lock_time();
    ctx.refund(payment).expect("refund should succeed after expiry");
    assert_eq!(ctx.token_balance(&sender_ata), BALANCE, "sender got every token back");
}

#[test]
fn spl_spend_with_the_wrong_secret_is_rejected() {
    let mut ctx = setup(SPL_TOKEN, None);
    let payment = ctx.pay().expect("payment should succeed");
    assert_eq!(ctx.spend(payment, [8u8; 32]), Err(E_INVALID_SECRET));
}

#[test]
fn token_2022_mint_without_a_fee_works() {
    let mut ctx = setup(TOKEN_2022, None);
    let receiver_ata = ata(&ctx.receiver.pubkey(), &ctx.mint, &TOKEN_2022);

    let payment = ctx.pay().expect("a plain Token-2022 mint should be accepted");
    ctx.spend(payment, ctx.secret).expect("spend should succeed");
    assert_eq!(ctx.token_balance(&receiver_ata), AMOUNT);
}

#[test]
fn a_mint_that_charges_a_transfer_fee_is_rejected() {
    let mut ctx = setup(TOKEN_2022, Some(250));
    assert_eq!(
        ctx.pay(),
        Err(E_TRANSFER_FEE_NOT_SUPPORTED),
        "a fee would leave the vault short of the agreed amount"
    );
}

#[test]
fn a_fee_free_mint_that_could_still_add_one_is_rejected() {
    // Zero fee today, but the config authority can raise it before the payment is claimed.
    let mut ctx = setup(TOKEN_2022, Some(0));
    assert_eq!(ctx.pay(), Err(E_TRANSFER_FEE_NOT_SUPPORTED));
}
