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
and finds nothing must not conclude the payment was never made. Use the events, or
`getSignaturesForAddress` on the PDA, to tell "never sent" from "already settled".

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

## Events

The three events are the only durable record once an account is closed, so integrations should
treat them as the API rather than as a debugging aid.

| Event | Emitted by | Carries |
|---|---|---|
| `PaymentSent` | `lamports_payment`, `spl_payment` | payment, sender, receiver, mint (`None` for SOL), amount, lock time, secret hash |
| `PaymentSpent` | `receiver_spend_*` | payment, receiver, amount, **the revealed secret** |
| `PaymentRefunded` | `sender_refund_*` | payment, sender, amount |

`PaymentSpent.secret` is what the counterparty needs to claim the other side of the swap.

## Mints with a transfer fee are rejected

A Token-2022 mint that charges a transfer fee is refused at payment time, and so is a fee-free one
whose `TransferFeeConfig` still has an authority that could add a fee later.

With a fee, the vault receives less than `amount`, so either the receiver is short-changed or the
sender has to over-send — neither is something a swap can silently absorb. The check runs as a
constraint on the mint **before** the vault is created, so a rejected mint costs nothing. It is
deliberately **not** re-checked on spend or refund: doing that would let a fee authority trap funds
mid-swap by adding a fee after the payment was made.

Classic SPL Token mints carry no extensions and are always accepted.

## Compute budget

Callers should set an explicit `SetComputeUnitLimit`. Requesting fewer units than an instruction
uses fails it outright; requesting far more overpays, because a priority fee is charged per
*requested* unit rather than per consumed unit.

| Instruction | Compute units | Token-2022 |
|---|---|---|
| `lamports_payment` | 8,349 | — |
| `receiver_spend_lamports` | 4,888 | — |
| `sender_refund_lamports` | 4,098 | — |
| `spl_payment` | 37,424 | 34,567 |
| `receiver_spend_spl` | 13,134 | 16,405 |
| `sender_refund_spl` | 13,084 | — |

These are pinned in `tests/htlc/tests/common/mod.rs`, which fails if an instruction grows past its
ceiling. Regenerate this table if you change them.

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
