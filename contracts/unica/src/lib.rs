#![no_std]

use soroban_sdk::{contract, contractimpl};

/// Unica payment contract.
///
/// Payment logic is intentionally absent in this scaffold.
#[contract]
pub struct Unica;

#[contractimpl]
impl Unica {
    /// Build-verification entry point. Always returns `1`.
    pub fn ping() -> u32 {
        1
    }
}
