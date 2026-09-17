# pisa

Async payjoin with cross-input signature aggregation.

A BIP 77 payjoin between two parties whose witness version 2 inputs form one
BIP 460 full-aggregation group, so the transaction carries one 64-byte
signature for both. No message is added to BIP 77. The sender's original
PSBT carries its public nonce, the receiver's proposal carries the receiver's
nonce and partial signature, and the sender finishes the signature before it
broadcasts.

This is a prototype. It runs on regtest against a build of Bitcoin Core that
validates BIP 460, and it depends on a branch of the payjoin crate that lets
the aggregation fields through. Nothing here is ready for real coins.

## What it does

`cargo run --bin pisa-regtest` starts a private regtest node, funds a sender
and a receiver wallet with witness version 2 coins, runs a payjoin between
them through an in-process payjoin directory and OHTTP relay, mines the
result, and prints it. The transaction has two inputs. One carries an empty
witness, the other a 65-byte witness with the aggregate signature and the
BIP 460 marker.

The same run is the end-to-end test:

```sh
BITCOIND_EXE=/path/to/bitcoind cargo test --test regtest -- --include-ignored
```

## Requirements

- Rust 1.85 or later.
- A Bitcoin Core built from the `bip460` branch of
  [fjahr/bitcoin](https://github.com/fjahr/bitcoin/tree/bip460). The commit
  this prototype was run against is pinned in `.github/workflows/ci.yml`.
  Release builds of Bitcoin Core treat witness version 2 as anyone-can-spend
  and have no `cisa()` descriptor, so they cannot take part.

```sh
git clone https://github.com/fjahr/bitcoin -b bip460
cmake -S bitcoin -B bitcoin/build -DBUILD_TESTS=OFF -DBUILD_GUI=OFF
cmake --build bitcoin/build -j"$(nproc)"
export BITCOIND_EXE="$PWD/bitcoin/build/bin/bitcoind"
```

The payjoin crate comes from a git submodule, so clone with
`--recurse-submodules` or run `git submodule update --init` afterwards.

## How the pieces fit

The aggregation itself lives in the wallet. Bitcoin Core's branch reserves a
nonce for one of its own witness version 2 outputs before the spending
transaction exists (`reservecisanonce`), signs an input that carries such a
nonce in one `walletprocesspsbt` call, and aggregates the group once every
partial signature is present. This crate never sees a secret nonce.

What this crate does is decide which inputs join the group and carry the
PSBT fields of the draft "CISA Fields for PSBT" between the two parties:

- `src/wallet.rs` defines the seam, the `AggregatingWallet` trait, and
  implements it over Bitcoin Core's RPC.
- `src/sender.rs` declares the sender's inputs for aggregation on the signed
  fallback PSBT before it leaves.
- `src/receiver.rs` contributes the receiver's coins as members of the
  group, each with a nonce reserved on the spot.
- `src/regtest.rs` drives the BIP 77 session end to end.

The payjoin crate had to change in four places for this to work; see
`docs/bip77-full-aggregation.md` for the protocol profile and for what the
implementation changed about it.

## License

The code is licensed under either the MIT license or the Apache License 2.0,
at your option. The documents under `docs/` are dedicated to the public
domain under CC0 1.0.
