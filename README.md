# satomic-swap

An HTLC (hash time-locked contract) program for Solana, used by the Komodo DeFi Framework to
settle the Solana side of an atomic swap.

> **Not audited. Devnet only.**
> This program has never been audited. It is live on devnet, for testing, and not on mainnet - where
> the program id below is reserved but unclaimed. Do not send it funds you are not prepared to lose.

Program id: `54mDjQz6jS9sggCV1P5GoUzqHaBEueGVZqmaMPLt2tDx`

## What it does

A payment locks funds that exactly two parties can move:

- the **receiver**, at any time, by revealing a secret that hashes to `secret_hash`;
- the **sender**, but only once `lock_time` has passed.

That asymmetry is the whole protocol. Both sides of a swap lock funds against the same secret
hash; whoever claims first publishes the secret on chain, which lets the counterparty claim the
other side. If nobody claims, both sides refund after their lock times expire.

The lock time gates **only** the refund. The receiver may spend with the secret at any point,
exactly like the UTXO HTLC scripts the framework already uses.

`lock_time` is a Unix timestamp in seconds, compared with the cluster clock's `i64`. A payment whose
lock time does not fit in an `i64` is refused, because it could never expire and so could never be
refunded.

## The address is the commitment

Every parameter that identifies a payment — sender, receiver, mint, secret hash, lock time and
amount — is part of the PDA seeds:

```
lamports:  ["payment",     sender, receiver,       secret_hash, lock_time, amount]
SPL:       ["payment_spl", sender, receiver, mint, secret_hash, lock_time, amount]
```

Anchor re-derives that address on every instruction, so a caller cannot substitute a different
vault, amount or counterparty: the account simply will not match. Nothing about the payment has to
be stored on chain and re-validated, and nobody can squat a pending swap's address without
reproducing its exact parameters — which includes funding the full amount.

The account holds the funds itself (for SOL) or owns the vault token account (for SPL), and it is
**closed** on spend or refund so the rent returns to whoever paid it. That matters if you are
integrating: a settled payment's account no longer exists, so a client that looks one up by address
and finds nothing must not conclude the payment was never made. Use `getSignaturesForAddress` on
the PDA to tell "never sent" from "already settled".

The SPL vault is an associated token account, and those take deposits from anyone, so it can hold
more than `amount`. Its address follows from the payment's parameters too, so it can even be
created before the payment is. Neither gets in the way: the payment accepts a vault that already
exists, and a spend or refund moves the vault's **whole** balance before closing it, so whoever
settles the payment also collects any surplus. Moving only `amount` would leave the vault
impossible to close, which used to mean one stray token unit could lock a payment for good.

## Who can act

Theft is prevented by the PDA seeds, not by signatures: a spend always pays the receiver named in
the seeds, and a refund always pays the sender. A third party who executed either could only
deliver the funds to their rightful owner.

On top of that, each instruction requires the party it belongs to: only the receiver may spend, and
only the sender may refund. That rule is a deliberate policy rather than a security necessity. The
Komodo DeFi Framework runs no watchers for Solana swaps, so no third party ever needs to act on a
user's behalf, and keeping the set of actors to the two parties means nobody else can, for example,
refund a payment out from under a receiver claiming late.

Watcher support would mean relaxing this — any signer, seed-bound destinations — and would need
re-auditing.

## Events, and where to read the secret

| Event | Emitted by | Carries |
|---|---|---|
| `PaymentSent` | `lamports_payment`, `spl_payment` | payment, sender, receiver, mint (`None` for SOL), amount, lock time, secret hash |
| `PaymentSpent` | `receiver_spend_*` | payment, receiver, amount, the revealed secret |
| `PaymentRefunded` | `sender_refund_*` | payment, sender, amount |

The events are a convenience. **Do not take the secret from `PaymentSpent`.** Anchor's `emit!`
writes events to the transaction's logs, and the runtime keeps only the first 10,000 bytes of those.
A receiver who pads its spend with cheap instructions pushes the event out of the logs: the claim
still succeeds, but a client waiting for the event never sees the secret, misses its own claim
window, and loses its side of the swap. `tests/htlc/tests/lamports.rs` does exactly that with 48
one-byte memos.

Read the secret from the spend instruction itself, which cannot be dropped:

1. Find the transactions with `getSignaturesForAddress` on the payment PDA.
2. In each, look for an instruction to this program whose data starts with the
   `receiver_spend_lamports` or `receiver_spend_spl` discriminator. Search the inner instructions
   too: a receiver can spend through another program. The 32 bytes after the discriminator are the
   secret.
3. Check that it hashes to the payment's `secret_hash` before relying on it.

The same caution applies to anything else read from logs. Any program can log a line that looks
exactly like one of these events, so a line counts only if it falls inside this program's own
invocation.

## Which mints are accepted

A payment is checked against its mint when it is made, as a constraint that runs **before** the
vault is created, so a rejected mint costs nothing. The rule: nobody but the swap may be able to
move the locked tokens, or stop them moving.

- **Classic SPL Token** mints have no extensions and are always accepted.
- **Token-2022** mints are accepted only if every extension they carry is on an allowlist. Anything
  else, including any extension Token-2022 adds later, is refused with `MintExtensionNotSupported`.
  - Accepted: token metadata and group pointers, interest-bearing and scaled-UI-amount configs
    (they change only how amounts are displayed), a mint close authority (a mint can only be closed
    at zero supply, and the vault holds some), default account state, and confidential transfers
    (only the payment PDA could configure the vault for them).
  - Accepted only if nobody can ever switch it on: a transfer hook with no hook program and no
    authority, and a pausable config that is unpaused with no authority. This program forwards no
    hook accounts, so a hook set mid-swap would block every transfer out of the vault.
  - Never accepted: a permanent delegate, which can move the vault's tokens at will.
- A **transfer fee** is refused with `TransferFeeNotSupported`, and so is a fee-free config whose
  authority could add one later. With a fee, the vault receives less than `amount`, so either the
  receiver is short-changed or the sender has to over-send.

None of this is re-checked on spend or refund, and it need not be: extensions cannot be added to an
existing mint, and every configuration accepted above has no authority left to change it.
Re-checking would only hand such an authority a way to trap funds mid-swap.

**The mint's freeze authority is accepted**, because USDC and USDT have one. It can freeze the
vault, so a swap is only as safe as the issuer of the token it locks. KDF should only offer tokens
whose issuer it trusts not to take sides in a swap.

## Compute budget

Callers should set an explicit `SetComputeUnitLimit`. Requesting fewer units than an instruction
uses fails it outright; requesting far more overpays, because a priority fee is charged per
*requested* unit rather than per consumed unit.

The cost is not fixed. Deriving a PDA tries bumps from 255 downwards, and each one rejected costs
1,500 units, so an instruction's cost depends on the payment's own addresses. With `p` as
`255 - bump` for the payment PDA and `v` the same for the vault, both from the
`find_program_address` calls that build the instruction:

| Instruction | Compute units | Token-2022 |
|---|---|---|
| `lamports_payment` | 8,815 + 1,500·p | — |
| `receiver_spend_lamports` | 5,172 | — |
| `sender_refund_lamports` | 4,296 | — |
| `spl_payment` | 32,802 + 1,500·p + 4,500·v | 37,524 + 1,500·p + 4,500·v |
| `receiver_spend_spl` | 13,486 + 1,500·v | 16,760 + 1,500·v |
| `sender_refund_spl` | 13,458 + 1,500·v | 16,733 + 1,500·v |

A fixed limit that ignores those terms works for most payments, then fails for one whose address
needed a long search, and fails on every retry, because the address never changes. If that is a
spend, it can keep failing until the sender is able to refund. Add a margin on top of the formula,
or simulate the transaction and use its `unitsConsumed`.

These figures are pinned in `tests/htlc/tests/common/mod.rs`, which fails if an instruction grows
past its ceiling. Regenerate this table if you change them.

## Integrating without this crate

The Komodo DeFi Framework does not depend on this crate: it pins `solana-*` 2.x while the program
needs 3.x/4.x. It builds the instructions by hand from the discriminators in
[`idl/satomic_swap.json`](idl/satomic_swap.json), which is checked in for exactly that reason and
verified against the compiled program by `tests/htlc/tests/idl.rs`.

If you rename an instruction or an event, its discriminator changes silently. Those tests are what
catch it — regenerate the IDL and update the consumer's copy.

## Development

Requires the [Solana toolchain](https://docs.anza.xyz/cli/install) and
[Anchor](https://www.anchor-lang.com/docs/installation) 1.2.0
(`cargo install anchor-cli --version 1.2.0 --locked`).

```sh
anchor build                                # builds the program
anchor idl build > idl/satomic_swap.json    # regenerates the checked-in IDL
cargo +nightly test -p htlc-tests           # runs the test suite
```

Nightly is required for the tests, not for the program: litesvm's dependency tree uses the unstable
`maybe_uninit_write_slice`, so `cargo test` on stable does not compile.

The tests live in their own crate (`tests/htlc`) so that **the program crate carries no
dev-dependencies**. `anchor idl build` compiles the program's test target, and a dev-dependency on
litesvm would drag nightly-only code into a stable build. CI enforces this.

Before opening a pull request:

```sh
cargo fmt --all
cargo clippy -p satomic-swap -- -D warnings
cargo +nightly clippy -p htlc-tests --tests -- -D warnings
```

## Deploying

See [docs/DEPLOYMENT.md](docs/DEPLOYMENT.md) for the runbook: backing up the program keypair,
choosing an upgrade-authority policy, the devnet and mainnet steps, and how to publish a build
anyone can verify against the audited commit.

## Status

| | |
|---|---|
| Audited | No |
| Deployed to devnet | Yes: commit `f0a4a35`, slot 512756155, binary sha256 `31d280dab2599b09877fbb4cdc28e9dfaa4414133beb52a87c123cc71dc2b194` |
| Deployed to mainnet | No |
| Upgrade authority | Devnet: single key `CfwfUDVVgrtfPx38y8T6aiEmcGpSBK54Cv3z4213vbS4`. Mainnet: not yet established |

The program must be audited, then deployed with a documented upgrade-authority policy, before the
framework enables Solana trading.
