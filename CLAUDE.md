# pisa — working rules for agents

## Verification

Before committing:

```sh
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The regtest round trip is the real gate. It needs a Bitcoin Core build that
validates BIP 460 (see README.md) and runs with:

```sh
BITCOIND_EXE=/path/to/bitcoind cargo test --test regtest -- --include-ignored
```

Report anything you could not run as unverified. Never claim a CI result you
did not see.

## Constraints

- The payjoin crate comes from the git submodule under `vendor/`. Changes to
  payjoin itself go there, on its own branch, with their own tests.
- Every use of the CISA PSBT fields goes through `payjoin::cisa`. Do not
  spell the field numbers anywhere else.
- Nonce handling follows BIP 459: never derive a nonce deterministically,
  never persist a secret nonce, never sign twice with one reservation.
- Comments explain why, for a reader who has never seen the change.
