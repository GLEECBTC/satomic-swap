# Deploying satomic-swap

The program is **not deployed anywhere** today. This is the runbook for changing that.

Program id: `54mDjQz6jS9sggCV1P5GoUzqHaBEueGVZqmaMPLt2tDx`

## Before anything else: back up the program keypair

`target/deploy/satomic_swap-keypair.json` **is** the program address. It is gitignored and exists
on exactly one machine right now.

Lose it and this address can never be deployed to or upgraded again — including after a deploy, so
losing it post-launch would freeze the program at whatever version was live, with no way to ship a
fix. Back it up somewhere durable and access-controlled before the first deploy, not after.

It is not a signing key for user funds; it only controls this address. That makes it lower-stakes
than the upgrade authority below, but it is still unrecoverable.

## Upgrade authority

A deployed Solana program is mutable by default: whoever holds the upgrade authority can replace
the code, and therefore can drain every payment the program holds. Decide this before mainnet, not
after.

The three options, in the order they get less risky and less flexible:

| Policy | What it means | Suitable for |
|---|---|---|
| Single key | One keypair can upgrade at will | devnet only |
| Multisig | Upgrades need *n* of *m* signatures (e.g. Squads) | mainnet |
| Revoked | Nobody can upgrade, ever | mainnet, once stable |

**Devnet** may use a single key; nothing there is real.

**Mainnet** should not. A single upgrade key is a single point of compromise with the entire value
locked in the program behind it — and users have no way to tell the difference between "the team
upgraded" and "the key was stolen".

Revoking the authority is the strongest guarantee and is irreversible: a bug found afterwards can
only be handled by deploying a new program at a new address and migrating. Revoke only once the
program has been audited, has run on mainnet for a while, and nothing about the framework's use of
it is still changing.

Set the authority at deploy time:

```sh
solana program set-upgrade-authority <PROGRAM_ID> --new-upgrade-authority <MULTISIG_ADDRESS>
```

Revoke it, when that day comes:

```sh
solana program set-upgrade-authority <PROGRAM_ID> --final
```

Record whichever applies in the README's status table, and in KDF's own documentation, so an
integrator can see it without asking.

## Devnet

1. Point the CLI at devnet and fund the deploying key. A deploy costs roughly the rent for the
   binary's size, currently about 1.7 SOL for a 240 KB program:

   ```sh
   solana config set --url devnet
   solana airdrop 2
   solana balance
   ```

   Devnet airdrops are rate limited; if it refuses, use https://faucet.solana.com.

2. Deploy:

   ```sh
   ./scripts/deploy.sh devnet
   ```

   The script rebuilds, checks that `declare_id!` matches the keypair, and refuses to continue if
   they disagree — a mismatch there produces a program that cannot verify its own address at
   runtime.

3. Confirm what landed:

   ```sh
   solana program show --url devnet 54mDjQz6jS9sggCV1P5GoUzqHaBEueGVZqmaMPLt2tDx
   ```

   Check `Authority` is who you expect and `Last Deployed In Slot` is recent.

4. Update the README status table, and tell KDF: its devnet tests can then run against a real
   deployment rather than only a local validator.

## Mainnet

Do not deploy to mainnet until:

- [ ] the audit is complete and its findings are addressed;
- [ ] the upgrade authority is a multisig, not a single key;
- [ ] the program keypair is backed up in more than one place;
- [ ] `idl/satomic_swap.json` matches the audited commit (CI checks this);
- [ ] KDF's swap test suite passes against the audited build;
- [ ] someone other than the deployer has verified the build reproduces (below).

Then:

```sh
solana config set --url mainnet-beta
./scripts/deploy.sh mainnet-beta      # asks you to type the program id back
solana program set-upgrade-authority 54mDjQz6jS9sggCV1P5GoUzqHaBEueGVZqmaMPLt2tDx \
  --new-upgrade-authority <MULTISIG_ADDRESS>
```

## Verifiable builds

An auditor reads source; users get a binary. A verifiable build is what ties the two together, so
anyone can confirm the deployed bytes came from the audited commit rather than taking it on trust.

[`solana-verify`](https://github.com/Ellipsis-Labs/solana-verifiable-build) builds the program
inside a pinned container and compares the hash with what is on chain:

```sh
cargo install solana-verify

# what the deployed program hashes to
solana-verify get-program-hash --url mainnet-beta 54mDjQz6jS9sggCV1P5GoUzqHaBEueGVZqmaMPLt2tDx

# build from this repo and compare
solana-verify verify-from-repo \
  --url mainnet-beta \
  --program-id 54mDjQz6jS9sggCV1P5GoUzqHaBEueGVZqmaMPLt2tDx \
  https://github.com/GLEECBTC/satomic-swap
```

Do this **before** revoking the upgrade authority: if the hashes disagree, the only fix is to
redeploy.

Note that reproducibility depends on the pinned toolchain in `.github/workflows/ci.yml`
(`SOLANA_VERSION`, `ANCHOR_VERSION`). Bumping either changes the resulting binary, so a verified
build is only meaningful against the versions recorded at the time.

## Rollback

There is no rollback for a revoked authority. While one exists, redeploying an earlier binary is
an ordinary upgrade:

```sh
git checkout <last-good-commit>
./scripts/deploy.sh mainnet-beta
```

Upgrading does **not** touch existing payment accounts. Funds locked by the old code stay locked
under the same PDAs, so any redeploy has to keep the seeds and the account layout compatible or it
strands them. If a change cannot preserve those, it needs a new program id and a migration plan,
not an upgrade.
