use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use evgl_interfaces::{
    ADMISSION_MIGRATION,
    admission::{
        AdmissionKeySnapshot, AdmissionOutcome, AdmissionReceiptOutcome, AdmissionTokenClaims,
        AdmissionVerificationKey, ScanReceiptClaims, SignedScanReceipt,
    },
};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, DbErr, QueryResult, Statement,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt};
use uuid::Uuid;

pub const DEFAULT_MAX_TOKEN_LIFETIME: Duration = Duration::hours(12);

pub struct AdmissionTokenSigner {
    signing_key: SigningKey,
    descriptor: AdmissionVerificationKey,
    max_lifetime: Duration,
}

impl AdmissionTokenSigner {
    pub fn new(
        signing_key: SigningKey,
        key_id: impl Into<String>,
        issuance_epoch: i64,
        active_from: DateTime<Utc>,
        retire_at: DateTime<Utc>,
        max_lifetime: Duration,
    ) -> Result<Self, AdmissionError> {
        let descriptor = AdmissionVerificationKey {
            key_id: key_id.into(),
            issuance_epoch,
            public_key: URL_SAFE_NO_PAD.encode(signing_key.verifying_key().as_bytes()),
            active_from,
            retire_at,
        };
        validate_descriptor(&descriptor)?;
        if max_lifetime <= Duration::zero() {
            return Err(AdmissionError::Policy(
                "maximum token lifetime must be positive".into(),
            ));
        }
        Ok(Self {
            signing_key,
            descriptor,
            max_lifetime,
        })
    }

    pub fn verification_key(&self) -> AdmissionVerificationKey {
        self.descriptor.clone()
    }

    pub fn sign(&self, claims: &AdmissionTokenClaims) -> Result<String, AdmissionError> {
        claims
            .validate_shape()
            .map_err(|error| AdmissionError::Policy(error.to_string()))?;
        if claims.key_id != self.descriptor.key_id
            || claims.issuance_epoch != self.descriptor.issuance_epoch
        {
            return Err(AdmissionError::Policy(
                "token key identity does not match signer".into(),
            ));
        }
        if claims.issued_at < self.descriptor.active_from
            || claims.expires_at > self.descriptor.retire_at
            || claims.expires_at - claims.issued_at > self.max_lifetime
        {
            return Err(AdmissionError::Policy(
                "token falls outside the signer or offline lifetime window".into(),
            ));
        }

        let payload = canonical_json(claims)?;
        let signature = self.signing_key.sign(&payload);
        Ok(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(signature.to_bytes())
        ))
    }
}

pub fn verify_admission_token(
    compact: &str,
    snapshot: &AdmissionKeySnapshot,
    expected_event_id: Uuid,
    now: DateTime<Utc>,
) -> Result<AdmissionTokenClaims, AdmissionError> {
    snapshot
        .validate_shape()
        .map_err(|error| AdmissionError::Policy(error.to_string()))?;
    if snapshot.event_id != expected_event_id {
        return Err(AdmissionError::WrongEvent);
    }
    if now < snapshot.generated_at || now >= snapshot.valid_until {
        return Err(AdmissionError::StaleSnapshot);
    }

    let (payload, signature) = decode_compact(compact)?;
    let claims: AdmissionTokenClaims =
        serde_json::from_slice(&payload).map_err(|_| AdmissionError::Malformed)?;
    claims
        .validate_shape()
        .map_err(|_| AdmissionError::Malformed)?;
    if claims.event_id != expected_event_id || claims.event_id != snapshot.event_id {
        return Err(AdmissionError::WrongEvent);
    }
    if now < claims.issued_at || now >= claims.expires_at {
        return Err(AdmissionError::Expired);
    }
    if claims.expires_at > snapshot.valid_until {
        return Err(AdmissionError::Policy(
            "token outlives the scanner snapshot".into(),
        ));
    }

    let descriptor = snapshot
        .keys
        .iter()
        .find(|key| key.key_id == claims.key_id && key.issuance_epoch == claims.issuance_epoch)
        .ok_or(AdmissionError::UnknownKey)?;
    validate_descriptor(descriptor)?;
    if claims.issued_at < descriptor.active_from || claims.expires_at > descriptor.retire_at {
        return Err(AdmissionError::RetiredKey);
    }
    let verifying_key = decode_verifying_key(&descriptor.public_key)?;
    verifying_key
        .verify(&payload, &signature)
        .map_err(|_| AdmissionError::InvalidSignature)?;

    if snapshot.revocations.iter().any(|revocation| {
        revocation.ticket_id == claims.ticket_id
            && revocation.issuance_epoch >= claims.issuance_epoch
            && revocation.revoked_at <= now
    }) {
        return Err(AdmissionError::Revoked);
    }
    Ok(claims)
}

pub struct ScannerReceiptSigner {
    scanner_id: Uuid,
    key_id: String,
    signing_key: SigningKey,
}

impl ScannerReceiptSigner {
    pub fn new(scanner_id: Uuid, key_id: impl Into<String>, signing_key: SigningKey) -> Self {
        Self {
            scanner_id,
            key_id: key_id.into(),
            signing_key,
        }
    }

    pub fn key_id(&self) -> &str {
        &self.key_id
    }

    pub fn public_key(&self) -> String {
        URL_SAFE_NO_PAD.encode(self.signing_key.verifying_key().as_bytes())
    }

    pub fn sign(&self, claims: ScanReceiptClaims) -> Result<SignedScanReceipt, AdmissionError> {
        claims
            .validate_shape()
            .map_err(|error| AdmissionError::Policy(error.to_string()))?;
        if claims.scanner_id != self.scanner_id || claims.scanner_key_id != self.key_id {
            return Err(AdmissionError::Policy(
                "receipt scanner or key identity does not match signer".into(),
            ));
        }
        let payload = canonical_json(&claims)?;
        Ok(SignedScanReceipt {
            claims,
            signature: URL_SAFE_NO_PAD.encode(self.signing_key.sign(&payload).to_bytes()),
        })
    }
}

pub fn verify_scan_receipt(
    receipt: &SignedScanReceipt,
    public_key: &str,
) -> Result<(), AdmissionError> {
    receipt
        .claims
        .validate_shape()
        .map_err(|_| AdmissionError::Malformed)?;
    let verifying_key = decode_verifying_key(public_key)?;
    let signature = decode_signature(&receipt.signature)?;
    verifying_key
        .verify(&canonical_json(&receipt.claims)?, &signature)
        .map_err(|_| AdmissionError::InvalidSignature)
}

/// Apply the deterministic winner rule after every receipt has passed
/// `verify_scan_receipt` and token/entitlement validation.
pub fn reconcile_verified_receipts(receipts: &[SignedScanReceipt]) -> Vec<AdmissionReceiptOutcome> {
    let mut winners = BTreeMap::<Uuid, &SignedScanReceipt>::new();
    for receipt in receipts {
        winners
            .entry(receipt.claims.ticket_id)
            .and_modify(|winner| {
                if receipt_order_key(receipt) < receipt_order_key(winner) {
                    *winner = receipt;
                }
            })
            .or_insert(receipt);
    }

    receipts
        .iter()
        .map(|receipt| {
            let winning_receipt_id = winners
                .get(&receipt.claims.ticket_id)
                .map(|winner| winner.claims.receipt_id);
            let accepted = winning_receipt_id == Some(receipt.claims.receipt_id);
            AdmissionReceiptOutcome {
                receipt_id: receipt.claims.receipt_id,
                ticket_id: receipt.claims.ticket_id,
                outcome: if accepted {
                    AdmissionOutcome::Accepted
                } else {
                    AdmissionOutcome::DuplicateReview
                },
                reason: (!accepted).then(|| "duplicate admission receipt".into()),
                winning_receipt_id,
            }
        })
        .collect()
}

fn receipt_order_key(receipt: &SignedScanReceipt) -> (DateTime<Utc>, Uuid, i64, Uuid) {
    (
        receipt.claims.scanned_at,
        receipt.claims.scanner_id,
        receipt.claims.scanner_sequence,
        receipt.claims.receipt_id,
    )
}

#[derive(Clone)]
pub struct AdmissionService {
    database: DatabaseConnection,
}

impl AdmissionService {
    pub fn new(database: DatabaseConnection) -> Self {
        Self { database }
    }

    pub async fn migrate(&self) -> Result<(), AdmissionError> {
        self.database
            .execute_unprepared(ADMISSION_MIGRATION)
            .await?;
        Ok(())
    }

    pub async fn register_signing_key(
        &self,
        descriptor: &AdmissionVerificationKey,
    ) -> Result<(), AdmissionError> {
        validate_descriptor(descriptor)?;
        let public_key = decode_bytes::<32>(&descriptor.public_key)?;
        let result = self
            .query_one(
                r#"insert into admission_signing_keys (
                   key_id, issuance_epoch, public_key, active_from, retire_at
               ) values ($1, $2, $3, $4, $5)
               on conflict (key_id) do update set key_id = excluded.key_id
               where admission_signing_keys.issuance_epoch = excluded.issuance_epoch
                 and admission_signing_keys.public_key = excluded.public_key
                 and admission_signing_keys.active_from = excluded.active_from
                 and admission_signing_keys.retire_at = excluded.retire_at
               returning key_id"#,
                vec![
                    descriptor.key_id.clone().into(),
                    descriptor.issuance_epoch.into(),
                    public_key.to_vec().into(),
                    descriptor.active_from.into(),
                    descriptor.retire_at.into(),
                ],
            )
            .await?;
        if result.is_none() {
            return Err(AdmissionError::Policy(
                "signing key id is already bound to different key material or epochs".into(),
            ));
        }
        Ok(())
    }

    pub async fn register_scanner_key(
        &self,
        scanner_id: Uuid,
        key_id: &str,
        public_key: &str,
        active_from: DateTime<Utc>,
        retire_at: DateTime<Utc>,
    ) -> Result<(), AdmissionError> {
        if key_id.trim().is_empty() || retire_at <= active_from {
            return Err(AdmissionError::Policy(
                "invalid scanner verification key window".into(),
            ));
        }
        let public_key = decode_bytes::<32>(public_key)?;
        let result = self
            .query_one(
                r#"insert into admission_scanner_keys (
                   scanner_id, key_id, public_key, active_from, retire_at
               ) values ($1, $2, $3, $4, $5)
               on conflict (scanner_id, key_id) do update set key_id = excluded.key_id
               where admission_scanner_keys.public_key = excluded.public_key
                 and admission_scanner_keys.active_from = excluded.active_from
                 and admission_scanner_keys.retire_at = excluded.retire_at
               returning scanner_id"#,
                vec![
                    scanner_id.into(),
                    key_id.to_owned().into(),
                    public_key.to_vec().into(),
                    active_from.into(),
                    retire_at.into(),
                ],
            )
            .await?;
        if result.is_none() {
            return Err(AdmissionError::Policy(
                "scanner key id is already bound to different key material or windows".into(),
            ));
        }
        Ok(())
    }

    pub async fn register_entitlement(
        &self,
        ticket_id: Uuid,
        order_id: Uuid,
        event_id: Uuid,
        issuance_epoch: i64,
        now: DateTime<Utc>,
    ) -> Result<(), AdmissionError> {
        let result = self
            .query_one(
                r#"insert into admission_entitlements (
                       ticket_id, order_id, event_id, issuance_epoch, created_at, updated_at
                   ) select $1, orders.id, orders.event_id, $4, $5, $5
                     from ticket_orders orders
                    where orders.id = $2 and orders.event_id = $3 and orders.status = 'paid'
                   on conflict (ticket_id) do update set ticket_id = excluded.ticket_id
                   where admission_entitlements.order_id = excluded.order_id
                     and admission_entitlements.event_id = excluded.event_id
                     and admission_entitlements.issuance_epoch = excluded.issuance_epoch
                   returning ticket_id"#,
                vec![
                    ticket_id.into(),
                    order_id.into(),
                    event_id.into(),
                    issuance_epoch.into(),
                    now.into(),
                ],
            )
            .await?;
        if result.is_none() {
            return Err(AdmissionError::Policy(
                "admission entitlement requires a paid order for the same event".into(),
            ));
        }
        Ok(())
    }

    pub async fn register_token(
        &self,
        claims: &AdmissionTokenClaims,
    ) -> Result<(), AdmissionError> {
        claims
            .validate_shape()
            .map_err(|error| AdmissionError::Policy(error.to_string()))?;
        let result = self
            .query_one(
                r#"insert into admission_tokens (
                       token_id, ticket_id, order_id, event_id, issuance_epoch,
                       key_id, issued_at, expires_at
                   ) select $1, entitlement.ticket_id, entitlement.order_id,
                            entitlement.event_id, entitlement.issuance_epoch,
                            signing_key.key_id, $7, $8
                     from admission_entitlements entitlement
                     join admission_signing_keys signing_key
                       on signing_key.key_id = $6
                      and signing_key.issuance_epoch = entitlement.issuance_epoch
                    where entitlement.ticket_id = $2
                      and entitlement.order_id = $3
                      and entitlement.event_id = $4
                      and entitlement.issuance_epoch = $5
                      and entitlement.status = 'active'
                      and $7 >= signing_key.active_from
                      and $8 <= signing_key.retire_at
                   on conflict (token_id) do update set token_id = excluded.token_id
                   where admission_tokens.ticket_id = excluded.ticket_id
                     and admission_tokens.order_id = excluded.order_id
                     and admission_tokens.event_id = excluded.event_id
                     and admission_tokens.issuance_epoch = excluded.issuance_epoch
                     and admission_tokens.key_id = excluded.key_id
                     and admission_tokens.issued_at = excluded.issued_at
                     and admission_tokens.expires_at = excluded.expires_at
                   returning token_id"#,
                vec![
                    claims.token_id.into(),
                    claims.ticket_id.into(),
                    claims.order_id.into(),
                    claims.event_id.into(),
                    claims.issuance_epoch.into(),
                    claims.key_id.clone().into(),
                    claims.issued_at.into(),
                    claims.expires_at.into(),
                ],
            )
            .await?;
        if result.is_none() {
            return Err(AdmissionError::Policy(
                "token does not match an active entitlement and signing-key epoch".into(),
            ));
        }
        Ok(())
    }

    pub async fn revoke_entitlement(
        &self,
        ticket_id: Uuid,
        revoked_at: DateTime<Utc>,
    ) -> Result<(), AdmissionError> {
        self.query_one(
            "select evgl_revoke_admission_entitlement($1, $2) as issuance_epoch",
            vec![ticket_id.into(), revoked_at.into()],
        )
        .await?
        .ok_or_else(|| AdmissionError::Database("revocation returned no row".into()))?;
        Ok(())
    }

    pub async fn record_receipt(
        &self,
        receipt: &SignedScanReceipt,
        received_at: DateTime<Utc>,
    ) -> Result<Uuid, AdmissionError> {
        let key_row = self
            .query_one(
                r#"select public_key
                      from admission_scanner_keys
                     where scanner_id = $1 and key_id = $2
                       and active_from <= $3 and retire_at > $3"#,
                vec![
                    receipt.claims.scanner_id.into(),
                    receipt.claims.scanner_key_id.clone().into(),
                    receipt.claims.scanned_at.into(),
                ],
            )
            .await?
            .ok_or(AdmissionError::UnknownKey)?;
        let public_key: Vec<u8> = key_row.try_get("", "public_key")?;
        verify_scan_receipt(receipt, &URL_SAFE_NO_PAD.encode(&public_key))?;

        let token_row = self
            .query_one(
                r#"select
                       token.ticket_id = $2
                       and token.order_id = $3
                       and token.event_id = $4
                       and token.revoked_at is null
                       and $5 >= token.issued_at
                       and $5 < token.expires_at
                       and entitlement.status = 'active'
                       and entitlement.issuance_epoch = token.issuance_epoch as candidate
                     from admission_tokens token
                     join admission_entitlements entitlement
                       on entitlement.ticket_id = token.ticket_id
                    where token.token_id = $1"#,
                vec![
                    receipt.claims.token_id.into(),
                    receipt.claims.ticket_id.into(),
                    receipt.claims.order_id.into(),
                    receipt.claims.event_id.into(),
                    receipt.claims.scanned_at.into(),
                ],
            )
            .await?
            .ok_or(AdmissionError::Malformed)?;
        let candidate: bool = token_row.try_get("", "candidate")?;
        let payload = canonical_json(&receipt.claims)?;
        let payload_hash = Sha256::digest(&payload).to_vec();
        let signature = decode_bytes::<64>(&receipt.signature)?.to_vec();

        let result = self
            .query_one(
                r#"select evgl_record_admission_receipt(
                       $1, $2, $3, $4, $5, $6, $7, $8,
                       $9, $10, $11, $12, $13, $14
                   ) as id"#,
                vec![
                    receipt.claims.receipt_id.into(),
                    receipt.claims.scanner_id.into(),
                    receipt.claims.scanner_key_id.clone().into(),
                    receipt.claims.scanner_sequence.into(),
                    receipt.claims.token_id.into(),
                    receipt.claims.event_id.into(),
                    receipt.claims.ticket_id.into(),
                    receipt.claims.order_id.into(),
                    receipt.claims.scanned_at.into(),
                    received_at.into(),
                    payload_hash.into(),
                    signature.into(),
                    (if candidate { "candidate" } else { "rejected" })
                        .to_owned()
                        .into(),
                    (!candidate)
                        .then(|| "token or entitlement is not currently admissible".to_owned())
                        .into(),
                ],
            )
            .await?
            .ok_or_else(|| AdmissionError::Database("receipt insert returned no row".into()))?;
        Ok(result.try_get("", "id")?)
    }

    pub async fn outcomes(
        &self,
        ticket_id: Uuid,
    ) -> Result<Vec<AdmissionReceiptOutcome>, AdmissionError> {
        let rows = self
            .database
            .query_all_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                r#"select receipt_id, ticket_id, outcome, reason, winning_receipt_id
                     from admission_receipt_outcomes
                    where ticket_id = $1
                    order by receipt_id"#,
                vec![ticket_id.into()],
            ))
            .await?;
        rows.into_iter()
            .map(|row| {
                let outcome: String = row.try_get("", "outcome")?;
                Ok(AdmissionReceiptOutcome {
                    receipt_id: row.try_get("", "receipt_id")?,
                    ticket_id: row.try_get("", "ticket_id")?,
                    outcome: match outcome.as_str() {
                        "accepted" => AdmissionOutcome::Accepted,
                        "duplicate_review" => AdmissionOutcome::DuplicateReview,
                        _ => AdmissionOutcome::Rejected,
                    },
                    reason: row.try_get("", "reason")?,
                    winning_receipt_id: row.try_get("", "winning_receipt_id")?,
                })
            })
            .collect::<Result<Vec<_>, DbErr>>()
            .map_err(AdmissionError::from)
    }

    async fn query_one(
        &self,
        sql: &str,
        values: Vec<sea_orm::Value>,
    ) -> Result<Option<QueryResult>, AdmissionError> {
        Ok(self
            .database
            .query_one_raw(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                sql,
                values,
            ))
            .await?)
    }
}

fn validate_descriptor(descriptor: &AdmissionVerificationKey) -> Result<(), AdmissionError> {
    if descriptor.key_id.trim().is_empty()
        || descriptor.issuance_epoch < 0
        || descriptor.retire_at <= descriptor.active_from
    {
        return Err(AdmissionError::Policy(
            "invalid admission verification key".into(),
        ));
    }
    decode_verifying_key(&descriptor.public_key)?;
    Ok(())
}

fn canonical_json<T: Serialize>(value: &T) -> Result<Vec<u8>, AdmissionError> {
    serde_json::to_vec(value).map_err(|_| AdmissionError::Malformed)
}

fn decode_compact(compact: &str) -> Result<(Vec<u8>, Signature), AdmissionError> {
    let mut components = compact.split('.');
    let payload = components.next().ok_or(AdmissionError::Malformed)?;
    let signature = components.next().ok_or(AdmissionError::Malformed)?;
    if components.next().is_some() || payload.is_empty() || signature.is_empty() {
        return Err(AdmissionError::Malformed);
    }
    let payload = URL_SAFE_NO_PAD
        .decode(payload)
        .map_err(|_| AdmissionError::Malformed)?;
    Ok((payload, decode_signature(signature)?))
}

fn decode_verifying_key(value: &str) -> Result<VerifyingKey, AdmissionError> {
    VerifyingKey::from_bytes(&decode_bytes::<32>(value)?).map_err(|_| AdmissionError::Malformed)
}

fn decode_signature(value: &str) -> Result<Signature, AdmissionError> {
    Ok(Signature::from_bytes(&decode_bytes::<64>(value)?))
}

fn decode_bytes<const N: usize>(value: &str) -> Result<[u8; N], AdmissionError> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| AdmissionError::Malformed)?
        .try_into()
        .map_err(|_| AdmissionError::Malformed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdmissionError {
    Malformed,
    InvalidSignature,
    WrongEvent,
    Expired,
    Revoked,
    UnknownKey,
    RetiredKey,
    StaleSnapshot,
    Policy(String),
    Database(String),
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => formatter.write_str("malformed admission data"),
            Self::InvalidSignature => formatter.write_str("invalid admission signature"),
            Self::WrongEvent => formatter.write_str("admission token belongs to another event"),
            Self::Expired => formatter.write_str("admission token is expired or not yet active"),
            Self::Revoked => formatter.write_str("admission entitlement is revoked"),
            Self::UnknownKey => formatter.write_str("admission signing key is unknown"),
            Self::RetiredKey => formatter.write_str("admission signing key is outside its window"),
            Self::StaleSnapshot => formatter.write_str("scanner key/revocation snapshot is stale"),
            Self::Policy(message) | Self::Database(message) => formatter.write_str(message),
        }
    }
}

impl std::error::Error for AdmissionError {}

impl From<DbErr> for AdmissionError {
    fn from(error: DbErr) -> Self {
        Self::Database(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use evgl_interfaces::admission::{ADMISSION_TOKEN_VERSION, AdmissionRevocation};

    fn claims(now: DateTime<Utc>, event_id: Uuid, key_id: &str) -> AdmissionTokenClaims {
        AdmissionTokenClaims {
            version: ADMISSION_TOKEN_VERSION,
            token_id: Uuid::new_v4(),
            event_id,
            ticket_id: Uuid::new_v4(),
            order_id: Uuid::new_v4(),
            issuance_epoch: 1,
            key_id: key_id.into(),
            issued_at: now,
            expires_at: now + Duration::minutes(10),
        }
    }

    #[test]
    fn tokens_fail_closed_for_wrong_event_expiry_revocation_and_malformed_data() {
        let now = Utc::now();
        let event_id = Uuid::new_v4();
        let signer = AdmissionTokenSigner::new(
            SigningKey::from_bytes(&[7; 32]),
            "key-1",
            1,
            now - Duration::minutes(1),
            now + Duration::hours(1),
            DEFAULT_MAX_TOKEN_LIFETIME,
        )
        .unwrap();
        let claims = claims(now, event_id, "key-1");
        let compact = signer.sign(&claims).unwrap();
        let mut snapshot = AdmissionKeySnapshot {
            snapshot_id: Uuid::new_v4(),
            event_id,
            generated_at: now - Duration::minutes(1),
            valid_until: now + Duration::minutes(30),
            keys: vec![signer.verification_key()],
            revocations: Vec::new(),
        };

        assert_eq!(
            verify_admission_token(&compact, &snapshot, Uuid::new_v4(), now),
            Err(AdmissionError::WrongEvent)
        );
        assert_eq!(
            verify_admission_token(&compact, &snapshot, event_id, now + Duration::minutes(11)),
            Err(AdmissionError::Expired)
        );
        assert_eq!(
            verify_admission_token("not-a-token", &snapshot, event_id, now),
            Err(AdmissionError::Malformed)
        );

        snapshot.revocations.push(AdmissionRevocation {
            ticket_id: claims.ticket_id,
            issuance_epoch: claims.issuance_epoch,
            revoked_at: now,
        });
        assert_eq!(
            verify_admission_token(&compact, &snapshot, event_id, now),
            Err(AdmissionError::Revoked)
        );
    }

    #[test]
    fn key_rotation_overlap_accepts_each_epoch_until_its_retirement() {
        let now = Utc::now();
        let event_id = Uuid::new_v4();
        let old = AdmissionTokenSigner::new(
            SigningKey::from_bytes(&[1; 32]),
            "old",
            1,
            now - Duration::hours(1),
            now + Duration::minutes(20),
            DEFAULT_MAX_TOKEN_LIFETIME,
        )
        .unwrap();
        let new = AdmissionTokenSigner::new(
            SigningKey::from_bytes(&[2; 32]),
            "new",
            2,
            now - Duration::minutes(1),
            now + Duration::hours(1),
            DEFAULT_MAX_TOKEN_LIFETIME,
        )
        .unwrap();
        let old_claims = claims(now, event_id, "old");
        let mut new_claims = claims(now, event_id, "new");
        new_claims.issuance_epoch = 2;
        let snapshot = AdmissionKeySnapshot {
            snapshot_id: Uuid::new_v4(),
            event_id,
            generated_at: now - Duration::minutes(1),
            valid_until: now + Duration::minutes(15),
            keys: vec![old.verification_key(), new.verification_key()],
            revocations: Vec::new(),
        };

        assert_eq!(
            verify_admission_token(&old.sign(&old_claims).unwrap(), &snapshot, event_id, now)
                .unwrap(),
            old_claims
        );
        assert_eq!(
            verify_admission_token(&new.sign(&new_claims).unwrap(), &snapshot, event_id, now)
                .unwrap(),
            new_claims
        );
        assert_eq!(
            verify_admission_token(
                &old.sign(&old_claims).unwrap(),
                &snapshot,
                event_id,
                now + Duration::minutes(16)
            ),
            Err(AdmissionError::StaleSnapshot)
        );
    }

    #[test]
    fn reconciliation_is_deterministic_across_input_orders() {
        let now = Utc::now();
        let ticket_id = Uuid::new_v4();
        let event_id = Uuid::new_v4();
        let order_id = Uuid::new_v4();
        let token_id = Uuid::new_v4();
        let first_signer = ScannerReceiptSigner::new(
            Uuid::from_u128(1),
            "scanner-1",
            SigningKey::from_bytes(&[3; 32]),
        );
        let second_signer = ScannerReceiptSigner::new(
            Uuid::from_u128(2),
            "scanner-2",
            SigningKey::from_bytes(&[4; 32]),
        );
        let first = first_signer
            .sign(ScanReceiptClaims {
                receipt_id: Uuid::from_u128(10),
                scanner_id: Uuid::from_u128(1),
                scanner_key_id: "scanner-1".into(),
                scanner_sequence: 1,
                token_id,
                event_id,
                ticket_id,
                order_id,
                scanned_at: now,
            })
            .unwrap();
        let second = second_signer
            .sign(ScanReceiptClaims {
                receipt_id: Uuid::from_u128(20),
                scanner_id: Uuid::from_u128(2),
                scanner_key_id: "scanner-2".into(),
                scanner_sequence: 1,
                token_id,
                event_id,
                ticket_id,
                order_id,
                scanned_at: now,
            })
            .unwrap();

        let forward = reconcile_verified_receipts(&[first.clone(), second.clone()]);
        let reverse = reconcile_verified_receipts(&[second, first]);
        assert!(
            forward
                .iter()
                .all(|outcome| { outcome.winning_receipt_id == Some(Uuid::from_u128(10)) })
        );
        assert!(
            reverse
                .iter()
                .all(|outcome| { outcome.winning_receipt_id == Some(Uuid::from_u128(10)) })
        );
        assert_eq!(
            forward
                .iter()
                .filter(|outcome| outcome.outcome == AdmissionOutcome::Accepted)
                .count(),
            1
        );
    }
}
