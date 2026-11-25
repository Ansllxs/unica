#![cfg(test)]

extern crate std;

use super::{
    DataKey, Error, PaymentExecuted, PaymentRecord, Unica, UnicaClient, INSTANCE_TTL_EXTEND_TO,
    PAYMENT_TTL_EXTEND_TO, PAYMENT_TTL_THRESHOLD,
};
use soroban_sdk::{
    contract, contractimpl,
    testutils::{
        storage::{Instance as _, Persistent as _},
        Address as _, AuthorizedFunction, AuthorizedInvocation, EnvTestConfig, Events, Ledger,
        MockAuth, MockAuthInvoke,
    },
    token::{StellarAssetClient, TokenClient},
    xdr::{ScErrorCode, ScErrorType},
    Address, BytesN, ConversionError, Env, Event, IntoVal, InvokeError, MuxedAddress, Symbol, Val,
};

const STARTING_BALANCE: i128 = 1_000_000;

type PaymentAttempt =
    Result<Result<PaymentRecord, ConversionError>, Result<soroban_sdk::Error, InvokeError>>;

struct Fixture {
    env: Env,
    contract: Address,
    token: Address,
    admin: Address,
    payer: Address,
    recipient: Address,
}

fn test_env() -> Env {
    let mut env = Env::default();
    env.set_config(EnvTestConfig {
        capture_snapshot_at_drop: false,
    });
    env.ledger().set_timestamp(1_700_000_000);
    env
}

impl Fixture {
    fn new() -> Self {
        let env = test_env();
        let contract = env.register(Unica, ());
        let admin = Address::generate(&env);
        let token = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let payer = Address::generate(&env);
        let recipient = Address::generate(&env);
        let fixture = Self {
            env,
            contract,
            token,
            admin,
            payer,
            recipient,
        };
        fixture.mint(&fixture.payer, STARTING_BALANCE);
        fixture
    }

    fn client(&self) -> UnicaClient<'_> {
        UnicaClient::new(&self.env, &self.contract)
    }

    fn balances(&self) -> TokenClient<'_> {
        TokenClient::new(&self.env, &self.token)
    }

    fn mint(&self, to: &Address, amount: i128) {
        let invocation = MockAuthInvoke {
            contract: &self.token,
            fn_name: "mint",
            args: (to.clone(), amount).into_val(&self.env),
            sub_invokes: &[],
        };
        StellarAssetClient::new(&self.env, &self.token)
            .mock_auths(&[MockAuth {
                address: &self.admin,
                invoke: &invocation,
            }])
            .mint(to, &amount);
    }

    fn payment_id(&self, byte: u8) -> BytesN<32> {
        BytesN::from_array(&self.env, &[byte; 32])
    }

    fn execute(
        &self,
        payer: &Address,
        recipient: &Address,
        token: &Address,
        amount: i128,
        payment_id: &BytesN<32>,
    ) -> PaymentRecord {
        let transfer = transfer_auth(&self.env, payer, recipient, token, amount);
        let call = PaymentCall {
            contract: &self.contract,
            payer,
            recipient,
            token,
            amount,
            payment_id,
        };
        let root = payment_auth(&self.env, &call, std::slice::from_ref(&transfer));
        self.client()
            .mock_auths(&[MockAuth {
                address: payer,
                invoke: &root,
            }])
            .execute_payment(payer, recipient, token, &amount, payment_id)
    }

    fn try_execute(
        &self,
        payer: &Address,
        recipient: &Address,
        token: &Address,
        amount: i128,
        payment_id: &BytesN<32>,
    ) -> PaymentAttempt {
        let transfer = transfer_auth(&self.env, payer, recipient, token, amount);
        let call = PaymentCall {
            contract: &self.contract,
            payer,
            recipient,
            token,
            amount,
            payment_id,
        };
        let root = payment_auth(&self.env, &call, std::slice::from_ref(&transfer));
        self.client()
            .mock_auths(&[MockAuth {
                address: payer,
                invoke: &root,
            }])
            .try_execute_payment(payer, recipient, token, &amount, payment_id)
    }

    fn key(&self, payer: &Address, payment_id: &BytesN<32>) -> DataKey {
        DataKey::Payment(payer.clone(), payment_id.clone())
    }

    fn persistent_ttl(&self, payer: &Address, payment_id: &BytesN<32>) -> u32 {
        let key = self.key(payer, payment_id);
        self.env.as_contract(&self.contract, || {
            self.env.storage().persistent().get_ttl(&key)
        })
    }

    fn instance_ttl(&self) -> u32 {
        self.env
            .as_contract(&self.contract, || self.env.storage().instance().get_ttl())
    }
}

fn transfer_auth<'a>(
    env: &'a Env,
    payer: &'a Address,
    recipient: &'a Address,
    token: &'a Address,
    amount: i128,
) -> MockAuthInvoke<'a> {
    MockAuthInvoke {
        contract: token,
        fn_name: "transfer",
        args: (payer.clone(), MuxedAddress::from(recipient.clone()), amount).into_val(env),
        sub_invokes: &[],
    }
}

struct PaymentCall<'a> {
    contract: &'a Address,
    payer: &'a Address,
    recipient: &'a Address,
    token: &'a Address,
    amount: i128,
    payment_id: &'a BytesN<32>,
}

fn payment_auth<'a>(
    env: &'a Env,
    call: &PaymentCall<'a>,
    sub_invokes: &'a [MockAuthInvoke<'a>],
) -> MockAuthInvoke<'a> {
    MockAuthInvoke {
        contract: call.contract,
        fn_name: "execute_payment",
        args: (
            call.payer.clone(),
            call.recipient.clone(),
            call.token.clone(),
            call.amount,
            call.payment_id.clone(),
        )
            .into_val(env),
        sub_invokes,
    }
}

fn expect_unica_error(result: PaymentAttempt, expected: Error) {
    let invoke = match result {
        Err(Ok(error)) => InvokeError::from(error),
        Err(Err(invoke)) => invoke,
        Ok(value) => panic!("expected {expected:?}, got {value:?}"),
    };
    assert_eq!(
        Error::try_from(invoke).unwrap_or_else(|_| panic!("unexpected invoke error {invoke:?}")),
        expected
    );
}

fn expect_host_rejection(result: PaymentAttempt) {
    let invoke = match result {
        Err(Ok(error)) => InvokeError::from(error),
        Err(Err(invoke)) => invoke,
        Ok(value) => panic!("expected a host rejection, got {value:?}"),
    };
    assert!(
        Error::try_from(invoke).is_err(),
        "expected a token or authorization failure, got contract error {invoke:?}"
    );
}

fn expected_auth<'a>(
    env: &'a Env,
    contract: &'a Address,
    payer: &'a Address,
    recipient: &'a Address,
    token: &'a Address,
    amount: i128,
    payment_id: &'a BytesN<32>,
) -> AuthorizedInvocation {
    let transfer_args: soroban_sdk::Vec<Val> =
        (payer.clone(), MuxedAddress::from(recipient.clone()), amount).into_val(env);
    let payment_args: soroban_sdk::Vec<Val> = (
        payer.clone(),
        recipient.clone(),
        token.clone(),
        amount,
        payment_id.clone(),
    )
        .into_val(env);
    AuthorizedInvocation {
        function: AuthorizedFunction::Contract((
            contract.clone(),
            Symbol::new(env, "execute_payment"),
            payment_args,
        )),
        sub_invocations: std::vec![AuthorizedInvocation {
            function: AuthorizedFunction::Contract((
                token.clone(),
                Symbol::new(env, "transfer"),
                transfer_args,
            )),
            sub_invocations: std::vec![],
        }],
    }
}

#[test]
fn valid_payment_transfers_the_token_amount() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(1);
    let amount = 250;

    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        amount,
        &payment_id,
    );

    assert_eq!(
        fixture.balances().balance(&fixture.payer),
        STARTING_BALANCE - amount
    );
    assert_eq!(fixture.balances().balance(&fixture.recipient), amount);
}

#[test]
fn valid_payment_updates_payer_and_recipient_balances() {
    let fixture = Fixture::new();
    let first = fixture.payment_id(1);
    let second = fixture.payment_id(2);

    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        100,
        &first,
    );
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        40,
        &second,
    );

    assert_eq!(
        fixture.balances().balance(&fixture.payer),
        STARTING_BALANCE - 140
    );
    assert_eq!(fixture.balances().balance(&fixture.recipient), 140);
}

#[test]
fn valid_payment_stores_the_complete_record() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(7);
    let amount = 80;

    let returned = fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        amount,
        &payment_id,
    );
    let stored = fixture
        .client()
        .get_payment(&fixture.payer, &payment_id)
        .unwrap();
    let expected = PaymentRecord {
        payment_id: payment_id.clone(),
        payer: fixture.payer.clone(),
        recipient: fixture.recipient.clone(),
        token: fixture.token.clone(),
        amount,
        timestamp: 1_700_000_000,
    };

    assert_eq!(returned, expected);
    assert_eq!(stored, expected);
}

#[test]
fn valid_payment_is_marked_processed() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(3);

    assert!(!fixture.client().is_processed(&fixture.payer, &payment_id));
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        15,
        &payment_id,
    );
    assert!(fixture.client().is_processed(&fixture.payer, &payment_id));
}

#[test]
fn valid_payment_emits_execution_event() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(4);
    let amount = 33;

    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        amount,
        &payment_id,
    );

    let event = PaymentExecuted {
        payer: fixture.payer.clone(),
        payment_id,
        recipient: fixture.recipient.clone(),
        token: fixture.token.clone(),
        amount,
        timestamp: 1_700_000_000,
    };
    assert_eq!(
        fixture
            .env
            .events()
            .all()
            .filter_by_contract(&fixture.contract),
        [event.to_xdr(&fixture.env, &fixture.contract)]
    );
}

#[test]
fn same_payment_identifier_is_rejected_for_the_same_payer() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(5);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        20,
        &payment_id,
    );

    expect_unica_error(
        fixture.try_execute(
            &fixture.payer,
            &fixture.recipient,
            &fixture.token,
            20,
            &payment_id,
        ),
        Error::AlreadyProcessed,
    );
}

#[test]
fn rejected_duplicate_does_not_transfer_more_funds() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(6);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        20,
        &payment_id,
    );

    let _ = fixture.try_execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        20,
        &payment_id,
    );

    assert_eq!(
        fixture.balances().balance(&fixture.payer),
        STARTING_BALANCE - 20
    );
    assert_eq!(fixture.balances().balance(&fixture.recipient), 20);
}

#[test]
fn changed_amount_does_not_bypass_duplicate_protection() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(8);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        20,
        &payment_id,
    );

    expect_unica_error(
        fixture.try_execute(
            &fixture.payer,
            &fixture.recipient,
            &fixture.token,
            21,
            &payment_id,
        ),
        Error::AlreadyProcessed,
    );
    assert_eq!(fixture.balances().balance(&fixture.recipient), 20);
}

#[test]
fn changed_recipient_does_not_bypass_duplicate_protection() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(9);
    let other = Address::generate(&fixture.env);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        20,
        &payment_id,
    );

    expect_unica_error(
        fixture.try_execute(&fixture.payer, &other, &fixture.token, 20, &payment_id),
        Error::AlreadyProcessed,
    );
    assert_eq!(fixture.balances().balance(&other), 0);
    assert_eq!(fixture.balances().balance(&fixture.recipient), 20);
}

#[test]
fn changed_token_does_not_bypass_duplicate_protection() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(10);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        20,
        &payment_id,
    );

    let other_admin = Address::generate(&fixture.env);
    let other_token = fixture
        .env
        .register_stellar_asset_contract_v2(other_admin.clone())
        .address();
    let mint = MockAuthInvoke {
        contract: &other_token,
        fn_name: "mint",
        args: (fixture.payer.clone(), STARTING_BALANCE).into_val(&fixture.env),
        sub_invokes: &[],
    };
    StellarAssetClient::new(&fixture.env, &other_token)
        .mock_auths(&[MockAuth {
            address: &other_admin,
            invoke: &mint,
        }])
        .mint(&fixture.payer, &STARTING_BALANCE);

    expect_unica_error(
        fixture.try_execute(
            &fixture.payer,
            &fixture.recipient,
            &other_token,
            20,
            &payment_id,
        ),
        Error::AlreadyProcessed,
    );
    assert_eq!(
        TokenClient::new(&fixture.env, &other_token).balance(&fixture.payer),
        STARTING_BALANCE
    );
    assert_eq!(
        TokenClient::new(&fixture.env, &other_token).balance(&fixture.recipient),
        0
    );
}

#[test]
fn different_identifiers_from_the_same_payer_are_accepted() {
    let fixture = Fixture::new();
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        10,
        &fixture.payment_id(1),
    );
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        12,
        &fixture.payment_id(2),
    );

    assert!(fixture
        .client()
        .is_processed(&fixture.payer, &fixture.payment_id(1)));
    assert!(fixture
        .client()
        .is_processed(&fixture.payer, &fixture.payment_id(2)));
    assert_eq!(fixture.balances().balance(&fixture.recipient), 22);
}

#[test]
fn different_payers_can_use_the_same_identifier() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(11);
    let other_payer = Address::generate(&fixture.env);
    fixture.mint(&other_payer, 500);

    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        30,
        &payment_id,
    );
    fixture.execute(
        &other_payer,
        &fixture.recipient,
        &fixture.token,
        40,
        &payment_id,
    );

    assert!(fixture.client().is_processed(&fixture.payer, &payment_id));
    assert!(fixture.client().is_processed(&other_payer, &payment_id));
    assert_eq!(fixture.balances().balance(&fixture.recipient), 70);
    assert_eq!(fixture.balances().balance(&other_payer), 460);
}

#[test]
fn unauthorized_caller_cannot_spend_another_payers_funds() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(12);

    expect_host_rejection(fixture.client().try_execute_payment(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        &25,
        &payment_id,
    ));
    assert_eq!(fixture.balances().balance(&fixture.payer), STARTING_BALANCE);
    assert_eq!(fixture.balances().balance(&fixture.recipient), 0);
    assert!(!fixture.client().is_processed(&fixture.payer, &payment_id));
    assert!(fixture
        .client()
        .get_payment(&fixture.payer, &payment_id)
        .is_none());
}

#[test]
fn authorizing_a_different_address_cannot_spend_the_payers_funds() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(13);
    let attacker = Address::generate(&fixture.env);
    let transfer = transfer_auth(
        &fixture.env,
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        25,
    );
    let call = PaymentCall {
        contract: &fixture.contract,
        payer: &fixture.payer,
        recipient: &fixture.recipient,
        token: &fixture.token,
        amount: 25,
        payment_id: &payment_id,
    };
    let root = payment_auth(&fixture.env, &call, std::slice::from_ref(&transfer));

    expect_host_rejection(
        fixture
            .client()
            .mock_auths(&[MockAuth {
                address: &attacker,
                invoke: &root,
            }])
            .try_execute_payment(
                &fixture.payer,
                &fixture.recipient,
                &fixture.token,
                &25,
                &payment_id,
            ),
    );
    assert_eq!(fixture.balances().balance(&fixture.payer), STARTING_BALANCE);
    assert!(!fixture.client().is_processed(&fixture.payer, &payment_id));
}

#[test]
fn successful_payment_requires_the_payers_authorization() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(14);
    let amount = 18;

    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        amount,
        &payment_id,
    );

    assert_eq!(
        fixture.env.auths(),
        [(
            fixture.payer.clone(),
            expected_auth(
                &fixture.env,
                &fixture.contract,
                &fixture.payer,
                &fixture.recipient,
                &fixture.token,
                amount,
                &payment_id,
            ),
        )]
    );
}

#[test]
fn zero_amount_is_rejected() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(15);

    expect_unica_error(
        fixture.client().try_execute_payment(
            &fixture.payer,
            &fixture.recipient,
            &fixture.token,
            &0,
            &payment_id,
        ),
        Error::InvalidAmount,
    );
    assert!(!fixture.client().is_processed(&fixture.payer, &payment_id));
    assert_eq!(fixture.balances().balance(&fixture.payer), STARTING_BALANCE);
}

#[test]
fn negative_amount_is_rejected() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(16);

    expect_unica_error(
        fixture.client().try_execute_payment(
            &fixture.payer,
            &fixture.recipient,
            &fixture.token,
            &-1,
            &payment_id,
        ),
        Error::InvalidAmount,
    );
    assert!(!fixture.client().is_processed(&fixture.payer, &payment_id));
    assert_eq!(fixture.balances().balance(&fixture.recipient), 0);
}

#[test]
fn insufficient_funds_are_rejected() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(17);
    let broke = Address::generate(&fixture.env);
    fixture.mint(&broke, 5);

    expect_host_rejection(fixture.try_execute(
        &broke,
        &fixture.recipient,
        &fixture.token,
        6,
        &payment_id,
    ));
    assert_eq!(fixture.balances().balance(&broke), 5);
    assert_eq!(fixture.balances().balance(&fixture.recipient), 0);
    assert!(!fixture.client().is_processed(&broke, &payment_id));
}

#[test]
fn failed_transfer_does_not_store_a_payment_record() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(18);
    let broke = Address::generate(&fixture.env);

    let _ = fixture.try_execute(&broke, &fixture.recipient, &fixture.token, 1, &payment_id);

    assert!(fixture.client().get_payment(&broke, &payment_id).is_none());
}

#[test]
fn failed_payment_is_not_marked_processed() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(19);

    let _ = fixture.try_execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        STARTING_BALANCE + 1,
        &payment_id,
    );

    assert!(!fixture.client().is_processed(&fixture.payer, &payment_id));
}

#[test]
fn failed_payment_does_not_emit_a_success_event() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(20);

    let _ = fixture.try_execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        STARTING_BALANCE + 1,
        &payment_id,
    );

    assert!(fixture
        .env
        .events()
        .all()
        .filter_by_contract(&fixture.contract)
        .events()
        .is_empty());
}

#[test]
fn failed_identifier_can_be_reused_after_the_failure_is_resolved() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(21);
    let broke = Address::generate(&fixture.env);
    fixture.mint(&broke, 5);

    expect_host_rejection(fixture.try_execute(
        &broke,
        &fixture.recipient,
        &fixture.token,
        10,
        &payment_id,
    ));
    fixture.mint(&broke, 10);

    let record = fixture.execute(&broke, &fixture.recipient, &fixture.token, 10, &payment_id);
    assert_eq!(record.amount, 10);
    assert_eq!(fixture.balances().balance(&broke), 5);
    assert_eq!(fixture.balances().balance(&fixture.recipient), 10);
    assert!(fixture.client().is_processed(&broke, &payment_id));
}

#[test]
fn missing_payment_returns_none_and_false() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(22);

    assert!(fixture
        .client()
        .get_payment(&fixture.payer, &payment_id)
        .is_none());
    assert!(!fixture.client().is_processed(&fixture.payer, &payment_id));
}

#[test]
fn rejected_payment_does_not_change_an_existing_record() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(23);
    let other = Address::generate(&fixture.env);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        40,
        &payment_id,
    );
    let before = fixture
        .client()
        .get_payment(&fixture.payer, &payment_id)
        .unwrap();
    let events_before = fixture
        .env
        .events()
        .all()
        .filter_by_contract(&fixture.contract)
        .events()
        .len();

    expect_unica_error(
        fixture.try_execute(&fixture.payer, &other, &fixture.token, 90, &payment_id),
        Error::AlreadyProcessed,
    );

    assert_eq!(
        fixture
            .client()
            .get_payment(&fixture.payer, &payment_id)
            .unwrap(),
        before
    );
    assert_eq!(fixture.balances().balance(&fixture.recipient), 40);
    assert_eq!(fixture.balances().balance(&other), 0);
    assert_eq!(
        fixture
            .env
            .events()
            .all()
            .filter_by_contract(&fixture.contract)
            .events()
            .len(),
        events_before
    );
}

#[test]
fn successful_payment_uses_persistent_storage_and_extends_ttl() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(24);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        5,
        &payment_id,
    );

    let key = fixture.key(&fixture.payer, &payment_id);
    fixture.env.as_contract(&fixture.contract, || {
        assert!(fixture.env.storage().persistent().has(&key));
        assert!(!fixture.env.storage().temporary().has(&key));
        assert_eq!(
            fixture.env.storage().persistent().get_ttl(&key),
            PAYMENT_TTL_EXTEND_TO
        );
        assert_eq!(
            fixture.env.storage().instance().get_ttl(),
            INSTANCE_TTL_EXTEND_TO
        );
    });
}

#[test]
fn read_within_the_ttl_threshold_does_not_extend_storage() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(25);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        5,
        &payment_id,
    );
    let sequence = fixture.env.ledger().sequence();
    fixture.env.ledger().set_sequence_number(sequence + 10);

    let _ = fixture.client().get_payment(&fixture.payer, &payment_id);
    assert!(fixture.client().is_processed(&fixture.payer, &payment_id));

    assert_eq!(
        fixture.persistent_ttl(&fixture.payer, &payment_id),
        PAYMENT_TTL_EXTEND_TO - 10
    );
    assert_eq!(fixture.instance_ttl(), INSTANCE_TTL_EXTEND_TO - 10);
}

#[test]
fn read_below_the_ttl_threshold_extends_the_payment_record() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(26);
    fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        5,
        &payment_id,
    );
    let sequence = fixture.env.ledger().sequence();
    let advance = PAYMENT_TTL_EXTEND_TO - PAYMENT_TTL_THRESHOLD + 1;
    fixture.env.ledger().set_sequence_number(sequence + advance);

    let _ = fixture.client().get_payment(&fixture.payer, &payment_id);

    assert_eq!(
        fixture.persistent_ttl(&fixture.payer, &payment_id),
        PAYMENT_TTL_EXTEND_TO
    );
}

/// SDK 30 restores an archived persistent entry before the test invocation.
/// A transaction that omits that restoration cannot be reproduced here. See
/// `soroban_sdk::_migrating::v23_archived_testing`.
#[test]
fn archived_payment_remains_a_duplicate_after_automatic_restoration() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(27);
    let original = fixture.execute(
        &fixture.payer,
        &fixture.recipient,
        &fixture.token,
        9,
        &payment_id,
    );
    let sequence = fixture.env.ledger().sequence();
    fixture
        .env
        .ledger()
        .set_sequence_number(sequence + PAYMENT_TTL_EXTEND_TO + 1);

    let restored = fixture
        .client()
        .get_payment(&fixture.payer, &payment_id)
        .unwrap();
    assert_eq!(restored, original);
    assert!(fixture.client().is_processed(&fixture.payer, &payment_id));
    expect_unica_error(
        fixture.try_execute(
            &fixture.payer,
            &fixture.recipient,
            &fixture.token,
            9,
            &payment_id,
        ),
        Error::AlreadyProcessed,
    );
    assert_eq!(fixture.balances().balance(&fixture.recipient), 9);
    // The restored entry is below the extension threshold, so the successful
    // read extends it again. Extension is still capped by the network maximum.
    assert_eq!(
        fixture.persistent_ttl(&fixture.payer, &payment_id),
        PAYMENT_TTL_EXTEND_TO
    );
}

#[test]
fn payer_can_transfer_to_self() {
    let fixture = Fixture::new();
    let payment_id = fixture.payment_id(28);

    let record = fixture.execute(
        &fixture.payer,
        &fixture.payer,
        &fixture.token,
        60,
        &payment_id,
    );

    assert_eq!(record.payer, record.recipient);
    assert_eq!(fixture.balances().balance(&fixture.payer), STARTING_BALANCE);
    assert!(fixture.client().is_processed(&fixture.payer, &payment_id));
    expect_unica_error(
        fixture.try_execute(
            &fixture.payer,
            &fixture.payer,
            &fixture.token,
            60,
            &payment_id,
        ),
        Error::AlreadyProcessed,
    );
    assert_eq!(fixture.balances().balance(&fixture.payer), STARTING_BALANCE);
}

/// Token that tries to call Unica again while `transfer` is still running.
///
/// The current Soroban host rejects that re-entry before the inner
/// `execute_payment` runs. This token records the host error and still returns
/// success, which is the case where a callback could otherwise create a second
/// payment if re-entry were permitted.
#[contract]
struct ReentrantToken;

#[contractimpl]
impl ReentrantToken {
    pub fn init(env: Env, unica: Address, recipient: Address, payment_id: BytesN<32>) {
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "unica"), &unica);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "to"), &recipient);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "id"), &payment_id);
    }

    pub fn transfer(env: Env, from: Address, _to: MuxedAddress, amount: i128) {
        from.require_auth();
        let unica: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "unica"))
            .unwrap();
        let recipient: Address = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "to"))
            .unwrap();
        let payment_id: BytesN<32> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "id"))
            .unwrap();
        let status = callback_status(UnicaClient::new(&env, &unica).try_execute_payment(
            &from,
            &recipient,
            &env.current_contract_address(),
            &amount,
            &payment_id,
        ));
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "status"), &status);
    }
}

fn callback_status(result: PaymentAttempt) -> (u32, u32) {
    match result {
        Ok(_) => (0, 0),
        Err(Ok(error)) => (error_type(error), error.get_code()),
        Err(Err(_)) => (u32::MAX, 0),
    }
}

fn error_type(error: soroban_sdk::Error) -> u32 {
    const TYPES: [ScErrorType; 10] = [
        ScErrorType::Contract,
        ScErrorType::WasmVm,
        ScErrorType::Context,
        ScErrorType::Storage,
        ScErrorType::Object,
        ScErrorType::Crypto,
        ScErrorType::Events,
        ScErrorType::Budget,
        ScErrorType::Value,
        ScErrorType::Auth,
    ];
    TYPES
        .into_iter()
        .find(|ty| error.is_type(*ty))
        .map(|ty| ty as u32)
        .unwrap_or(u32::MAX)
}

#[test]
fn reentrant_callback_cannot_record_a_second_payment() {
    let env = test_env();
    let contract = env.register(Unica, ());
    let token = env.register(ReentrantToken, ());
    let payer = Address::generate(&env);
    let recipient = Address::generate(&env);
    let payment_id = BytesN::from_array(&env, &[99; 32]);
    let amount = 10_i128;

    ReentrantTokenClient::new(&env, &token).init(&contract, &recipient, &payment_id);

    let transfer = transfer_auth(&env, &payer, &recipient, &token, amount);
    let call = PaymentCall {
        contract: &contract,
        payer: &payer,
        recipient: &recipient,
        token: &token,
        amount,
        payment_id: &payment_id,
    };
    let root = payment_auth(&env, &call, std::slice::from_ref(&transfer));
    let record = UnicaClient::new(&env, &contract)
        .mock_auths(&[MockAuth {
            address: &payer,
            invoke: &root,
        }])
        .execute_payment(&payer, &recipient, &token, &amount, &payment_id);

    let event = PaymentExecuted {
        payer: payer.clone(),
        payment_id: payment_id.clone(),
        recipient: recipient.clone(),
        token: token.clone(),
        amount,
        timestamp: 1_700_000_000,
    };
    assert_eq!(record.amount, amount);
    assert_eq!(record.token, token);
    assert_eq!(
        env.events().all().filter_by_contract(&contract),
        [event.to_xdr(&env, &contract)]
    );
    assert_eq!(
        env.auths(),
        [(
            payer.clone(),
            expected_auth(
                &env,
                &contract,
                &payer,
                &recipient,
                &token,
                amount,
                &payment_id,
            ),
        )]
    );

    let client = UnicaClient::new(&env, &contract);
    assert!(client.is_processed(&payer, &payment_id));
    assert_eq!(
        client.get_payment(&payer, &payment_id).unwrap().amount,
        amount
    );
    let status: (u32, u32) = env.as_contract(&token, || {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "status"))
            .unwrap()
    });
    // The host rejects the callback with Context/InvalidAction ("Contract
    // re-entry is not allowed") before Unica's duplicate check can run.
    // SDK 30 does not expose a way to permit re-entry in unit tests, so the
    // store-before-transfer guard cannot be reached through this callback.
    assert_eq!(
        status,
        (
            ScErrorType::Context as u32,
            ScErrorCode::InvalidAction as u32,
        )
    );
}
