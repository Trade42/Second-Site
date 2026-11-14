//! The one opt-in test against the real Capital One Nessie v2 sandbox. Ignored by default; it
//! runs only when `NESSIE_API_KEY` is set:
//!
//! ```text
//! NESSIE_API_KEY=nsk_... orchestrator/scripts/cargo.sh test -p ss-nessie --test live_sandbox -- --ignored --nocapture
//! ```
//!
//! It creates one clearly named test customer, one checking account with $100.00 of mock money,
//! one stand-in merchant and one $1.23 purchase (through the approval gate), settles it, and then
//! deletes the account, the customer and the merchant (transactions cannot be deleted; deleting
//! the account cancels anything still pending). Cleanup runs even if an assertion fails.

mod common;

use chrono::Utc;
use common::approved;
use ss_core::IntegrationStatus;
use ss_domain::integrations::base::ProviderErrorKind;
use ss_nessie::approval::PaymentOperation;
use ss_nessie::config::{ApiKey, NessieConfig, ProviderMode, ENV_API_KEY, ENV_BASE_URL};
use ss_nessie::models::*;
use ss_nessie::{
    ApprovalAction, LiveNessieBank, NessieBank, PaymentAuthorization, PaymentIntent,
    SimulatedOutcome, Simulation,
};
use uuid::Uuid;

struct Cleanup<'a> {
    bank: &'a LiveNessieBank,
    account: Option<String>,
    customer: Option<String>,
    merchant: Option<String>,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        if let Some(a) = self.account.take() {
            eprintln!(
                "cleanup: delete account {a}: {:?}",
                self.bank.delete_account(&a).map_err(|e| e.to_string())
            );
        }
        if let Some(c) = self.customer.take() {
            eprintln!(
                "cleanup: delete customer {c}: {:?}",
                self.bank.delete_customer(&c).map_err(|e| e.to_string())
            );
        }
        if let Some(m) = self.merchant.take() {
            eprintln!(
                "cleanup: delete merchant {m}: {:?}",
                self.bank.delete_merchant(&m).map_err(|e| e.to_string())
            );
        }
    }
}

#[test]
#[ignore = "talks to the real Nessie sandbox; needs NESSIE_API_KEY"]
fn live_sandbox_round_trip() {
    let Some(key) = std::env::var(ENV_API_KEY).ok().and_then(ApiKey::new) else {
        eprintln!("NESSIE_API_KEY is not set: the live sandbox test did not run");
        return;
    };
    let secret = key.expose().to_owned();
    let config = NessieConfig {
        mode: ProviderMode::Live,
        api_key: Some(key),
        base_url: std::env::var(ENV_BASE_URL)
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| ss_nessie::DEFAULT_BASE_URL.to_owned()),
        ..NessieConfig::default()
    };
    let bank = LiveNessieBank::from_config(&config).expect("live config");
    let no_key = |s: String| {
        assert!(!s.contains(&secret), "key leaked into: {s}");
        s
    };
    assert_eq!(bank.status().status, IntegrationStatus::Partial);

    let h = bank.health().expect("health");
    eprintln!(
        "health: status={} service={} version={}",
        h.status, h.service, h.version
    );

    let mut cleanup = Cleanup {
        bank: &bank,
        account: None,
        customer: None,
        merchant: None,
    };
    let customer = bank
        .create_customer(&CustomerCreate {
            first_name: "SecondSite".into(),
            last_name: "Automated Test (delete me)".into(),
            address: Address {
                street_number: "1500".into(),
                street_name: "Main St".into(),
                city: "McLean".into(),
                state: "VA".into(),
                zip: "22101".into(),
            },
        })
        .unwrap_or_else(|e| panic!("create customer: {}", no_key(e.to_string())));
    cleanup.customer = Some(customer.record.id.clone());
    eprintln!("created customer {}", customer.record.id);
    assert_eq!(customer.simulation, Simulation::NessieSandbox);

    let account = bank
        .create_account(
            &customer.record.id,
            &AccountCreate {
                account_type: AccountType::Checking,
                nickname: "SS test (delete me)".into(),
                balance_cents: 10_000,
                credit_limit_cents: None,
                rewards: None,
            },
        )
        .unwrap_or_else(|e| panic!("create account: {}", no_key(e.to_string())));
    cleanup.account = Some(account.record.id.clone());
    eprintln!(
        "created account {} type={} balance_cents={} available_cents={}",
        account.record.id,
        account.record.account_type,
        account.record.balance_cents,
        account.record.available_cents
    );
    assert_eq!(account.record.balance_cents, 10_000);
    assert_eq!(account.record.available_cents, 10_000);
    let st = bank.status();
    assert_eq!(st.status, IntegrationStatus::Live, "{}", st.note);
    eprintln!(
        "status after first authenticated success: {} / {}",
        st.status,
        no_key(st.note.clone())
    );

    let merchant = bank
        .create_merchant(&MerchantCreate {
            name: "SIMULATED TEST MERCHANT (Second Site, delete me)".into(),
            category: Some("test".into()),
            address: None,
            geocode: None,
        })
        .unwrap_or_else(|e| panic!("create merchant: {}", no_key(e.to_string())));
    cleanup.merchant = Some(merchant.record.id.clone());
    eprintln!("created merchant {}", merchant.record.id);

    let intent = PaymentIntent {
        simulation: Simulation::NessieSandbox,
        project_id: Uuid::new_v4(),
        property_id: Uuid::new_v4(),
        scenario_id: Uuid::new_v4(),
        plan_item_id: "test:live_sandbox".into(),
        action: ApprovalAction::Purchase,
        operation: PaymentOperation::Purchase {
            account_id: account.record.id.clone(),
            merchant_id: merchant.record.id.clone(),
        },
        amount_minor: 123,
        currency: "USD".into(),
        description: "SIMULATION Second Site automated test purchase (delete me)".into(),
    };
    let auth = PaymentAuthorization::verify(&approved(&intent), intent.clone(), Utc::now())
        .expect("authorization");
    let rec = bank
        .execute(auth)
        .unwrap_or_else(|e| panic!("purchase: {}", no_key(e.to_string())));
    eprintln!(
        "purchase {:?}: outcome={} provider_status={} note={}",
        rec.nessie_transaction_id,
        rec.outcome.as_str(),
        rec.provider_status,
        rec.note
    );
    assert!(matches!(
        rec.outcome,
        SimulatedOutcome::SimulatedPending | SimulatedOutcome::SimulatedCompleted
    ));
    let tx_id = rec.nessie_transaction_id.clone().expect("transaction id");

    let after = bank.get_account(&account.record.id).expect("account");
    eprintln!(
        "account after purchase: balance_cents={} pending_debits_cents={:?} available_cents={}",
        after.record.balance_cents, after.record.pending_debits_cents, after.record.available_cents
    );
    assert_eq!(after.record.available_cents, 10_000 - 123);

    let settled = bank
        .settle_transaction(&tx_id)
        .unwrap_or_else(|e| panic!("settle: {}", no_key(e.to_string())));
    let refreshed = rec.refreshed(&settled).expect("refresh");
    eprintln!(
        "after settle: provider_status={} outcome={}",
        refreshed.provider_status,
        refreshed.outcome.as_str()
    );
    assert!(matches!(
        refreshed.outcome,
        SimulatedOutcome::SimulatedPending | SimulatedOutcome::SimulatedCompleted
    ));
    let listed = bank
        .list_account_transactions(&account.record.id, 10)
        .expect("list");
    assert!(listed.record.iter().any(|t| t.id == tx_id));

    let account_id = account.record.id.clone();
    drop(cleanup);
    let gone = bank.get_account(&account_id).unwrap_err();
    eprintln!("account after cleanup: {}", no_key(gone.to_string()));
    assert_eq!(gone.kind(), Some(ProviderErrorKind::NotFound));
}
