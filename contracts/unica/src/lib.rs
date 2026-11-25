#![no_std]

//! Unica records one successful token transfer for each payer and payment identifier.
//!
//! The contract does not hold funds. It authorizes the payer, asks the token
//! contract to transfer from that payer to the recipient, and stores a persistent
//! record of the successful payment.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, panic_with_error, token,
    Address, BytesN, Env,
};

/// Ledgers in one day at a 5-second close time.
///
/// See the Stellar storage guides:
/// <https://developers.stellar.org/docs/build/guides/storage/storage-strategies>
const DAY_IN_LEDGERS: u32 = 17_280;

/// Live lifetime requested for a payment record, about 120 days.
///
/// The network caps TTL extensions (about 180 days at the current close time).
/// `extend_ttl` does not restore an archived entry and does not make the record
/// permanent. An archived persistent entry remains on the ledger, but a
/// transaction cannot read it until restoration. Restoration is normally added
/// by simulation; a transaction that omits it fails before this contract runs.
/// A restored entry starts at the minimum TTL, so reads and writes below extend it.
const PAYMENT_TTL_EXTEND_TO: u32 = 120 * DAY_IN_LEDGERS;

/// Extend a payment record only when fewer than about 113 days remain.
const PAYMENT_TTL_THRESHOLD: u32 = PAYMENT_TTL_EXTEND_TO - (7 * DAY_IN_LEDGERS);

/// Live lifetime requested for this contract instance and its code, about 30 days.
const INSTANCE_TTL_EXTEND_TO: u32 = 30 * DAY_IN_LEDGERS;

/// Extend the instance only when fewer than about 29 days remain.
const INSTANCE_TTL_THRESHOLD: u32 = INSTANCE_TTL_EXTEND_TO - DAY_IN_LEDGERS;

/// Failures a caller can handle. Both abort the transaction.
#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    /// The amount is zero or negative.
    InvalidAmount = 1,
    /// This payer has already used this payment identifier.
    AlreadyProcessed = 2,
}

/// Persistent key for one payer's payment identifier.
///
/// The identifier is scoped to the payer. Another payer may use the same
/// identifier. The same payer may not reuse it with a different recipient,
/// token, or amount.
#[contracttype]
enum DataKey {
    Payment(Address, BytesN<32>),
}

/// Record of one successful payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentRecord {
    pub payment_id: BytesN<32>,
    pub payer: Address,
    pub recipient: Address,
    pub token: Address,
    pub amount: i128,
    pub timestamp: u64,
}

/// Published only after the token transfer returns and the record is stored.
///
/// A failed transaction rolls the event back with the storage write.
#[contractevent]
pub struct PaymentExecuted {
    #[topic]
    pub payer: Address,
    #[topic]
    pub payment_id: BytesN<32>,
    pub recipient: Address,
    pub token: Address,
    pub amount: i128,
    pub timestamp: u64,
}

#[contract]
pub struct Unica;

#[contractimpl]
impl Unica {
    /// Transfer `amount` of `token` from `payer` to `recipient` once.
    ///
    /// `payment_id` is unique per payer. The payer must authorize this call.
    /// The token contract performs the transfer and moves the funds directly;
    /// Unica does not take custody.
    ///
    /// # Panics
    ///
    /// * [`Error::InvalidAmount`] when `amount` is not positive.
    /// * [`Error::AlreadyProcessed`] when this payer already used `payment_id`.
    /// * When the payer's authorization is missing.
    /// * When the token contract rejects the transfer.
    ///
    /// # Returns
    ///
    /// The persistent record written for this payment.
    pub fn execute_payment(
        env: Env,
        payer: Address,
        recipient: Address,
        token: Address,
        amount: i128,
        payment_id: BytesN<32>,
    ) -> PaymentRecord {
        if amount <= 0 {
            panic_with_error!(&env, Error::InvalidAmount);
        }

        // Authorize this invocation before any write or token call. The token
        // contract also authorizes `payer`; requiring it here binds that
        // authorization to this call.
        payer.require_auth();

        let key = DataKey::Payment(payer.clone(), payment_id.clone());
        if env.storage().persistent().has(&key) {
            panic_with_error!(&env, Error::AlreadyProcessed);
        }

        let record = PaymentRecord {
            payment_id: payment_id.clone(),
            payer: payer.clone(),
            recipient: recipient.clone(),
            token: token.clone(),
            amount,
            timestamp: env.ledger().timestamp(),
        };

        // Store before the external call. Soroban rolls this write back if the
        // transfer fails. A re-entrant call with the same payer and identifier
        // observes the record and is rejected.
        env.storage().persistent().set(&key, &record);
        extend_payment_ttl(&env, &key);
        extend_instance_ttl(&env);

        token::TokenClient::new(&env, &token).transfer(&payer, &recipient, &amount);

        PaymentExecuted {
            payer,
            payment_id,
            recipient,
            token,
            amount,
            timestamp: record.timestamp,
        }
        .publish(&env);

        record
    }

    /// Return the payer's payment record, or `None` when it is absent.
    ///
    /// An archived record is not absent. The transaction restores it before this
    /// function runs, or the transaction fails closed.
    pub fn get_payment(env: Env, payer: Address, payment_id: BytesN<32>) -> Option<PaymentRecord> {
        let key = DataKey::Payment(payer, payment_id);
        let record = env.storage().persistent().get(&key)?;
        extend_payment_ttl(&env, &key);
        Some(record)
    }

    /// Return whether this payer has already used `payment_id`.
    ///
    /// `false` means no live record exists. An archived record fails the
    /// transaction until it is restored; it is not reported as unprocessed.
    pub fn is_processed(env: Env, payer: Address, payment_id: BytesN<32>) -> bool {
        let key = DataKey::Payment(payer, payment_id);
        if !env.storage().persistent().has(&key) {
            return false;
        }
        extend_payment_ttl(&env, &key);
        true
    }
}

fn extend_payment_ttl(env: &Env, key: &DataKey) {
    env.storage()
        .persistent()
        .extend_ttl(key, PAYMENT_TTL_THRESHOLD, PAYMENT_TTL_EXTEND_TO);
}

fn extend_instance_ttl(env: &Env) {
    env.storage()
        .instance()
        .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_TTL_EXTEND_TO);
}

#[cfg(test)]
mod test;
