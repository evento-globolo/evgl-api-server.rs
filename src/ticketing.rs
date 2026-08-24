use chrono::{DateTime, Utc};
use evgl_interfaces::{
    TICKETING_INVENTORY_MIGRATION,
    ticketing::{
        CancelTicketOrder, ConfigureEventInventory, ConfirmTicketPayment, CreateTicketClass,
        CreateTicketOrder, InventoryReceipt, JoinWaitlist, ReserveTickets,
    },
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, QueryResult, Statement,
};
use uuid::Uuid;

#[derive(Clone)]
pub struct TicketingService {
    database: DatabaseConnection,
}

impl TicketingService {
    pub fn new(database: DatabaseConnection) -> Self {
        Self { database }
    }

    pub fn database(&self) -> &DatabaseConnection {
        &self.database
    }

    pub async fn migrate(&self) -> Result<(), DbErr> {
        self.database
            .execute_unprepared(TICKETING_INVENTORY_MIGRATION)
            .await?;
        Ok(())
    }

    pub async fn configure_event(
        &self,
        input: &ConfigureEventInventory,
        now: DateTime<Utc>,
    ) -> Result<Uuid, DbErr> {
        input.validate().map_err(validation_error)?;
        let result = self
            .query_one(
                r#"with event_lock as materialized (
                        select pg_advisory_xact_lock(hashtextextended($1::text, 3464))
                    )
                    insert into event_inventory (event_id, capacity, created_at, updated_at)
                    select $1, $2, $3, $3 from event_lock
                    on conflict (event_id) do update
                    set capacity = excluded.capacity, updated_at = excluded.updated_at
                    where excluded.capacity >= (
                        coalesce((select sum(quantity) from ticket_holds
                                  where event_id = excluded.event_id
                                    and status = 'held' and expires_at > excluded.updated_at), 0)
                        + coalesce((select sum(quantity) from ticket_orders
                                    where event_id = excluded.event_id and status = 'paid'), 0)
                    ) returning event_id"#,
                vec![input.event_id.into(), input.capacity.into(), now.into()],
            )
            .await?;
        uuid_column(
            result,
            "event_id",
            "event capacity cannot be reduced below allocated stock",
        )
    }

    pub async fn create_ticket_class(
        &self,
        input: &CreateTicketClass,
        now: DateTime<Utc>,
    ) -> Result<Uuid, DbErr> {
        input.validate().map_err(validation_error)?;
        let result = self
            .query_one(
                r#"insert into ticket_classes (
                        event_id, name, capacity, sale_starts_at, sale_ends_at, created_at
                    ) select $1, $2, $3, $4, $5, $6
                      from event_inventory
                     where event_id = $1 and capacity >= $3
                    returning id"#,
                vec![
                    input.event_id.into(),
                    input.name.trim().to_owned().into(),
                    input.capacity.into(),
                    input.sale_starts_at.into(),
                    input.sale_ends_at.into(),
                    now.into(),
                ],
            )
            .await?;
        uuid_column(
            result,
            "id",
            "event inventory is missing or class capacity exceeds event capacity",
        )
    }

    pub async fn reserve(&self, input: &ReserveTickets, now: DateTime<Utc>) -> Result<Uuid, DbErr> {
        input.validate(now).map_err(validation_error)?;
        self.call_uuid(
            "select evgl_reserve_tickets($1, $2, $3, $4, $5, $6) as id",
            vec![
                input.event_id.into(),
                input.ticket_class_id.into(),
                input.quantity.into(),
                input.idempotency_key.clone().into(),
                now.into(),
                input.expires_at.into(),
            ],
        )
        .await
    }

    pub async fn create_order(
        &self,
        input: &CreateTicketOrder,
        now: DateTime<Utc>,
    ) -> Result<Uuid, DbErr> {
        input.validate().map_err(validation_error)?;
        self.call_uuid(
            "select evgl_create_ticket_order($1, $2, $3) as id",
            vec![
                input.hold_id.into(),
                input.checkout_idempotency_key.clone().into(),
                now.into(),
            ],
        )
        .await
    }

    pub async fn confirm_payment(
        &self,
        input: &ConfirmTicketPayment,
        now: DateTime<Utc>,
    ) -> Result<Uuid, DbErr> {
        input.validate().map_err(validation_error)?;
        self.call_uuid(
            "select evgl_confirm_ticket_payment($1, $2, $3) as id",
            vec![
                input.order_id.into(),
                input.payment_idempotency_key.clone().into(),
                now.into(),
            ],
        )
        .await
    }

    pub async fn cancel_order(
        &self,
        input: &CancelTicketOrder,
        now: DateTime<Utc>,
    ) -> Result<Uuid, DbErr> {
        input.validate().map_err(validation_error)?;
        self.call_uuid(
            "select evgl_cancel_ticket_order($1, $2, $3, $4) as id",
            vec![
                input.order_id.into(),
                input.cancellation_idempotency_key.clone().into(),
                input.refund.into(),
                now.into(),
            ],
        )
        .await
    }

    pub async fn expire_holds(&self, now: DateTime<Utc>) -> Result<i64, DbErr> {
        let result = self
            .query_one(
                "select evgl_expire_ticket_holds($1) as count",
                vec![now.into()],
            )
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("hold expiry result missing".into()))?;
        result.try_get("", "count")
    }

    pub async fn join_waitlist(
        &self,
        input: &JoinWaitlist,
        now: DateTime<Utc>,
    ) -> Result<Uuid, DbErr> {
        input.validate().map_err(validation_error)?;
        self.call_uuid(
            "select evgl_join_ticket_waitlist($1, $2, $3, $4, $5) as id",
            vec![
                input.event_id.into(),
                input.ticket_class_id.into(),
                input.attendee_ref_hash.clone().into(),
                input.quantity.into(),
                now.into(),
            ],
        )
        .await
    }

    pub async fn promote_waitlist(
        &self,
        event_id: Uuid,
        ticket_class_id: Uuid,
        now: DateTime<Utc>,
        offer_expires_at: DateTime<Utc>,
    ) -> Result<Option<Uuid>, DbErr> {
        let result = self
            .query_one(
                "select evgl_promote_ticket_waitlist($1, $2, $3, $4) as id",
                vec![
                    event_id.into(),
                    ticket_class_id.into(),
                    now.into(),
                    offer_expires_at.into(),
                ],
            )
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("waitlist promotion result missing".into()))?;
        result.try_get("", "id")
    }

    pub async fn inventory_receipt(
        &self,
        event_id: Uuid,
        now: DateTime<Utc>,
    ) -> Result<InventoryReceipt, DbErr> {
        let result = self
            .query_one(
                r#"select event_id, event_capacity, held, sold, remaining
                      from ticket_inventory_receipts where event_id = $1"#,
                vec![event_id.into()],
            )
            .await?
            .ok_or_else(|| DbErr::RecordNotFound("event inventory receipt missing".into()))?;
        Ok(InventoryReceipt {
            event_id: result.try_get("", "event_id")?,
            event_capacity: result.try_get("", "event_capacity")?,
            held: result.try_get("", "held")?,
            sold: result.try_get("", "sold")?,
            remaining: result.try_get("", "remaining")?,
            generated_at: now,
        })
    }

    async fn call_uuid(&self, sql: &str, values: Vec<sea_orm::Value>) -> Result<Uuid, DbErr> {
        let result = self.query_one(sql, values).await?;
        uuid_column(result, "id", "database function returned no row")
    }

    async fn query_one(
        &self,
        sql: &str,
        values: Vec<sea_orm::Value>,
    ) -> Result<Option<QueryResult>, DbErr> {
        self.database
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                sql,
                values,
            ))
            .await
    }
}

fn uuid_column(result: Option<QueryResult>, column: &str, missing: &str) -> Result<Uuid, DbErr> {
    result
        .ok_or_else(|| DbErr::RecordNotFound(missing.into()))?
        .try_get("", column)
}

fn validation_error(error: impl ToString) -> DbErr {
    DbErr::Custom(error.to_string())
}
