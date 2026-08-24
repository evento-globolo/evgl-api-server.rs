use chrono::{Duration, Utc};
use ed25519_dalek::SigningKey;
use evgl_api::admission::{
    AdmissionService, AdmissionTokenSigner, DEFAULT_MAX_TOKEN_LIFETIME, ScannerReceiptSigner,
    verify_admission_token,
};
use evgl_api::ticketing::TicketingService;
use evgl_interfaces::admission::{
    ADMISSION_TOKEN_VERSION, AdmissionKeySnapshot, AdmissionOutcome, AdmissionTokenClaims,
    ScanReceiptClaims,
};
use evgl_interfaces::ticketing::{
    CancelTicketOrder, ConfigureEventInventory, ConfirmTicketPayment, CreateTicketClass,
    CreateTicketOrder, JoinWaitlist, ReserveTickets,
};
use sea_orm::{
    ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection, DbErr,
    Statement,
};
use std::env;
use uuid::Uuid;

struct TestDatabase {
    admin: DatabaseConnection,
    scoped: DatabaseConnection,
    schema: String,
    service: TicketingService,
}

impl TestDatabase {
    async fn create() -> Result<Self, DbErr> {
        let url = env::var("EVGL_TEST_DATABASE_URL").map_err(|_| {
            DbErr::Custom(
                "EVGL_TEST_DATABASE_URL is required for PostgreSQL integration tests".into(),
            )
        })?;
        let admin = Database::connect(&url).await?;
        let schema = format!("evgl_test_{}", Uuid::new_v4().simple());
        admin
            .execute_unprepared(&format!("create schema \"{schema}\""))
            .await?;

        let mut options = ConnectOptions::new(url);
        options.set_schema_search_path(format!("{schema},public"));
        let scoped = Database::connect(options).await?;
        let service = TicketingService::new(scoped.clone());
        service.migrate().await?;

        Ok(Self {
            admin,
            scoped,
            schema,
            service,
        })
    }

    async fn scalar_i64(&self, sql: &str) -> Result<i64, DbErr> {
        let row = self
            .scoped
            .query_one_raw(Statement::from_string(DatabaseBackend::Postgres, sql))
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("count query returned no row".into()))?;
        row.try_get("", "count")
    }

    async fn cleanup(self) -> Result<(), DbErr> {
        self.scoped.close().await?;
        self.admin
            .execute_unprepared(&format!("drop schema \"{}\" cascade", self.schema))
            .await?;
        self.admin.close().await
    }
}

async fn class(
    test: &TestDatabase,
    event_id: Uuid,
    name: &str,
    capacity: i32,
) -> Result<Uuid, DbErr> {
    let now = Utc::now();
    test.service
        .create_ticket_class(
            &CreateTicketClass {
                event_id,
                name: name.into(),
                capacity,
                sale_starts_at: now - Duration::hours(1),
                sale_ends_at: now + Duration::hours(1),
            },
            now,
        )
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_workers_never_exceed_class_or_event_capacity() -> Result<(), DbErr> {
    let test = TestDatabase::create().await?;
    let now = Utc::now();
    let event_id = Uuid::new_v4();
    test.service
        .configure_event(
            &ConfigureEventInventory {
                event_id,
                capacity: 10,
            },
            now,
        )
        .await?;
    let class_a = class(&test, event_id, "general", 7).await?;
    let class_b = class(&test, event_id, "balcony", 7).await?;

    let mut workers = Vec::new();
    for worker in 0..50 {
        let service = test.service.clone();
        workers.push(tokio::spawn(async move {
            service
                .reserve(
                    &ReserveTickets {
                        event_id,
                        ticket_class_id: if worker % 2 == 0 { class_a } else { class_b },
                        quantity: 1,
                        idempotency_key: format!("worker-{worker}"),
                        expires_at: now + Duration::minutes(5),
                    },
                    now,
                )
                .await
        }));
    }

    let mut accepted = 0;
    for worker in workers {
        if worker.await.expect("reservation worker panicked").is_ok() {
            accepted += 1;
        }
    }

    assert_eq!(accepted, 10);
    let receipt = test.service.inventory_receipt(event_id, now).await?;
    assert_eq!(receipt.held, 10);
    assert_eq!(receipt.sold, 0);
    assert_eq!(receipt.remaining, 0);
    assert!(
        test.scalar_i64(&format!(
            "select count(*) as count from ticket_holds where ticket_class_id = '{class_a}'"
        ))
        .await?
            <= 7
    );
    assert!(
        test.scalar_i64(&format!(
            "select count(*) as count from ticket_holds where ticket_class_id = '{class_b}'"
        ))
        .await?
            <= 7
    );

    test.cleanup().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn sellout_retry_timeout_refund_and_fair_promotion_are_auditable() -> Result<(), DbErr> {
    let test = TestDatabase::create().await?;
    let now = Utc::now();
    let event_id = Uuid::new_v4();
    test.service
        .configure_event(
            &ConfigureEventInventory {
                event_id,
                capacity: 1,
            },
            now,
        )
        .await?;
    let class_id = class(&test, event_id, "single-seat", 1).await?;
    let reservation = ReserveTickets {
        event_id,
        ticket_class_id: class_id,
        quantity: 1,
        idempotency_key: "canary-reserve".into(),
        expires_at: now + Duration::minutes(5),
    };
    let hold_id = test.service.reserve(&reservation, now).await?;
    assert_eq!(test.service.reserve(&reservation, now).await?, hold_id);

    let order_id = test
        .service
        .create_order(
            &CreateTicketOrder {
                hold_id,
                checkout_idempotency_key: "canary-checkout".into(),
            },
            now,
        )
        .await?;
    let mut callbacks = Vec::new();
    for _ in 0..20 {
        let service = test.service.clone();
        callbacks.push(tokio::spawn(async move {
            service
                .confirm_payment(
                    &ConfirmTicketPayment {
                        order_id,
                        payment_idempotency_key: "canary-payment".into(),
                    },
                    now,
                )
                .await
        }));
    }
    for callback in callbacks {
        assert_eq!(callback.await.expect("payment worker panicked")?, order_id);
    }

    let first_waitlist_id = test
        .service
        .join_waitlist(
            &JoinWaitlist {
                event_id,
                ticket_class_id: class_id,
                attendee_ref_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                quantity: 1,
            },
            now,
        )
        .await?;
    test.service
        .join_waitlist(
            &JoinWaitlist {
                event_id,
                ticket_class_id: class_id,
                attendee_ref_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                quantity: 1,
            },
            now,
        )
        .await?;

    let cancellation = CancelTicketOrder {
        order_id,
        cancellation_idempotency_key: "canary-refund".into(),
        refund: true,
    };
    assert_eq!(
        test.service.cancel_order(&cancellation, now).await?,
        order_id
    );
    assert_eq!(
        test.service.cancel_order(&cancellation, now).await?,
        order_id
    );
    assert!(
        test.service
            .promote_waitlist(
                event_id,
                class_id,
                now + Duration::seconds(1),
                now + Duration::minutes(3),
            )
            .await?
            .is_some()
    );

    assert_eq!(
        test.scalar_i64("select count(*) as count from ticket_orders")
            .await?,
        1
    );
    assert_eq!(
        test.scalar_i64("select count(*) as count from ticket_order_history")
            .await?,
        3
    );
    assert_eq!(
        test.scalar_i64(&format!(
            "select count(*) as count from ticket_waitlist where id = '{first_waitlist_id}' and status = 'offered'"
        ))
        .await?,
        1
    );

    let timeout_event = Uuid::new_v4();
    test.service
        .configure_event(
            &ConfigureEventInventory {
                event_id: timeout_event,
                capacity: 1,
            },
            now,
        )
        .await?;
    let timeout_class = class(&test, timeout_event, "timeout", 1).await?;
    test.service
        .reserve(
            &ReserveTickets {
                event_id: timeout_event,
                ticket_class_id: timeout_class,
                quantity: 1,
                idempotency_key: "timeout-reserve".into(),
                expires_at: now + Duration::seconds(1),
            },
            now,
        )
        .await?;
    assert_eq!(
        test.service
            .expire_holds(now + Duration::seconds(2))
            .await?,
        1
    );
    assert_eq!(
        test.service
            .expire_holds(now + Duration::seconds(2))
            .await?,
        0
    );
    assert_eq!(
        test.scalar_i64(
            "select count(*) as count from ticket_inventory_ledger where event_type = 'hold_expired'"
        )
        .await?,
        1
    );

    test.cleanup().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn offline_scans_reconcile_once_and_preserve_every_signed_receipt() -> Result<(), DbErr> {
    let test = TestDatabase::create().await?;
    let admission = AdmissionService::new(test.scoped.clone());
    admission
        .migrate()
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let now = Utc::now();
    let event_id = Uuid::new_v4();
    test.service
        .configure_event(
            &ConfigureEventInventory {
                event_id,
                capacity: 1,
            },
            now,
        )
        .await?;
    let class_id = class(&test, event_id, "admission", 1).await?;
    let hold_id = test
        .service
        .reserve(
            &ReserveTickets {
                event_id,
                ticket_class_id: class_id,
                quantity: 1,
                idempotency_key: "admission-reserve".into(),
                expires_at: now + Duration::minutes(5),
            },
            now,
        )
        .await?;
    let order_id = test
        .service
        .create_order(
            &CreateTicketOrder {
                hold_id,
                checkout_idempotency_key: "admission-checkout".into(),
            },
            now,
        )
        .await?;
    test.service
        .confirm_payment(
            &ConfirmTicketPayment {
                order_id,
                payment_idempotency_key: "admission-payment".into(),
            },
            now,
        )
        .await?;

    let token_signer = AdmissionTokenSigner::new(
        SigningKey::from_bytes(&[11; 32]),
        "admission-key-1",
        1,
        now - Duration::minutes(1),
        now + Duration::hours(1),
        DEFAULT_MAX_TOKEN_LIFETIME,
    )
    .map_err(|error| DbErr::Custom(error.to_string()))?;
    admission
        .register_signing_key(&token_signer.verification_key())
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let ticket_id = Uuid::new_v4();
    admission
        .register_entitlement(ticket_id, order_id, event_id, 1, now)
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let token_claims = AdmissionTokenClaims {
        version: ADMISSION_TOKEN_VERSION,
        token_id: Uuid::new_v4(),
        event_id,
        ticket_id,
        order_id,
        issuance_epoch: 1,
        key_id: "admission-key-1".into(),
        issued_at: now,
        expires_at: now + Duration::minutes(10),
    };
    let compact = token_signer
        .sign(&token_claims)
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let snapshot = AdmissionKeySnapshot {
        snapshot_id: Uuid::new_v4(),
        event_id,
        generated_at: now - Duration::seconds(1),
        valid_until: now + Duration::minutes(30),
        keys: vec![token_signer.verification_key()],
        revocations: Vec::new(),
    };
    assert_eq!(
        verify_admission_token(&compact, &snapshot, event_id, now)
            .map_err(|error| DbErr::Custom(error.to_string()))?,
        token_claims
    );
    admission
        .register_token(&token_claims)
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;

    let first_scanner_id = Uuid::from_u128(1);
    let second_scanner_id = Uuid::from_u128(2);
    let first_scanner = ScannerReceiptSigner::new(
        first_scanner_id,
        "scanner-key-1",
        SigningKey::from_bytes(&[21; 32]),
    );
    let second_scanner = ScannerReceiptSigner::new(
        second_scanner_id,
        "scanner-key-2",
        SigningKey::from_bytes(&[22; 32]),
    );
    admission
        .register_scanner_key(
            first_scanner_id,
            first_scanner.key_id(),
            &first_scanner.public_key(),
            now - Duration::minutes(1),
            now + Duration::hours(1),
        )
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    admission
        .register_scanner_key(
            second_scanner_id,
            second_scanner.key_id(),
            &second_scanner.public_key(),
            now - Duration::minutes(1),
            now + Duration::hours(1),
        )
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;

    let first_receipt = first_scanner
        .sign(ScanReceiptClaims {
            receipt_id: Uuid::from_u128(10),
            scanner_id: first_scanner_id,
            scanner_key_id: first_scanner.key_id().into(),
            scanner_sequence: 1,
            token_id: token_claims.token_id,
            event_id,
            ticket_id,
            order_id,
            scanned_at: now + Duration::seconds(1),
        })
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let second_receipt = second_scanner
        .sign(ScanReceiptClaims {
            receipt_id: Uuid::from_u128(20),
            scanner_id: second_scanner_id,
            scanner_key_id: second_scanner.key_id().into(),
            scanner_sequence: 1,
            token_id: token_claims.token_id,
            event_id,
            ticket_id,
            order_id,
            scanned_at: now + Duration::seconds(1),
        })
        .map_err(|error| DbErr::Custom(error.to_string()))?;

    admission
        .record_receipt(&second_receipt, now + Duration::seconds(3))
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    admission
        .record_receipt(&first_receipt, now + Duration::seconds(4))
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    assert_eq!(
        admission
            .record_receipt(&second_receipt, now + Duration::seconds(5))
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))?,
        second_receipt.claims.receipt_id
    );

    let outcomes = admission
        .outcomes(ticket_id)
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    assert_eq!(outcomes.len(), 2);
    assert_eq!(
        outcomes
            .iter()
            .filter(|outcome| outcome.outcome == AdmissionOutcome::Accepted)
            .count(),
        1
    );
    assert!(
        outcomes
            .iter()
            .all(|outcome| { outcome.winning_receipt_id == Some(first_receipt.claims.receipt_id) })
    );

    let sequence_conflict = second_scanner
        .sign(ScanReceiptClaims {
            receipt_id: Uuid::from_u128(21),
            scanner_id: second_scanner_id,
            scanner_key_id: second_scanner.key_id().into(),
            scanner_sequence: 1,
            token_id: token_claims.token_id,
            event_id,
            ticket_id,
            order_id,
            scanned_at: now + Duration::seconds(2),
        })
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    assert!(
        admission
            .record_receipt(&sequence_conflict, now + Duration::seconds(6))
            .await
            .is_err()
    );
    assert_eq!(
        test.scalar_i64("select count(*) as count from admission_scan_receipts")
            .await?,
        2
    );

    let later_sequence = second_scanner
        .sign(ScanReceiptClaims {
            receipt_id: Uuid::from_u128(30),
            scanner_id: second_scanner_id,
            scanner_key_id: second_scanner.key_id().into(),
            scanner_sequence: 3,
            token_id: token_claims.token_id,
            event_id,
            ticket_id,
            order_id,
            scanned_at: now + Duration::seconds(3),
        })
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let delayed_sequence = second_scanner
        .sign(ScanReceiptClaims {
            receipt_id: Uuid::from_u128(29),
            scanner_id: second_scanner_id,
            scanner_key_id: second_scanner.key_id().into(),
            scanner_sequence: 2,
            token_id: token_claims.token_id,
            event_id,
            ticket_id,
            order_id,
            scanned_at: now + Duration::seconds(2),
        })
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    admission
        .record_receipt(&later_sequence, now + Duration::seconds(7))
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    admission
        .record_receipt(&delayed_sequence, now + Duration::seconds(8))
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    assert_eq!(
        test.scalar_i64("select count(*) as count from admission_scan_receipts")
            .await?,
        4
    );
    assert_eq!(
        test.scalar_i64(
            "select count(*) as count from admission_scan_receipts where sequence_out_of_order"
        )
        .await?,
        1
    );

    admission
        .revoke_entitlement(ticket_id, now + Duration::seconds(9))
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    let revoked_outcomes = admission
        .outcomes(ticket_id)
        .await
        .map_err(|error| DbErr::Custom(error.to_string()))?;
    assert!(
        revoked_outcomes
            .iter()
            .all(|outcome| outcome.outcome == AdmissionOutcome::Rejected)
    );
    assert_eq!(revoked_outcomes.len(), 4);

    test.cleanup().await
}
