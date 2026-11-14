//! The models against the Nessie documentation: verbatim doc examples, schema-constructed
//! bodies, and our own request bodies validated against the published request schemas.

mod common;

use common::{assert_nessie_conforms, nessie_conforms, nessie_golden};
use serde_json::{json, Value};
use ss_nessie::models::*;
use ss_nessie::plan::{stand_in_merchant, PayeeRole};
use ss_nessie::SimulatedOutcome;

fn verbatim(name: &str) -> Value {
    nessie_golden("doc_examples.json")["verbatim"][name]["body"].clone()
}

fn constructed(name: &str) -> Value {
    nessie_golden("doc_examples.json")["constructed"][name].clone()
}

fn address() -> Address {
    serde_json::from_value(verbatim("address")).unwrap()
}

#[test]
fn golden_files_record_their_source() {
    let s = nessie_golden("schemas.json");
    assert_eq!(s["_meta"]["api_version"], "2.1.3");
    assert_eq!(s["_meta"]["source"], "https://v2.nessieisreal.com/openapi.json");
    assert_eq!(
        s["_meta"]["security_schemes"]["bearerAuth"]["scheme"],
        "bearer"
    );
    let ops = s["operations"].as_object().unwrap();
    assert_eq!(ops["create-purchase"]["path"], "/accounts/{id}/purchases");
    assert_eq!(ops["pay-bill"]["path"], "/bills/{id}/payments");
    assert_eq!(ops["create-bill-for-account"]["request_schema"], "BillCreateDTO");
    assert!(nessie_golden("doc_examples.json")["_meta"]["source"]
        .as_str()
        .unwrap()
        .contains("49bf003e"));
}

#[test]
fn verbatim_account_excerpt_parses() {
    let a: Account = serde_json::from_value(verbatim("account_credit_card_excerpt")).unwrap();
    assert_eq!(a.id, "acct_01J8...");
    assert_eq!(a.account_type, AccountType::CreditCard);
    assert_eq!(a.balance_cents, 4250);
    assert_eq!(a.pending_debits_cents, Some(0));
    assert_eq!(a.available_cents, 95750);
    assert_eq!(a.credit_limit_cents, Some(100000));
    assert_eq!(a.customer_id, None);
    // The guide's excerpt is not schema-complete; the schema check has teeth.
    assert!(!nessie_conforms("AccountDTO", &verbatim("account_credit_card_excerpt")).is_empty());
}

#[test]
fn verbatim_request_examples_round_trip() {
    let p: PurchaseCreate = serde_json::from_value(verbatim("purchase_request")).unwrap();
    assert_eq!(p.amount_cents, 1299);
    assert_eq!(p.description.as_deref(), Some("coffee"));
    assert_eq!(serde_json::to_value(&p).unwrap(), verbatim("purchase_request"));
    assert_nessie_conforms("PurchaseCreateDTO", &verbatim("purchase_request"));

    for name in ["transfer_request", "card_payment_transfer_request"] {
        let t: TransferCreate = serde_json::from_value(verbatim(name)).unwrap();
        assert_eq!(serde_json::to_value(&t).unwrap(), verbatim(name));
        assert_nessie_conforms("TransferCreateDTO", &verbatim(name));
        // The doc placeholders are not ids: validation refuses to put them in a request.
        assert!(t.validate().is_err());
    }
}

#[test]
fn verbatim_events_problem_health_address() {
    let e: EventPage = serde_json::from_value(verbatim("events_page")).unwrap();
    assert_eq!(e.items.len(), 1);
    assert_eq!(e.items[0].seq, 1);
    assert_eq!(e.items[0].event_type, "transaction.created");
    assert_eq!(e.next_cursor, Some(1));
    assert_nessie_conforms("EventsListResponseBody", &verbatim("events_page"));

    let p: Problem = serde_json::from_value(verbatim("problem_insufficient_funds")).unwrap();
    assert_eq!(p.slug(), "insufficient_funds");
    assert_eq!(p.status, Some(422));
    assert_nessie_conforms("ErrorModel", &verbatim("problem_insufficient_funds"));

    let h: Health = serde_json::from_value(verbatim("health")).unwrap();
    assert_eq!((h.status.as_str(), h.version.as_str()), ("healthy", "2.0.0"));
    assert_nessie_conforms("HealthDTO", &verbatim("health"));

    assert!(address().validate().is_ok());
    assert_nessie_conforms("AddressDTO", &verbatim("address"));
    let g: Geocode = serde_json::from_value(verbatim("geocode")).unwrap();
    assert!(g.validate().is_ok());
    assert_nessie_conforms("GeocodeDTO", &verbatim("geocode"));
}

#[test]
fn constructed_responses_conform_and_parse() {
    for (body, schema) in [
        ("CustomerDTO", "CustomerDTO"),
        ("AccountDTO", "AccountDTO"),
        ("MerchantDTO", "MerchantDTO"),
        ("TransactionDTO_purchase_pending", "TransactionDTO"),
        ("TransactionDTO_bill_payment_completed", "TransactionDTO"),
        ("BillDTO_recurring", "BillDTO"),
        ("ListResponseAccountDTOBody", "ListResponseAccountDTOBody"),
        ("TransactionListResponseBody_null_items", "TransactionListResponseBody"),
    ] {
        assert_nessie_conforms(schema, &constructed(body));
    }
    let c: Customer = serde_json::from_value(constructed("CustomerDTO")).unwrap();
    assert_eq!(c.id, "cust_example_1");
    let a: Account = serde_json::from_value(constructed("AccountDTO")).unwrap();
    assert_eq!(a.available_cents, a.balance_cents - a.pending_debits_cents.unwrap());
    let m: Merchant = serde_json::from_value(constructed("MerchantDTO")).unwrap();
    assert_eq!(m.geocode.unwrap().lat, 38.9339);
    let t: Transaction =
        serde_json::from_value(constructed("TransactionDTO_purchase_pending")).unwrap();
    assert_eq!(t.status, TransactionStatus::Pending);
    assert_eq!(t.kind, TransactionKind::Purchase);
    assert_eq!(t.medium, Some(Medium::Balance));
    let t: Transaction =
        serde_json::from_value(constructed("TransactionDTO_bill_payment_completed")).unwrap();
    assert_eq!(t.kind, TransactionKind::BillPayment);
    assert_eq!(t.bill_id.as_deref(), Some("bill_example_1"));
    let b: Bill = serde_json::from_value(constructed("BillDTO_recurring")).unwrap();
    assert_eq!(b.status, BillStatus::Recurring);
    let amt = b.payment_amount_minor().unwrap();
    assert_eq!((amt.minor, amt.exact), (690000, true));
    let page: Page<Account> =
        serde_json::from_value(constructed("ListResponseAccountDTOBody")).unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.next_cursor.as_deref(), Some("opaque-cursor-1"));
    let l: TransactionList =
        serde_json::from_value(constructed("TransactionListResponseBody_null_items")).unwrap();
    assert!(l.items.is_empty());
}

#[test]
fn responses_tolerate_unknown_fields_and_values() {
    let mut a = constructed("AccountDTO");
    a["brand_new_field"] = json!({"x": 1});
    a["type"] = json!("Money Market");
    let a: Account = serde_json::from_value(a).unwrap();
    assert_eq!(a.account_type, AccountType::Unknown("Money Market".into()));
    assert_eq!(a.account_type.as_str(), "Money Market");

    let mut t = constructed("TransactionDTO_purchase_pending");
    t["status"] = json!("reversed");
    t["kind"] = json!("crypto_swap");
    t["future"] = json!([1, 2]);
    let t: Transaction = serde_json::from_value(t).unwrap();
    assert_eq!(t.status, TransactionStatus::Unknown("reversed".into()));
    assert_eq!(t.kind, TransactionKind::Unknown("crypto_swap".into()));
    // Unknown stays UNKNOWN: never read as completed.
    assert_eq!(
        SimulatedOutcome::from_transaction_status(&t.status),
        SimulatedOutcome::Unknown
    );
    assert_eq!(serde_json::to_value(&t).unwrap()["status"], "reversed");

    // Missing required money fields are an error, not a default.
    let mut t = constructed("TransactionDTO_purchase_pending");
    t.as_object_mut().unwrap().remove("amount_cents");
    assert!(serde_json::from_value::<Transaction>(t).is_err());
    let mut a = constructed("AccountDTO");
    a.as_object_mut().unwrap().remove("available_cents");
    assert!(serde_json::from_value::<Account>(a).is_err());
}

#[test]
fn request_bodies_conform_to_the_published_request_schemas() {
    let customer = CustomerCreate {
        first_name: "Simulated".into(),
        last_name: "Owner".into(),
        address: address(),
    };
    customer.validate().unwrap();
    assert_nessie_conforms("CustomerCreateDTO", &serde_json::to_value(&customer).unwrap());

    let checking = AccountCreate {
        account_type: AccountType::Checking,
        nickname: "Second Site simulation".into(),
        balance_cents: 25_000_000,
        credit_limit_cents: None,
        rewards: None,
    };
    checking.validate().unwrap();
    let v = serde_json::to_value(&checking).unwrap();
    assert_eq!(v, json!({"type": "Checking", "nickname": "Second Site simulation", "balance_cents": 25000000}));
    assert_nessie_conforms("AccountCreateDTO", &v);
    let card = AccountCreate {
        account_type: AccountType::CreditCard,
        nickname: "Card".into(),
        balance_cents: 0,
        credit_limit_cents: Some(500_000),
        rewards: Some(0),
    };
    card.validate().unwrap();
    assert_nessie_conforms("AccountCreateDTO", &serde_json::to_value(&card).unwrap());

    for role in [
        PayeeRole::Landlord,
        PayeeRole::Contractor,
        PayeeRole::FurnishingSupplier,
    ] {
        let m = stand_in_merchant(role);
        m.validate().unwrap();
        assert!(m.name.starts_with("SIMULATED "));
        assert_nessie_conforms("MerchantCreateDTO", &serde_json::to_value(&m).unwrap());
    }
    let full = MerchantCreate {
        name: "M".into(),
        category: Some("c".into()),
        address: Some(address()),
        geocode: Some(Geocode {
            lat: 38.9339,
            lng: -77.1773,
        }),
    };
    full.validate().unwrap();
    assert_nessie_conforms("MerchantCreateDTO", &serde_json::to_value(&full).unwrap());

    let bodies = [
        (
            "PurchaseCreateDTO",
            serde_json::to_value(PurchaseCreate {
                merchant_id: "merch_1".into(),
                amount_cents: 1,
                description: None,
            })
            .unwrap(),
        ),
        (
            "DepositCreateDTO",
            serde_json::to_value(DepositCreate {
                amount_cents: 100,
                description: Some("SIMULATION funding".into()),
            })
            .unwrap(),
        ),
        (
            "WithdrawalCreateDTO",
            serde_json::to_value(WithdrawalCreate {
                amount_cents: 100,
                description: None,
            })
            .unwrap(),
        ),
        (
            "BillPaymentCreateDTO",
            serde_json::to_value(BillPaymentCreate {
                amount_cents: Some(690000),
            })
            .unwrap(),
        ),
    ];
    for (schema, body) in bodies {
        assert_nessie_conforms(schema, &body);
    }

    let bill = BillCreate::new_minor(
        BillStatus::Recurring,
        "SIMULATED LANDLORD".into(),
        Some("rent".into()),
        690_050,
        Some(1),
    )
    .unwrap();
    bill.validate().unwrap();
    let v = serde_json::to_value(&bill).unwrap();
    assert_eq!(serde_json::to_string(&v["payment_amount"]).unwrap(), "6900.5");
    assert_eq!(v["status"], "recurring");
    assert_nessie_conforms("BillCreateDTO", &v);

    // The schema rejects a body with an extra field: our exact bodies matter.
    let mut extra = serde_json::to_value(&checking).unwrap();
    extra["key"] = json!("nsk_x");
    assert!(!nessie_conforms("AccountCreateDTO", &extra).is_empty());
}

#[test]
fn request_validation_applies_documented_limits() {
    let mut c = CustomerCreate {
        first_name: String::new(),
        last_name: "x".into(),
        address: address(),
    };
    assert!(c.validate().is_err());
    c.first_name = "é".repeat(80);
    assert!(c.validate().is_ok(), "80 characters (not bytes) is allowed");
    c.first_name.push('é');
    assert!(c.validate().is_err());
    c.first_name = "A".into();
    for (state, zip) in [("Virginia", "22101"), ("VA", "2210"), ("VA", "22a01"), ("VA", "221011")] {
        c.address.state = state.into();
        c.address.zip = zip.into();
        assert!(c.validate().is_err(), "{state} {zip}");
    }

    let mut a = AccountCreate {
        account_type: AccountType::Checking,
        nickname: "x".repeat(41),
        balance_cents: 0,
        credit_limit_cents: None,
        rewards: None,
    };
    assert!(a.validate().is_err());
    a.nickname = "ok".into();
    a.balance_cents = -1;
    assert!(a.validate().is_err());
    a.balance_cents = 0;
    a.credit_limit_cents = Some(100);
    assert!(a.validate().is_err(), "credit limit is for cards only");
    a.account_type = AccountType::CreditCard;
    a.credit_limit_cents = Some(0);
    assert!(a.validate().is_err());
    a.account_type = AccountType::Unknown("Brokerage".into());
    a.credit_limit_cents = None;
    assert!(a.validate().is_err());

    for id in ["", "a/b", "a?key=1", "a b", "../x", "acct_01J8..."] {
        let p = PurchaseCreate {
            merchant_id: id.into(),
            amount_cents: 5,
            description: None,
        };
        assert!(p.validate().is_err(), "{id:?}");
    }
    let mut p = PurchaseCreate {
        merchant_id: "merch_1".into(),
        amount_cents: 0,
        description: None,
    };
    assert!(p.validate().is_err());
    p.amount_cents = 1;
    p.description = Some("d".repeat(201));
    assert!(p.validate().is_err());
    p.description = Some("d".repeat(200));
    assert!(p.validate().is_ok());

    let mut b = BillCreate::new_minor(BillStatus::Recurring, "p".into(), None, 1, Some(31)).unwrap();
    assert!(b.validate().is_ok());
    b.recurring_date = Some(32);
    assert!(b.validate().is_err());
    b.recurring_date = Some(1);
    b.payment_amount = f64::NAN;
    assert!(b.validate().is_err());
    b.payment_amount = -0.01;
    assert!(b.validate().is_err());
    b.payment_amount = 1.0;
    b.status = BillStatus::Unknown("overdue".into());
    assert!(b.validate().is_err());
    assert!(BillCreate::new_minor(BillStatus::Recurring, "p".into(), None, -1, None).is_err());
    assert!(BillPaymentCreate { amount_cents: Some(0) }.validate().is_err());
}

#[test]
fn bill_amounts_convert_with_visible_rounding() {
    let mut b: Bill = serde_json::from_value(constructed("BillDTO_recurring")).unwrap();
    b.payment_amount = 10.005;
    let c = b.payment_amount_minor().unwrap();
    assert_eq!((c.minor, c.exact), (1001, false));
    b.payment_amount = -5.0;
    assert!(b.payment_amount_minor().is_err());
}
