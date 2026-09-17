//! Async payjoin with BIP 460 full signature aggregation.
//!
//! A BIP 77 payjoin already has the two messages that BIP 459 full
//! aggregation needs. The sender's original PSBT carries its public nonce,
//! the receiver's proposal carries the receiver's nonce and partial
//! signature, and the sender finishes the signature before it broadcasts.
//! No message is added.
//!
//! The aggregation itself lives in the wallet. This crate only decides which
//! inputs join the group, moves the PSBT fields of the draft "CISA Fields for
//! PSBT" between the parties, and drives the BIP 77 session. See
//! [`wallet::AggregatingWallet`] for the contract a wallet has to meet.

pub mod receiver;
pub mod regtest;
pub mod sender;
pub mod wallet;
