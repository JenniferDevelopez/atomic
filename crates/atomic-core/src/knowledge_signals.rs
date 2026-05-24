//! Deterministic knowledge-quality signals.
//!
//! Signals are opportunities to improve the knowledge base, not system health
//! checks. Providers are deterministic and emit normalized `KnowledgeSignal`
//! rows that the dashboard can render.

use crate::error::AtomicCoreError;
use crate::storage::StorageBackend;
use crate::AtomicCore;
use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::HashMap;

pub const WIKI_CANDIDATE_PROVIDER_ID: &str = "wiki_candidate";
pub const WIKI_UPDATE_PROVIDER_ID: &str = "wiki_update";
pub const TAG_REDUNDANCY_PROVIDER_ID: &str = "tag_redundancy";
pub const EMPTY_TAG_PROVIDER_ID: &str = "empty_tag";
pub const MISSING_TAG_OVERLAP_PROVIDER_ID: &str = "missing_tag_overlap";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignal {
    pub id: String,
    pub provider_id: String,
    pub target: KnowledgeSignalTarget,
    pub score: f32,
    pub confidence: f32,
    pub severity: KnowledgeSignalSeverity,
    pub title: String,
    pub summary: String,
    pub reasons: Vec<KnowledgeSignalReason>,
    #[serde(default)]
    pub evidence: serde_json::Value,
    pub suggested_actions: Vec<KnowledgeSignalAction>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

trait KnowledgeSignalEvidence: Serialize {
    const SCHEMA: &'static str;
    const SCHEMA_VERSION: u32 = 1;

    fn to_value(&self) -> Result<Value, AtomicCoreError> {
        let mut value = serde_json::to_value(self)?;
        if let Some(obj) = value.as_object_mut() {
            obj.insert("schema".to_string(), json!(Self::SCHEMA));
            obj.insert("schema_version".to_string(), json!(Self::SCHEMA_VERSION));
        }
        Ok(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalTarget {
    pub kind: String,
    pub id: String,
    pub label: String,
}

impl KnowledgeSignalTarget {
    fn tag(id: String, label: String) -> Self {
        Self {
            kind: "tag".to_string(),
            id,
            label,
        }
    }

    fn atom(id: String, label: String) -> Self {
        Self {
            kind: "atom".to_string(),
            id,
            label,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeSignalSeverity {
    Info,
    Opportunity,
    Review,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalReason {
    pub kind: String,
    pub label: String,
    pub value: serde_json::Value,
    pub contribution: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalAction {
    pub id: String,
    pub label: String,
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(default)]
    pub include_dismissed: bool,
    #[serde(default)]
    pub include_snoozed: bool,
    #[serde(default)]
    pub limit: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct KnowledgeSignalProviderConfig {
    pub provider_id: String,
    pub enabled: bool,
    pub weight: f32,
    pub min_score: f32,
    pub min_confidence: f32,
    pub show_on_dashboard: bool,
    pub include_in_briefing: bool,
    #[serde(default)]
    pub config_json: serde_json::Value,
}

impl KnowledgeSignalProviderConfig {
    fn default_for(provider_id: &str) -> Self {
        Self {
            provider_id: provider_id.to_string(),
            enabled: true,
            weight: 1.0,
            min_score: 0.0,
            min_confidence: 0.0,
            show_on_dashboard: true,
            include_in_briefing: false,
            config_json: json!({}),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnowledgeSignalFeedback {
    pub signal_key: String,
    pub provider_id: String,
    pub target_type: String,
    pub target_id: Option<String>,
    pub state: String,
    pub snoozed_until: Option<String>,
}

#[async_trait]
pub trait KnowledgeSignalProvider: Send + Sync {
    fn id(&self) -> &'static str;
    fn name(&self) -> &'static str;
    fn default_config(&self) -> KnowledgeSignalProviderConfig {
        KnowledgeSignalProviderConfig::default_for(self.id())
    }

    async fn evaluate(
        &self,
        core: &AtomicCore,
        config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError>;
}

pub async fn list_knowledge_signals(
    core: &AtomicCore,
    filter: KnowledgeSignalFilter,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let providers = signal_providers();
    let feedback = list_feedback(core).await?;
    let now = Utc::now();
    let mut out = Vec::new();

    for provider in providers {
        if let Some(filter_provider) = filter.provider_id.as_deref() {
            if filter_provider != provider.id() {
                continue;
            }
        }

        let config = get_provider_config(core, provider.id()).await?;
        if !config.enabled || !config.show_on_dashboard {
            continue;
        }

        let mut signals = provider.evaluate(core, &config).await?;
        apply_provider_weight(&mut signals, config.weight);
        signals.retain(|signal| {
            signal.score >= config.min_score && signal.confidence >= config.min_confidence
        });
        out.extend(signals);
    }

    out.retain(|signal| {
        let Some(fb) = feedback.get(&signal.id) else {
            return true;
        };

        match fb.state.as_str() {
            "dismissed" | "ignored" => filter.include_dismissed,
            "snoozed" => {
                if filter.include_snoozed {
                    return true;
                }
                match fb
                    .snoozed_until
                    .as_deref()
                    .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
                {
                    Some(until) => until.with_timezone(&Utc) <= now,
                    None => true,
                }
            }
            _ => true,
        }
    });

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });

    if let Some(limit) = filter.limit {
        if limit >= 0 {
            out.truncate(limit as usize);
        }
    }

    Ok(out)
}

pub async fn list_briefing_knowledge_signals(
    core: &AtomicCore,
    _window_start: DateTime<Utc>,
    _window_end: DateTime<Utc>,
    limit: i32,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let providers = signal_providers();
    let feedback = list_feedback(core).await?;
    let now = Utc::now();
    let mut out = Vec::new();

    for provider in providers {
        let config = get_provider_config(core, provider.id()).await?;
        if !config.enabled || !config.include_in_briefing {
            continue;
        }

        let mut signals = provider.evaluate(core, &config).await?;
        apply_provider_weight(&mut signals, config.weight);
        signals.retain(|signal| {
            signal.score >= config.min_score
                && signal.confidence >= config.min_confidence
                && signal_is_visible_with_feedback(feedback.get(&signal.id), now)
        });
        out.extend(signals);
    }

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });

    if limit >= 0 {
        out.truncate(limit as usize);
    }

    Ok(out)
}

pub async fn list_feedback(
    core: &AtomicCore,
) -> Result<HashMap<String, KnowledgeSignalFeedback>, AtomicCoreError> {
    list_feedback_inner(core).await
}

pub fn signal_is_visible_with_feedback(
    feedback: Option<&KnowledgeSignalFeedback>,
    now: DateTime<Utc>,
) -> bool {
    match feedback {
        Some(fb) if matches!(fb.state.as_str(), "dismissed" | "ignored") => false,
        Some(fb) if fb.state == "snoozed" => fb
            .snoozed_until
            .as_deref()
            .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
            .map(|until| until.with_timezone(&Utc) <= now)
            .unwrap_or(true),
        _ => true,
    }
}

fn apply_provider_weight(signals: &mut [KnowledgeSignal], weight: f32) {
    for signal in signals {
        signal.score = (signal.score * weight).clamp(0.0, 100.0);
    }
}

pub async fn dismiss_signal(core: &AtomicCore, signal_key: &str) -> Result<(), AtomicCoreError> {
    set_feedback(core, signal_key, "dismissed", None).await
}

pub async fn snooze_signal(
    core: &AtomicCore,
    signal_key: &str,
    until: &str,
) -> Result<(), AtomicCoreError> {
    chrono::DateTime::parse_from_rfc3339(until).map_err(|_| {
        AtomicCoreError::Validation("snoozed_until must be an RFC3339 timestamp".to_string())
    })?;
    set_feedback(core, signal_key, "snoozed", Some(until)).await
}

pub async fn set_provider_config(
    core: &AtomicCore,
    provider_id: &str,
    mut config: KnowledgeSignalProviderConfig,
) -> Result<KnowledgeSignalProviderConfig, AtomicCoreError> {
    config.provider_id = provider_id.to_string();
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let config_to_store = config.clone();
            tokio::task::spawn_blocking(move || {
                let now = Utc::now().to_rfc3339();
                let config_json = serde_json::to_string(&config_to_store.config_json)?;
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                conn.execute(
                    "INSERT INTO knowledge_signal_preferences
                        (provider_id, enabled, weight, min_score, min_confidence,
                         show_on_dashboard, include_in_briefing, config_json, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(provider_id) DO UPDATE SET
                        enabled = excluded.enabled,
                        weight = excluded.weight,
                        min_score = excluded.min_score,
                        min_confidence = excluded.min_confidence,
                        show_on_dashboard = excluded.show_on_dashboard,
                        include_in_briefing = excluded.include_in_briefing,
                        config_json = excluded.config_json,
                        updated_at = excluded.updated_at",
                    params![
                        config_to_store.provider_id,
                        if config_to_store.enabled { 1 } else { 0 },
                        config_to_store.weight,
                        config_to_store.min_score,
                        config_to_store.min_confidence,
                        if config_to_store.show_on_dashboard {
                            1
                        } else {
                            0
                        },
                        if config_to_store.include_in_briefing {
                            1
                        } else {
                            0
                        },
                        config_json,
                        now,
                    ],
                )?;
                Ok::<(), AtomicCoreError>(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))??;
            Ok(config)
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let now = Utc::now().to_rfc3339();
            let config_json = serde_json::to_string(&config.config_json)?;
            sqlx::query(
                "INSERT INTO knowledge_signal_preferences
                    (db_id, provider_id, enabled, weight, min_score, min_confidence,
                     show_on_dashboard, include_in_briefing, config_json, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
                 ON CONFLICT(db_id, provider_id) DO UPDATE SET
                    enabled = excluded.enabled,
                    weight = excluded.weight,
                    min_score = excluded.min_score,
                    min_confidence = excluded.min_confidence,
                    show_on_dashboard = excluded.show_on_dashboard,
                    include_in_briefing = excluded.include_in_briefing,
                    config_json = excluded.config_json,
                    updated_at = excluded.updated_at",
            )
            .bind(&storage.db_id)
            .bind(&config.provider_id)
            .bind(config.enabled)
            .bind(config.weight)
            .bind(config.min_score)
            .bind(config.min_confidence)
            .bind(config.show_on_dashboard)
            .bind(config.include_in_briefing)
            .bind(config_json)
            .bind(now)
            .execute(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok(config)
        }
    }
}

pub async fn restore_signal(core: &AtomicCore, signal_key: &str) -> Result<(), AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let key = signal_key.to_string();
            tokio::task::spawn_blocking(move || {
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                conn.execute(
                    "DELETE FROM knowledge_signal_feedback WHERE signal_key = ?1",
                    params![key],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            sqlx::query(
                "DELETE FROM knowledge_signal_feedback
                 WHERE db_id = $1 AND signal_key = $2",
            )
            .bind(&storage.db_id)
            .bind(signal_key)
            .execute(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok(())
        }
    }
}

async fn get_provider_config(
    core: &AtomicCore,
    provider_id: &str,
) -> Result<KnowledgeSignalProviderConfig, AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let id = provider_id.to_string();
            tokio::task::spawn_blocking(move || {
                let conn = storage.database().read_conn()?;
                let row = conn
                    .query_row(
                        "SELECT enabled, weight, min_score, min_confidence, show_on_dashboard,
                                include_in_briefing, config_json
                         FROM knowledge_signal_preferences
                         WHERE provider_id = ?1",
                        params![id],
                        |row| {
                            Ok((
                                row.get::<_, i32>(0)?,
                                row.get::<_, f32>(1)?,
                                row.get::<_, f32>(2)?,
                                row.get::<_, f32>(3)?,
                                row.get::<_, i32>(4)?,
                                row.get::<_, i32>(5)?,
                                row.get::<_, String>(6)?,
                            ))
                        },
                    )
                    .optional()?;

                let Some((enabled, weight, min_score, min_confidence, show, briefing, json)) = row
                else {
                    return Ok(default_provider_config(&id));
                };

                Ok(KnowledgeSignalProviderConfig {
                    provider_id: id,
                    enabled: enabled != 0,
                    weight,
                    min_score,
                    min_confidence,
                    show_on_dashboard: show != 0,
                    include_in_briefing: briefing != 0,
                    config_json: serde_json::from_str(&json).unwrap_or_else(|_| json!({})),
                })
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let row = sqlx::query_as::<_, (bool, f32, f32, f32, bool, bool, String)>(
                "SELECT enabled, weight, min_score, min_confidence, show_on_dashboard,
                        include_in_briefing, config_json
                 FROM knowledge_signal_preferences
                 WHERE db_id = $1 AND provider_id = $2",
            )
            .bind(&storage.db_id)
            .bind(provider_id)
            .fetch_optional(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            let Some((enabled, weight, min_score, min_confidence, show, briefing, config_json)) =
                row
            else {
                return Ok(default_provider_config(provider_id));
            };

            Ok(KnowledgeSignalProviderConfig {
                provider_id: provider_id.to_string(),
                enabled,
                weight,
                min_score,
                min_confidence,
                show_on_dashboard: show,
                include_in_briefing: briefing,
                config_json: serde_json::from_str(&config_json).unwrap_or_else(|_| json!({})),
            })
        }
    }
}

fn default_provider_config(provider_id: &str) -> KnowledgeSignalProviderConfig {
    let mut config = KnowledgeSignalProviderConfig::default_for(provider_id);
    if provider_id == WIKI_CANDIDATE_PROVIDER_ID || provider_id == WIKI_UPDATE_PROVIDER_ID {
        config.include_in_briefing = true;
    }
    if provider_id == WIKI_UPDATE_PROVIDER_ID {
        config.min_score = 10.0;
        config.min_confidence = 0.1;
    }
    if provider_id == TAG_REDUNDANCY_PROVIDER_ID {
        config.include_in_briefing = true;
        config.min_score = 45.0;
        config.min_confidence = 0.55;
    }
    if provider_id == EMPTY_TAG_PROVIDER_ID {
        config.include_in_briefing = false;
        config.min_score = 10.0;
        config.min_confidence = 0.8;
    }
    if provider_id == MISSING_TAG_OVERLAP_PROVIDER_ID {
        config.include_in_briefing = false;
        config.min_score = 50.0;
        config.min_confidence = 0.65;
    }
    config
}

fn signal_providers() -> Vec<Box<dyn KnowledgeSignalProvider>> {
    vec![
        Box::new(WikiCandidateProvider),
        Box::new(WikiUpdateProvider),
        Box::new(TagRedundancyProvider),
        Box::new(EmptyTagProvider),
        Box::new(MissingTagOverlapProvider),
    ]
}

async fn list_feedback_inner(
    core: &AtomicCore,
) -> Result<HashMap<String, KnowledgeSignalFeedback>, AtomicCoreError> {
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            tokio::task::spawn_blocking(move || {
                let conn = storage.database().read_conn()?;
                let mut stmt = conn.prepare(
                    "SELECT signal_key, provider_id, target_type, target_id, state, snoozed_until
                     FROM knowledge_signal_feedback",
                )?;
                let rows = stmt.query_map([], |row| {
                    Ok(KnowledgeSignalFeedback {
                        signal_key: row.get(0)?,
                        provider_id: row.get(1)?,
                        target_type: row.get(2)?,
                        target_id: row.get(3)?,
                        state: row.get(4)?,
                        snoozed_until: row.get(5)?,
                    })
                })?;

                let mut out = HashMap::new();
                for row in rows {
                    let fb = row?;
                    out.insert(fb.signal_key.clone(), fb);
                }
                Ok(out)
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let rows = sqlx::query_as::<
                _,
                (
                    String,
                    String,
                    String,
                    Option<String>,
                    String,
                    Option<String>,
                ),
            >(
                "SELECT signal_key, provider_id, target_type, target_id, state, snoozed_until
                 FROM knowledge_signal_feedback
                 WHERE db_id = $1",
            )
            .bind(&storage.db_id)
            .fetch_all(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

            let mut out = HashMap::new();
            for (signal_key, provider_id, target_type, target_id, state, snoozed_until) in rows {
                out.insert(
                    signal_key.clone(),
                    KnowledgeSignalFeedback {
                        signal_key,
                        provider_id,
                        target_type,
                        target_id,
                        state,
                        snoozed_until,
                    },
                );
            }
            Ok(out)
        }
    }
}

async fn set_feedback(
    core: &AtomicCore,
    signal_key: &str,
    state: &str,
    snoozed_until: Option<&str>,
) -> Result<(), AtomicCoreError> {
    let (provider_id, target_type, target_id) = parse_signal_key(signal_key)?;
    match &core.storage {
        StorageBackend::Sqlite(storage) => {
            let storage = storage.clone();
            let key = signal_key.to_string();
            let state = state.to_string();
            let snoozed_until = snoozed_until.map(|s| s.to_string());
            tokio::task::spawn_blocking(move || {
                let now = Utc::now().to_rfc3339();
                let conn = storage
                    .database()
                    .conn
                    .lock()
                    .map_err(|e| AtomicCoreError::Lock(e.to_string()))?;
                conn.execute(
                    "INSERT INTO knowledge_signal_feedback
                        (signal_key, provider_id, target_type, target_id, state, snoozed_until, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                     ON CONFLICT(signal_key) DO UPDATE SET
                        state = excluded.state,
                        snoozed_until = excluded.snoozed_until,
                        updated_at = excluded.updated_at",
                    params![
                        key,
                        provider_id,
                        target_type,
                        target_id,
                        state,
                        snoozed_until,
                        now
                    ],
                )?;
                Ok(())
            })
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
        }
        #[cfg(feature = "postgres")]
        StorageBackend::Postgres(storage) => {
            let now = Utc::now().to_rfc3339();
            sqlx::query(
                "INSERT INTO knowledge_signal_feedback
                    (db_id, signal_key, provider_id, target_type, target_id, state,
                     snoozed_until, created_at, updated_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8)
                 ON CONFLICT(db_id, signal_key) DO UPDATE SET
                    state = excluded.state,
                    snoozed_until = excluded.snoozed_until,
                    updated_at = excluded.updated_at",
            )
            .bind(&storage.db_id)
            .bind(signal_key)
            .bind(provider_id)
            .bind(target_type)
            .bind(target_id)
            .bind(state)
            .bind(snoozed_until)
            .bind(now)
            .execute(&storage.pool)
            .await
            .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
            Ok(())
        }
    }
}

fn parse_signal_key(signal_key: &str) -> Result<(String, String, Option<String>), AtomicCoreError> {
    let parts: Vec<&str> = signal_key.split(':').collect();
    if parts.len() < 3 {
        return Err(AtomicCoreError::Validation(format!(
            "Invalid knowledge signal key: {signal_key}"
        )));
    }
    let provider_id = parts[0].to_string();
    let target_type = parts[1].to_string();
    let target_id = parts.get(2).map(|s| s.to_string());
    Ok((provider_id, target_type, target_id))
}

struct WikiCandidateProvider;

#[async_trait]
impl KnowledgeSignalProvider for WikiCandidateProvider {
    fn id(&self) -> &'static str {
        WIKI_CANDIDATE_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Wiki candidates"
    }

    async fn evaluate(
        &self,
        core: &AtomicCore,
        _config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
        match &core.storage {
            StorageBackend::Sqlite(storage) => {
                let storage = storage.clone();
                tokio::task::spawn_blocking(move || {
                    let cutoff = (Utc::now() - Duration::days(14)).to_rfc3339();
                    let conn = storage.database().read_conn()?;
                    let mut stmt = conn.prepare(
                        "WITH link_mentions AS (
                            SELECT tag_id, SUM(cnt) as link_count FROM (
                                SELECT wl.target_tag_id as tag_id, COUNT(*) as cnt
                                FROM wiki_links wl
                                WHERE wl.target_tag_id IS NOT NULL
                                GROUP BY wl.target_tag_id
                                UNION ALL
                                SELECT t2.id as tag_id, COUNT(*) as cnt
                                FROM wiki_links wl
                                JOIN tags t2 ON wl.target_tag_name = t2.name COLLATE NOCASE
                                WHERE wl.target_tag_id IS NULL
                                GROUP BY t2.id
                            )
                            GROUP BY tag_id
                        ),
                        tag_atoms AS (
                            SELECT
                                at.tag_id,
                                COUNT(DISTINCT a.id) as atom_count,
                                COUNT(DISTINCT CASE
                                    WHEN a.source_url IS NOT NULL AND length(trim(a.source_url)) > 0
                                    THEN a.source_url
                                END) as source_count,
                                SUM(CASE WHEN length(trim(a.content)) >= 200 THEN 1 ELSE 0 END) as substantive_count,
                                SUM(CASE WHEN a.created_at >= ?1 THEN 1 ELSE 0 END) as recent_count
                            FROM atom_tags at
                            JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
                            GROUP BY at.tag_id
                        ),
                        intra_edges AS (
                            SELECT
                                at1.tag_id,
                                COUNT(*) as edge_count,
                                AVG(se.similarity_score) as avg_similarity
                            FROM semantic_edges se
                            JOIN atoms source ON source.id = se.source_atom_id AND source.kind = 'captured'
                            JOIN atoms target ON target.id = se.target_atom_id AND target.kind = 'captured'
                            JOIN atom_tags at1 ON at1.atom_id = se.source_atom_id
                            JOIN atom_tags at2 ON at2.atom_id = se.target_atom_id AND at2.tag_id = at1.tag_id
                            GROUP BY at1.tag_id
                        )
                        SELECT
                            t.id,
                            t.name,
                            COALESCE(ta.atom_count, 0) as atom_count,
                            COALESCE(lm.link_count, 0) as mention_count,
                            COALESCE(ta.source_count, 0) as source_count,
                            COALESCE(ta.substantive_count, 0) as substantive_count,
                            COALESCE(ta.recent_count, 0) as recent_count,
                            COALESCE(ie.edge_count, 0) as edge_count,
                            COALESCE(ie.avg_similarity, 0.0) as avg_similarity
                        FROM tags t
                        JOIN tag_atoms ta ON ta.tag_id = t.id
                        LEFT JOIN link_mentions lm ON lm.tag_id = t.id
                        LEFT JOIN intra_edges ie ON ie.tag_id = t.id
                        WHERE t.parent_id IS NOT NULL
                          AND NOT EXISTS (SELECT 1 FROM wiki_articles wa WHERE wa.tag_id = t.id)
                          AND t.name GLOB '*[^0-9]*'
                          AND length(t.name) >= 2
                          AND ta.atom_count > 0",
                    )?;

                    let rows = stmt.query_map(params![cutoff], |row| {
                        Ok(WikiCandidateRow {
                            tag_id: row.get(0)?,
                            tag_name: row.get(1)?,
                            atom_count: row.get(2)?,
                            mention_count: row.get(3)?,
                            source_count: row.get(4)?,
                            substantive_count: row.get(5)?,
                            recent_count: row.get(6)?,
                            edge_count: row.get(7)?,
                            avg_similarity: row.get(8)?,
                        })
                    })?;

                    let now = Utc::now().to_rfc3339();
                    let mut signals = Vec::new();
                    for row in rows {
                        signals.push(row?.into_signal(&now)?);
                    }
                    signals.sort_by(|a, b| {
                        b.score
                            .partial_cmp(&a.score)
                            .unwrap_or(Ordering::Equal)
                    });
                    Ok(signals)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => {
                let cutoff = (Utc::now() - Duration::days(14)).to_rfc3339();
                let rows = sqlx::query_as::<
                    _,
                    (String, String, i64, i64, i64, i64, i64, i64, f64),
                >(
                    "WITH link_mentions AS (
                        SELECT wl.target_tag_id as tag_id, COUNT(*)::BIGINT as link_count
                        FROM wiki_links wl
                        WHERE wl.target_tag_id IS NOT NULL
                          AND wl.db_id = $2
                        GROUP BY wl.target_tag_id
                    ),
                    tag_atoms AS (
                        SELECT
                            at.tag_id,
                            COUNT(DISTINCT a.id)::BIGINT as atom_count,
                            COUNT(DISTINCT CASE
                                WHEN a.source_url IS NOT NULL AND length(trim(a.source_url)) > 0
                                THEN a.source_url
                            END)::BIGINT as source_count,
                            SUM(CASE WHEN length(trim(a.content)) >= 200 THEN 1 ELSE 0 END)::BIGINT as substantive_count,
                            SUM(CASE WHEN a.created_at >= $1 THEN 1 ELSE 0 END)::BIGINT as recent_count
                        FROM atom_tags at
                        JOIN atoms a ON a.id = at.atom_id AND a.db_id = at.db_id AND a.kind = 'captured'
                        WHERE at.db_id = $2
                        GROUP BY at.tag_id
                    ),
                    intra_edges AS (
                        SELECT
                            at1.tag_id,
                            COUNT(*)::BIGINT as edge_count,
                            AVG(se.similarity_score)::FLOAT8 as avg_similarity
                        FROM semantic_edges se
                        JOIN atoms source
                          ON source.id = se.source_atom_id
                         AND source.db_id = se.db_id
                         AND source.kind = 'captured'
                        JOIN atoms target
                          ON target.id = se.target_atom_id
                         AND target.db_id = se.db_id
                         AND target.kind = 'captured'
                        JOIN atom_tags at1
                          ON at1.atom_id = se.source_atom_id
                         AND at1.db_id = se.db_id
                        JOIN atom_tags at2
                          ON at2.atom_id = se.target_atom_id
                         AND at2.tag_id = at1.tag_id
                         AND at2.db_id = se.db_id
                        WHERE se.db_id = $2
                        GROUP BY at1.tag_id
                    )
                    SELECT
                        t.id,
                        t.name,
                        COALESCE(ta.atom_count, 0)::BIGINT as atom_count,
                        COALESCE(lm.link_count, 0)::BIGINT as mention_count,
                        COALESCE(ta.source_count, 0)::BIGINT as source_count,
                        COALESCE(ta.substantive_count, 0)::BIGINT as substantive_count,
                        COALESCE(ta.recent_count, 0)::BIGINT as recent_count,
                        COALESCE(ie.edge_count, 0)::BIGINT as edge_count,
                        COALESCE(ie.avg_similarity, 0.0)::FLOAT8 as avg_similarity
                    FROM tags t
                    JOIN tag_atoms ta ON ta.tag_id = t.id
                    LEFT JOIN link_mentions lm ON lm.tag_id = t.id
                    LEFT JOIN intra_edges ie ON ie.tag_id = t.id
                    WHERE t.db_id = $2
                      AND t.parent_id IS NOT NULL
                      AND NOT EXISTS (
                          SELECT 1 FROM wiki_articles wa
                          WHERE wa.tag_id = t.id AND wa.db_id = $2
                      )
                      AND t.name ~ '[^0-9]'
                      AND length(t.name) >= 2
                      AND ta.atom_count > 0",
                )
                .bind(cutoff)
                .bind(&storage.db_id)
                .fetch_all(&storage.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

                let now = Utc::now().to_rfc3339();
                let mut signals = Vec::with_capacity(rows.len());
                for (
                    tag_id,
                    tag_name,
                    atom_count,
                    mention_count,
                    source_count,
                    substantive_count,
                    recent_count,
                    edge_count,
                    avg_similarity,
                ) in rows
                {
                    signals.push(
                        WikiCandidateRow {
                            tag_id,
                            tag_name,
                            atom_count: atom_count as i32,
                            mention_count: mention_count as i32,
                            source_count: source_count as i32,
                            substantive_count: substantive_count as i32,
                            recent_count: recent_count as i32,
                            edge_count: edge_count as i32,
                            avg_similarity: avg_similarity as f32,
                        }
                        .into_signal(&now)?,
                    );
                }
                signals.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
                Ok(signals)
            }
        }
    }
}

struct WikiCandidateRow {
    tag_id: String,
    tag_name: String,
    atom_count: i32,
    mention_count: i32,
    source_count: i32,
    substantive_count: i32,
    recent_count: i32,
    edge_count: i32,
    avg_similarity: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WikiCandidateEvidence {
    pub tag_id: String,
    pub tag_name: String,
    pub atom_count: i32,
    pub mention_count: i32,
    pub source_count: i32,
    pub substantive_count: i32,
    pub recent_count: i32,
    pub semantic_edge_count: i32,
    pub avg_similarity: f32,
}

impl KnowledgeSignalEvidence for WikiCandidateEvidence {
    const SCHEMA: &'static str = "wiki_candidate";
}

impl WikiCandidateRow {
    fn into_signal(self, now: &str) -> Result<KnowledgeSignal, AtomicCoreError> {
        let atom_volume = scaled_ln(self.atom_count, 25.0);
        let source_diversity = if self.atom_count <= 1 {
            0.0
        } else {
            (self.source_count as f32 / self.atom_count.min(10) as f32).min(1.0)
        };
        let substantive = if self.atom_count == 0 {
            0.0
        } else {
            (self.substantive_count as f32 / self.atom_count as f32).min(1.0)
        };
        let recent_growth = (self.recent_count as f32 / 5.0).min(1.0);
        let mention_strength = (self.mention_count as f32 / 5.0).min(1.0);
        let semantic_edge_cohesion = if self.edge_count == 0 {
            0.0
        } else {
            ((self.avg_similarity - 0.5) / 0.35).clamp(0.0, 1.0)
        };

        let score = 100.0
            * (0.30 * atom_volume
                + 0.20 * source_diversity
                + 0.15 * substantive
                + 0.15 * recent_growth
                + 0.10 * mention_strength
                + 0.10 * semantic_edge_cohesion);

        let confidence = (0.35 * atom_volume
            + 0.25 * substantive
            + 0.20 * source_diversity
            + 0.20 * semantic_edge_cohesion)
            .clamp(0.0, 1.0);

        let mut reasons = vec![
            KnowledgeSignalReason {
                kind: "atom_volume".to_string(),
                label: format!(
                    "{} atom{}",
                    self.atom_count,
                    if self.atom_count == 1 { "" } else { "s" }
                ),
                value: json!(self.atom_count),
                contribution: atom_volume,
            },
            KnowledgeSignalReason {
                kind: "source_diversity".to_string(),
                label: format!(
                    "{} distinct source{}",
                    self.source_count,
                    if self.source_count == 1 { "" } else { "s" }
                ),
                value: json!(self.source_count),
                contribution: source_diversity,
            },
        ];

        if self.recent_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "recent_growth".to_string(),
                label: format!("{} added in the last 14 days", self.recent_count),
                value: json!(self.recent_count),
                contribution: recent_growth,
            });
        }

        if self.mention_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "wiki_mentions".to_string(),
                label: format!(
                    "{} wiki mention{}",
                    self.mention_count,
                    if self.mention_count == 1 { "" } else { "s" }
                ),
                value: json!(self.mention_count),
                contribution: mention_strength,
            });
        }

        if self.edge_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "semantic_edge_cohesion".to_string(),
                label: format!("semantic edge cohesion {:.0}%", self.avg_similarity * 100.0),
                value: json!({
                    "edge_count": self.edge_count,
                    "avg_similarity": self.avg_similarity,
                }),
                contribution: semantic_edge_cohesion,
            });
        }

        let evidence = WikiCandidateEvidence {
            tag_id: self.tag_id.clone(),
            tag_name: self.tag_name.clone(),
            atom_count: self.atom_count,
            mention_count: self.mention_count,
            source_count: self.source_count,
            substantive_count: self.substantive_count,
            recent_count: self.recent_count,
            semantic_edge_count: self.edge_count,
            avg_similarity: self.avg_similarity,
        };

        Ok(KnowledgeSignal {
            id: format!("wiki_candidate:tag:{}", self.tag_id),
            provider_id: WIKI_CANDIDATE_PROVIDER_ID.to_string(),
            target: KnowledgeSignalTarget::tag(self.tag_id.clone(), self.tag_name.clone()),
            score,
            confidence,
            severity: KnowledgeSignalSeverity::Opportunity,
            title: format!("Generate a wiki for {}", self.tag_name),
            summary: "Strong candidate for synthesis based on tag usage and source material."
                .to_string(),
            reasons,
            evidence: evidence.to_value()?,
            suggested_actions: vec![
                KnowledgeSignalAction {
                    id: "generate_wiki".to_string(),
                    label: "Generate wiki".to_string(),
                    kind: "wiki".to_string(),
                },
                KnowledgeSignalAction {
                    id: "review_tag".to_string(),
                    label: "Review tag".to_string(),
                    kind: "open".to_string(),
                },
            ],
            created_at: now.to_string(),
            expires_at: None,
        })
    }
}

struct WikiUpdateProvider;

#[async_trait]
impl KnowledgeSignalProvider for WikiUpdateProvider {
    fn id(&self) -> &'static str {
        WIKI_UPDATE_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Wiki updates"
    }

    async fn evaluate(
        &self,
        core: &AtomicCore,
        _config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
        match &core.storage {
            StorageBackend::Sqlite(storage) => {
                let storage = storage.clone();
                tokio::task::spawn_blocking(move || {
                    let recent_cutoff = (Utc::now() - Duration::days(14)).to_rfc3339();
                    let conn = storage.database().read_conn()?;
                    let mut stmt = conn.prepare(
                        "WITH RECURSIVE descendant_tags(root_tag_id, tag_id) AS (
                            SELECT wa.tag_id, wa.tag_id
                            FROM wiki_articles wa
                            UNION ALL
                            SELECT dt.root_tag_id, t.id
                            FROM tags t
                            JOIN descendant_tags dt ON t.parent_id = dt.tag_id
                        ),
                        tag_atoms AS (
                            SELECT
                                dt.root_tag_id as tag_id,
                                COUNT(DISTINCT a.id) as current_atom_count,
                                COUNT(DISTINCT CASE WHEN a.created_at > wa.updated_at THEN a.id END) as new_atom_count,
                                COUNT(DISTINCT CASE
                                    WHEN a.created_at > wa.updated_at
                                     AND a.source_url IS NOT NULL
                                     AND length(trim(a.source_url)) > 0
                                    THEN a.source_url
                                END) as new_source_count,
                                SUM(CASE
                                    WHEN a.created_at > wa.updated_at AND length(trim(a.content)) >= 200
                                    THEN 1 ELSE 0
                                END) as new_substantive_count,
                                SUM(CASE
                                    WHEN a.created_at > wa.updated_at AND a.created_at >= ?1
                                    THEN 1 ELSE 0
                                END) as new_recent_count
                            FROM descendant_tags dt
                            JOIN wiki_articles wa ON wa.tag_id = dt.root_tag_id
                            JOIN atom_tags at ON at.tag_id = dt.tag_id
                            JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
                            GROUP BY dt.root_tag_id
                        ),
                        inbound_mentions AS (
                            SELECT tag_id, SUM(cnt) as inbound_count FROM (
                                SELECT wl.target_tag_id as tag_id, COUNT(*) as cnt
                                FROM wiki_links wl
                                WHERE wl.target_tag_id IS NOT NULL
                                GROUP BY wl.target_tag_id
                                UNION ALL
                                SELECT t2.id as tag_id, COUNT(*) as cnt
                                FROM wiki_links wl
                                JOIN tags t2 ON wl.target_tag_name = t2.name COLLATE NOCASE
                                WHERE wl.target_tag_id IS NULL
                                GROUP BY t2.id
                            )
                            GROUP BY tag_id
                        )
                        SELECT
                            wa.id,
                            wa.tag_id,
                            t.name,
                            wa.atom_count,
                            COALESCE(ta.current_atom_count, 0) as current_atom_count,
                            COALESCE(ta.new_atom_count, 0) as new_atom_count,
                            COALESCE(ta.new_source_count, 0) as new_source_count,
                            COALESCE(ta.new_substantive_count, 0) as new_substantive_count,
                            COALESCE(ta.new_recent_count, 0) as new_recent_count,
                            COALESCE(im.inbound_count, 0) as inbound_link_count,
                            wa.updated_at
                        FROM wiki_articles wa
                        JOIN tags t ON t.id = wa.tag_id
                        LEFT JOIN tag_atoms ta ON ta.tag_id = wa.tag_id
                        LEFT JOIN inbound_mentions im ON im.tag_id = wa.tag_id
                        WHERE COALESCE(ta.new_atom_count, 0) > 0",
                    )?;

                    let rows = stmt.query_map(params![recent_cutoff], |row| {
                        Ok(WikiUpdateRow {
                            article_id: row.get(0)?,
                            tag_id: row.get(1)?,
                            tag_name: row.get(2)?,
                            article_atom_count: row.get(3)?,
                            current_atom_count: row.get(4)?,
                            new_atom_count: row.get(5)?,
                            new_source_count: row.get(6)?,
                            new_substantive_count: row.get(7)?,
                            new_recent_count: row.get(8)?,
                            inbound_link_count: row.get(9)?,
                            updated_at: row.get(10)?,
                        })
                    })?;

                    let now = Utc::now().to_rfc3339();
                    let mut signals = Vec::new();
                    for row in rows {
                        signals.push(row?.into_signal(&now)?);
                    }
                    signals.sort_by(|a, b| {
                        b.score
                            .partial_cmp(&a.score)
                            .unwrap_or(Ordering::Equal)
                    });
                    Ok(signals)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => {
                let recent_cutoff = (Utc::now() - Duration::days(14)).to_rfc3339();
                let rows = sqlx::query_as::<
                    _,
                    (String, String, String, i32, i64, i64, i64, i64, i64, i64, String),
                >(
                    "WITH RECURSIVE descendant_tags(root_tag_id, tag_id) AS (
                        SELECT wa.tag_id, wa.tag_id
                        FROM wiki_articles wa
                        WHERE wa.db_id = $2
                        UNION ALL
                        SELECT dt.root_tag_id, t.id
                        FROM tags t
                        JOIN descendant_tags dt ON t.parent_id = dt.tag_id
                        WHERE t.db_id = $2
                    ),
                    tag_atoms AS (
                        SELECT
                            dt.root_tag_id as tag_id,
                            COUNT(DISTINCT a.id)::BIGINT as current_atom_count,
                            COUNT(DISTINCT CASE WHEN a.created_at > wa.updated_at THEN a.id END)::BIGINT as new_atom_count,
                            COUNT(DISTINCT CASE
                                WHEN a.created_at > wa.updated_at
                                 AND a.source_url IS NOT NULL
                                 AND length(trim(a.source_url)) > 0
                                THEN a.source_url
                            END)::BIGINT as new_source_count,
                            SUM(CASE
                                WHEN a.created_at > wa.updated_at AND length(trim(a.content)) >= 200
                                THEN 1 ELSE 0
                            END)::BIGINT as new_substantive_count,
                            SUM(CASE
                                WHEN a.created_at > wa.updated_at AND a.created_at >= $1
                                THEN 1 ELSE 0
                            END)::BIGINT as new_recent_count
                        FROM descendant_tags dt
                        JOIN wiki_articles wa ON wa.tag_id = dt.root_tag_id AND wa.db_id = $2
                        JOIN atom_tags at ON at.tag_id = dt.tag_id AND at.db_id = $2
                        JOIN atoms a ON a.id = at.atom_id AND a.db_id = $2 AND a.kind = 'captured'
                        GROUP BY dt.root_tag_id
                    ),
                    inbound_mentions AS (
                        SELECT wl.target_tag_id as tag_id, COUNT(*)::BIGINT as inbound_count
                        FROM wiki_links wl
                        WHERE wl.target_tag_id IS NOT NULL
                          AND wl.db_id = $2
                        GROUP BY wl.target_tag_id
                    )
                    SELECT
                        wa.id,
                        wa.tag_id,
                        t.name,
                        wa.atom_count,
                        COALESCE(ta.current_atom_count, 0)::BIGINT as current_atom_count,
                        COALESCE(ta.new_atom_count, 0)::BIGINT as new_atom_count,
                        COALESCE(ta.new_source_count, 0)::BIGINT as new_source_count,
                        COALESCE(ta.new_substantive_count, 0)::BIGINT as new_substantive_count,
                        COALESCE(ta.new_recent_count, 0)::BIGINT as new_recent_count,
                        COALESCE(im.inbound_count, 0)::BIGINT as inbound_link_count,
                        wa.updated_at
                    FROM wiki_articles wa
                    JOIN tags t ON t.id = wa.tag_id AND t.db_id = $2
                    LEFT JOIN tag_atoms ta ON ta.tag_id = wa.tag_id
                    LEFT JOIN inbound_mentions im ON im.tag_id = wa.tag_id
                    WHERE wa.db_id = $2
                      AND COALESCE(ta.new_atom_count, 0) > 0",
                )
                .bind(recent_cutoff)
                .bind(&storage.db_id)
                .fetch_all(&storage.pool)
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

                let now = Utc::now().to_rfc3339();
                let mut signals = Vec::with_capacity(rows.len());
                for (
                    article_id,
                    tag_id,
                    tag_name,
                    article_atom_count,
                    current_atom_count,
                    new_atom_count,
                    new_source_count,
                    new_substantive_count,
                    new_recent_count,
                    inbound_link_count,
                    updated_at,
                ) in rows
                {
                    signals.push(
                        WikiUpdateRow {
                            article_id,
                            tag_id,
                            tag_name,
                            article_atom_count,
                            current_atom_count: current_atom_count as i32,
                            new_atom_count: new_atom_count as i32,
                            new_source_count: new_source_count as i32,
                            new_substantive_count: new_substantive_count as i32,
                            new_recent_count: new_recent_count as i32,
                            inbound_link_count: inbound_link_count as i32,
                            updated_at,
                        }
                        .into_signal(&now)?,
                    );
                }
                signals.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
                Ok(signals)
            }
        }
    }
}

struct WikiUpdateRow {
    article_id: String,
    tag_id: String,
    tag_name: String,
    article_atom_count: i32,
    current_atom_count: i32,
    new_atom_count: i32,
    new_source_count: i32,
    new_substantive_count: i32,
    new_recent_count: i32,
    inbound_link_count: i32,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct WikiUpdateEvidence {
    pub article_id: String,
    pub tag_id: String,
    pub tag_name: String,
    pub article_atom_count: i32,
    pub current_atom_count: i32,
    pub new_atom_count: i32,
    pub new_source_count: i32,
    pub new_substantive_count: i32,
    pub new_recent_count: i32,
    pub inbound_link_count: i32,
    pub updated_at: String,
}

impl KnowledgeSignalEvidence for WikiUpdateEvidence {
    const SCHEMA: &'static str = "wiki_update";
}

impl WikiUpdateRow {
    fn into_signal(self, now: &str) -> Result<KnowledgeSignal, AtomicCoreError> {
        let new_atom_volume = scaled_ln(self.new_atom_count, 12.0);
        let growth_ratio = if self.article_atom_count <= 0 {
            1.0
        } else {
            (self.new_atom_count as f32 / self.article_atom_count as f32 / 0.5).min(1.0)
        };
        let source_diversity = if self.new_atom_count <= 1 {
            0.0
        } else {
            (self.new_source_count as f32 / self.new_atom_count.min(8) as f32).min(1.0)
        };
        let substantive = if self.new_atom_count == 0 {
            0.0
        } else {
            (self.new_substantive_count as f32 / self.new_atom_count as f32).min(1.0)
        };
        let recent_growth = (self.new_recent_count as f32 / 5.0).min(1.0);
        let inbound_strength = (self.inbound_link_count as f32 / 5.0).min(1.0);

        let score = 100.0
            * (0.35 * new_atom_volume
                + 0.20 * growth_ratio
                + 0.15 * source_diversity
                + 0.15 * substantive
                + 0.10 * recent_growth
                + 0.05 * inbound_strength);

        let confidence = (0.40 * new_atom_volume
            + 0.25 * substantive
            + 0.20 * source_diversity
            + 0.15 * growth_ratio)
            .clamp(0.0, 1.0);

        let mut reasons = vec![
            KnowledgeSignalReason {
                kind: "new_atom_volume".to_string(),
                label: format!(
                    "{} new atom{}",
                    self.new_atom_count,
                    if self.new_atom_count == 1 { "" } else { "s" }
                ),
                value: json!(self.new_atom_count),
                contribution: new_atom_volume,
            },
            KnowledgeSignalReason {
                kind: "growth_ratio".to_string(),
                label: format!(
                    "+{:.0}% since last update",
                    if self.article_atom_count <= 0 {
                        100.0
                    } else {
                        (self.new_atom_count as f32 / self.article_atom_count as f32) * 100.0
                    }
                ),
                value: json!({
                    "article_atom_count": self.article_atom_count,
                    "new_atom_count": self.new_atom_count,
                }),
                contribution: growth_ratio,
            },
        ];

        if self.new_source_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "new_source_diversity".to_string(),
                label: format!(
                    "{} new source{}",
                    self.new_source_count,
                    if self.new_source_count == 1 { "" } else { "s" }
                ),
                value: json!(self.new_source_count),
                contribution: source_diversity,
            });
        }

        if self.new_recent_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "recent_growth".to_string(),
                label: format!("{} added in the last 14 days", self.new_recent_count),
                value: json!(self.new_recent_count),
                contribution: recent_growth,
            });
        }

        if self.inbound_link_count > 0 {
            reasons.push(KnowledgeSignalReason {
                kind: "inbound_wiki_links".to_string(),
                label: format!(
                    "{} inbound wiki link{}",
                    self.inbound_link_count,
                    if self.inbound_link_count == 1 {
                        ""
                    } else {
                        "s"
                    }
                ),
                value: json!(self.inbound_link_count),
                contribution: inbound_strength,
            });
        }

        let evidence = WikiUpdateEvidence {
            article_id: self.article_id.clone(),
            tag_id: self.tag_id.clone(),
            tag_name: self.tag_name.clone(),
            article_atom_count: self.article_atom_count,
            current_atom_count: self.current_atom_count,
            new_atom_count: self.new_atom_count,
            new_source_count: self.new_source_count,
            new_substantive_count: self.new_substantive_count,
            new_recent_count: self.new_recent_count,
            inbound_link_count: self.inbound_link_count,
            updated_at: self.updated_at.clone(),
        };

        Ok(KnowledgeSignal {
            id: format!("wiki_update:tag:{}", self.tag_id),
            provider_id: WIKI_UPDATE_PROVIDER_ID.to_string(),
            target: KnowledgeSignalTarget::tag(self.tag_id.clone(), self.tag_name.clone()),
            score,
            confidence,
            severity: KnowledgeSignalSeverity::Opportunity,
            title: format!("Update the wiki for {}", self.tag_name),
            summary: "New source material has accumulated since this wiki was last updated."
                .to_string(),
            reasons,
            evidence: evidence.to_value()?,
            suggested_actions: vec![
                KnowledgeSignalAction {
                    id: "update_wiki".to_string(),
                    label: "Update wiki".to_string(),
                    kind: "wiki".to_string(),
                },
                KnowledgeSignalAction {
                    id: "review_tag".to_string(),
                    label: "Review tag".to_string(),
                    kind: "open".to_string(),
                },
            ],
            created_at: now.to_string(),
            expires_at: None,
        })
    }
}

struct TagRedundancyProvider;

#[async_trait]
impl KnowledgeSignalProvider for TagRedundancyProvider {
    fn id(&self) -> &'static str {
        TAG_REDUNDANCY_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Tag redundancy"
    }

    async fn evaluate(
        &self,
        core: &AtomicCore,
        _config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
        match &core.storage {
            StorageBackend::Sqlite(storage) => {
                let storage = storage.clone();
                tokio::task::spawn_blocking(move || {
                    let conn = storage.database().read_conn()?;
                    evaluate_tag_redundancy_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => evaluate_tag_redundancy_postgres(storage).await,
        }
    }
}

struct EmptyTagProvider;

#[async_trait]
impl KnowledgeSignalProvider for EmptyTagProvider {
    fn id(&self) -> &'static str {
        EMPTY_TAG_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Empty tags"
    }

    async fn evaluate(
        &self,
        core: &AtomicCore,
        _config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
        match &core.storage {
            StorageBackend::Sqlite(storage) => {
                let storage = storage.clone();
                tokio::task::spawn_blocking(move || {
                    let conn = storage.database().read_conn()?;
                    evaluate_empty_tags_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => evaluate_empty_tags_postgres(storage).await,
        }
    }
}

struct MissingTagOverlapProvider;

#[async_trait]
impl KnowledgeSignalProvider for MissingTagOverlapProvider {
    fn id(&self) -> &'static str {
        MISSING_TAG_OVERLAP_PROVIDER_ID
    }

    fn name(&self) -> &'static str {
        "Missing tag overlap"
    }

    async fn evaluate(
        &self,
        core: &AtomicCore,
        _config: &KnowledgeSignalProviderConfig,
    ) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
        match &core.storage {
            StorageBackend::Sqlite(storage) => {
                let storage = storage.clone();
                tokio::task::spawn_blocking(move || {
                    let conn = storage.database().read_conn()?;
                    evaluate_missing_tag_overlap_sqlite(&conn)
                })
                .await
                .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?
            }
            #[cfg(feature = "postgres")]
            StorageBackend::Postgres(storage) => {
                evaluate_missing_tag_overlap_postgres(storage).await
            }
        }
    }
}

#[derive(Debug, Clone)]
struct TagCleanupTag {
    id: String,
    name: String,
    parent_id: Option<String>,
    path: Vec<String>,
    atom_count: i32,
    child_count: i32,
    has_wiki: bool,
    is_autotag_target: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TagCleanupTagEvidence {
    pub id: String,
    pub name: String,
    pub parent_id: Option<String>,
    pub path: Vec<String>,
    pub atom_count: i32,
    pub child_count: i32,
    pub has_wiki: bool,
    pub is_autotag_target: bool,
}

impl From<&TagCleanupTag> for TagCleanupTagEvidence {
    fn from(tag: &TagCleanupTag) -> Self {
        Self {
            id: tag.id.clone(),
            name: tag.name.clone(),
            parent_id: tag.parent_id.clone(),
            path: tag.path.clone(),
            atom_count: tag.atom_count,
            child_count: tag.child_count,
            has_wiki: tag.has_wiki,
            is_autotag_target: tag.is_autotag_target,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct TagRedundancyEvidence {
    pub primary_tag: TagCleanupTagEvidence,
    pub secondary_tag: TagCleanupTagEvidence,
    pub shared_atom_count: i32,
    pub primary_unique_atom_count: i32,
    pub secondary_unique_atom_count: i32,
    pub jaccard_overlap: f32,
    pub containment_overlap: f32,
    pub centroid_similarity: Option<f32>,
    pub name_similarity: f32,
    pub hierarchy_relationship: String,
    pub review_posture: String,
}

impl KnowledgeSignalEvidence for TagRedundancyEvidence {
    const SCHEMA: &'static str = "tag_redundancy";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct EmptyTagEvidence {
    pub tag: TagCleanupTagEvidence,
}

impl KnowledgeSignalEvidence for EmptyTagEvidence {
    const SCHEMA: &'static str = "empty_tag";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "openapi", derive(utoipa::ToSchema))]
pub struct MissingTagOverlapEvidence {
    pub atom_id: String,
    pub atom_title: String,
    pub current_tag_count: i32,
    pub suggested_tag: TagCleanupTagEvidence,
    pub nearby_tagged_atom_count: i32,
    pub strongest_similarity: f32,
    pub average_similarity: f32,
}

impl KnowledgeSignalEvidence for MissingTagOverlapEvidence {
    const SCHEMA: &'static str = "missing_tag_overlap";
}

#[derive(Debug, Clone)]
struct TagPairCandidate {
    tag_a_id: String,
    tag_b_id: String,
    shared_atom_count: i32,
}

#[derive(Debug, Clone)]
struct MissingTagCandidate {
    atom_id: String,
    atom_title: String,
    tag_id: String,
    nearby_tagged_atom_count: i32,
    strongest_similarity: f32,
    average_similarity: f32,
}

fn evaluate_tag_redundancy_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_sqlite_tag_cleanup_tags(conn)?;
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT at1.tag_id, at2.tag_id, COUNT(*) as shared_count
         FROM atom_tags at1
         JOIN atom_tags at2
           ON at1.atom_id = at2.atom_id
          AND at1.tag_id < at2.tag_id
         JOIN atoms a ON a.id = at1.atom_id AND a.kind = 'captured'
         GROUP BY at1.tag_id, at2.tag_id
         HAVING COUNT(*) >= 3",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(TagPairCandidate {
            tag_a_id: row.get(0)?,
            tag_b_id: row.get(1)?,
            shared_atom_count: row.get(2)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        let candidate = row?;
        let Some(a) = tags.get(&candidate.tag_a_id) else {
            continue;
        };
        let Some(b) = tags.get(&candidate.tag_b_id) else {
            continue;
        };
        if let Some(signal) = tag_pair_signal(a, b, candidate.shared_atom_count, None, &now)? {
            out.push(signal);
        }
    }
    Ok(out)
}

fn evaluate_empty_tags_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_sqlite_tag_cleanup_tags(conn)?;
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "SELECT t.id
         FROM tags t
         LEFT JOIN atom_tags at ON at.tag_id = t.id
         LEFT JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
         LEFT JOIN tags child ON child.parent_id = t.id
         GROUP BY t.id
         HAVING COUNT(DISTINCT a.id) = 0
            AND COUNT(DISTINCT child.id) = 0",
    )?;
    let ids = stmt
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut out = Vec::new();
    for id in ids {
        let Some(tag) = tags.get(&id) else {
            continue;
        };
        if is_structural_tag(tag) {
            continue;
        }
        out.push(empty_tag_signal(tag, &now)?);
    }
    Ok(out)
}

fn evaluate_missing_tag_overlap_sqlite(
    conn: &rusqlite::Connection,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_sqlite_tag_cleanup_tags(conn)?;
    let current_tags = load_sqlite_atom_tag_ids(conn)?;
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "WITH neighbor_edges AS (
            SELECT source_atom_id as atom_id, target_atom_id as neighbor_atom_id, similarity_score
            FROM semantic_edges
            WHERE similarity_score >= 0.55
            UNION ALL
            SELECT target_atom_id as atom_id, source_atom_id as neighbor_atom_id, similarity_score
            FROM semantic_edges
            WHERE similarity_score >= 0.55
         )
         SELECT
            ne.atom_id,
            a.title,
            nt.tag_id,
            COUNT(DISTINCT ne.neighbor_atom_id) as nearby_tagged_atom_count,
            MAX(ne.similarity_score) as strongest_similarity,
            AVG(ne.similarity_score) as average_similarity
         FROM neighbor_edges ne
         JOIN atoms a ON a.id = ne.atom_id AND a.kind = 'captured'
         JOIN atoms neighbor ON neighbor.id = ne.neighbor_atom_id AND neighbor.kind = 'captured'
         JOIN atom_tags nt ON nt.atom_id = ne.neighbor_atom_id
         LEFT JOIN atom_tags existing
           ON existing.atom_id = ne.atom_id
          AND existing.tag_id = nt.tag_id
         WHERE existing.tag_id IS NULL
         GROUP BY ne.atom_id, a.title, nt.tag_id
         HAVING COUNT(DISTINCT ne.neighbor_atom_id) >= 3",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(MissingTagCandidate {
            atom_id: row.get(0)?,
            atom_title: row.get(1)?,
            tag_id: row.get(2)?,
            nearby_tagged_atom_count: row.get(3)?,
            strongest_similarity: row.get::<_, f32>(4)?,
            average_similarity: row.get::<_, f32>(5)?,
        })
    })?;

    let mut out = Vec::new();
    for row in rows {
        let candidate = row?;
        let Some(tag) = tags.get(&candidate.tag_id) else {
            continue;
        };
        let atom_tag_ids = current_tags
            .get(&candidate.atom_id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if let Some(signal) = missing_tag_signal(&candidate, tag, atom_tag_ids, &tags, &now)? {
            out.push(signal);
        }
    }
    limit_missing_tag_signals(out)
}

fn load_sqlite_atom_tag_ids(
    conn: &rusqlite::Connection,
) -> Result<HashMap<String, Vec<String>>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT at.atom_id, at.tag_id
         FROM atom_tags at
         JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (atom_id, tag_id) = row?;
        out.entry(atom_id).or_default().push(tag_id);
    }
    Ok(out)
}

fn load_sqlite_tag_cleanup_tags(
    conn: &rusqlite::Connection,
) -> Result<HashMap<String, TagCleanupTag>, AtomicCoreError> {
    let mut stmt = conn.prepare(
        "SELECT
            t.id,
            t.name,
            t.parent_id,
            COALESCE(COUNT(DISTINCT a.id), 0) as atom_count,
            COALESCE(COUNT(DISTINCT child.id), 0) as child_count,
            EXISTS(SELECT 1 FROM wiki_articles w WHERE w.tag_id = t.id) as has_wiki,
            COALESCE(t.is_autotag_target, 0) as is_autotag_target
         FROM tags t
         LEFT JOIN atom_tags at ON at.tag_id = t.id
         LEFT JOIN atoms a ON a.id = at.atom_id AND a.kind = 'captured'
         LEFT JOIN tags child ON child.parent_id = t.id
         GROUP BY t.id, t.name, t.parent_id, t.is_autotag_target",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i32>(3)?,
            row.get::<_, i32>(4)?,
            row.get::<_, i32>(5)? != 0,
            row.get::<_, i32>(6)? != 0,
        ))
    })?;

    let mut raw = HashMap::new();
    for row in rows {
        let (id, name, parent_id, atom_count, child_count, has_wiki, is_autotag_target) = row?;
        raw.insert(
            id,
            (
                name,
                parent_id,
                atom_count,
                child_count,
                has_wiki,
                is_autotag_target,
            ),
        );
    }
    Ok(build_tag_cleanup_tags(raw))
}

#[cfg(feature = "postgres")]
async fn evaluate_tag_redundancy_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_postgres_tag_cleanup_tags(storage).await?;
    let now = Utc::now().to_rfc3339();
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT at1.tag_id, at2.tag_id, COUNT(*) as shared_count
         FROM atom_tags at1
         JOIN atom_tags at2
           ON at1.atom_id = at2.atom_id
          AND at1.tag_id < at2.tag_id
          AND at1.db_id = at2.db_id
         JOIN atoms a
           ON a.id = at1.atom_id
          AND a.db_id = at1.db_id
          AND a.kind = 'captured'
         WHERE at1.db_id = $1
         GROUP BY at1.tag_id, at2.tag_id
         HAVING COUNT(*) >= 3",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut out = Vec::new();
    for (tag_a_id, tag_b_id, shared_atom_count) in rows {
        let Some(a) = tags.get(&tag_a_id) else {
            continue;
        };
        let Some(b) = tags.get(&tag_b_id) else {
            continue;
        };
        if let Some(signal) = tag_pair_signal(a, b, shared_atom_count as i32, None, &now)? {
            out.push(signal);
        }
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn evaluate_empty_tags_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_postgres_tag_cleanup_tags(storage).await?;
    let now = Utc::now().to_rfc3339();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT t.id
         FROM tags t
         LEFT JOIN atom_tags at ON at.tag_id = t.id AND at.db_id = t.db_id
         LEFT JOIN atoms a ON a.id = at.atom_id AND a.db_id = t.db_id AND a.kind = 'captured'
         LEFT JOIN tags child ON child.parent_id = t.id AND child.db_id = t.db_id
         WHERE t.db_id = $1
         GROUP BY t.id
         HAVING COUNT(DISTINCT a.id) = 0
            AND COUNT(DISTINCT child.id) = 0",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut out = Vec::new();
    for id in rows {
        let Some(tag) = tags.get(&id) else {
            continue;
        };
        if is_structural_tag(tag) {
            continue;
        }
        out.push(empty_tag_signal(tag, &now)?);
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn evaluate_missing_tag_overlap_postgres(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    let tags = load_postgres_tag_cleanup_tags(storage).await?;
    let current_tags = load_postgres_atom_tag_ids(storage).await?;
    let now = Utc::now().to_rfc3339();
    let rows: Vec<(String, String, String, i64, f32, f32)> = sqlx::query_as(
        "WITH neighbor_edges AS (
            SELECT source_atom_id as atom_id, target_atom_id as neighbor_atom_id, similarity_score
            FROM semantic_edges
            WHERE similarity_score >= 0.55 AND db_id = $1
            UNION ALL
            SELECT target_atom_id as atom_id, source_atom_id as neighbor_atom_id, similarity_score
            FROM semantic_edges
            WHERE similarity_score >= 0.55 AND db_id = $1
         )
         SELECT
            ne.atom_id,
            a.title,
            nt.tag_id,
            COUNT(DISTINCT ne.neighbor_atom_id)::BIGINT as nearby_tagged_atom_count,
            MAX(ne.similarity_score)::REAL as strongest_similarity,
            AVG(ne.similarity_score)::REAL as average_similarity
         FROM neighbor_edges ne
         JOIN atoms a ON a.id = ne.atom_id AND a.db_id = $1 AND a.kind = 'captured'
         JOIN atoms neighbor
           ON neighbor.id = ne.neighbor_atom_id
          AND neighbor.db_id = $1
          AND neighbor.kind = 'captured'
         JOIN atom_tags nt ON nt.atom_id = ne.neighbor_atom_id AND nt.db_id = $1
         LEFT JOIN atom_tags existing
           ON existing.atom_id = ne.atom_id
          AND existing.tag_id = nt.tag_id
          AND existing.db_id = $1
         WHERE existing.tag_id IS NULL
         GROUP BY ne.atom_id, a.title, nt.tag_id
         HAVING COUNT(DISTINCT ne.neighbor_atom_id) >= 3",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let mut out = Vec::new();
    for (
        atom_id,
        atom_title,
        tag_id,
        nearby_tagged_atom_count,
        strongest_similarity,
        average_similarity,
    ) in rows
    {
        let Some(tag) = tags.get(&tag_id) else {
            continue;
        };
        let atom_tag_ids = current_tags.get(&atom_id).map(Vec::as_slice).unwrap_or(&[]);
        let candidate = MissingTagCandidate {
            atom_id,
            atom_title,
            tag_id,
            nearby_tagged_atom_count: nearby_tagged_atom_count as i32,
            strongest_similarity,
            average_similarity,
        };
        if let Some(signal) = missing_tag_signal(&candidate, tag, atom_tag_ids, &tags, &now)? {
            out.push(signal);
        }
    }
    limit_missing_tag_signals(out)
}

#[cfg(feature = "postgres")]
async fn load_postgres_atom_tag_ids(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<HashMap<String, Vec<String>>, AtomicCoreError> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT at.atom_id, at.tag_id
             FROM atom_tags at
             JOIN atoms a ON a.id = at.atom_id AND a.db_id = at.db_id AND a.kind = 'captured'
             WHERE at.db_id = $1",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (atom_id, tag_id) in rows {
        out.entry(atom_id).or_default().push(tag_id);
    }
    Ok(out)
}

#[cfg(feature = "postgres")]
async fn load_postgres_tag_cleanup_tags(
    storage: &crate::storage::postgres::PostgresStorage,
) -> Result<HashMap<String, TagCleanupTag>, AtomicCoreError> {
    let rows: Vec<(String, String, Option<String>, i64, i64, bool, bool)> = sqlx::query_as(
        "SELECT
            t.id,
            t.name,
            t.parent_id,
            COALESCE(COUNT(DISTINCT a.id), 0) as atom_count,
            COALESCE(COUNT(DISTINCT child.id), 0) as child_count,
            EXISTS(
                SELECT 1
                FROM wiki_articles w
                WHERE w.tag_id = t.id AND w.db_id = t.db_id
            ) as has_wiki,
            t.is_autotag_target
         FROM tags t
         LEFT JOIN atom_tags at ON at.tag_id = t.id AND at.db_id = t.db_id
         LEFT JOIN atoms a ON a.id = at.atom_id AND a.db_id = t.db_id AND a.kind = 'captured'
         LEFT JOIN tags child ON child.parent_id = t.id AND child.db_id = t.db_id
         WHERE t.db_id = $1
         GROUP BY t.id, t.name, t.parent_id, t.db_id, t.is_autotag_target",
    )
    .bind(&storage.db_id)
    .fetch_all(&storage.pool)
    .await
    .map_err(|e| AtomicCoreError::DatabaseOperation(e.to_string()))?;

    let raw = rows
        .into_iter()
        .map(
            |(id, name, parent_id, atom_count, child_count, has_wiki, is_autotag_target)| {
                (
                    id,
                    (
                        name,
                        parent_id,
                        atom_count as i32,
                        child_count as i32,
                        has_wiki,
                        is_autotag_target,
                    ),
                )
            },
        )
        .collect();
    Ok(build_tag_cleanup_tags(raw))
}

fn build_tag_cleanup_tags(
    raw: HashMap<String, (String, Option<String>, i32, i32, bool, bool)>,
) -> HashMap<String, TagCleanupTag> {
    fn path_for(
        id: &str,
        raw: &HashMap<String, (String, Option<String>, i32, i32, bool, bool)>,
        memo: &mut HashMap<String, Vec<String>>,
    ) -> Vec<String> {
        if let Some(path) = memo.get(id) {
            return path.clone();
        }
        let Some((name, parent_id, _, _, _, _)) = raw.get(id) else {
            return Vec::new();
        };
        let mut path = parent_id
            .as_deref()
            .map(|parent| path_for(parent, raw, memo))
            .unwrap_or_default();
        path.push(name.clone());
        memo.insert(id.to_string(), path.clone());
        path
    }

    let mut memo = HashMap::new();
    raw.iter()
        .map(
            |(id, (name, parent_id, atom_count, child_count, has_wiki, is_autotag_target))| {
                (
                    id.clone(),
                    TagCleanupTag {
                        id: id.clone(),
                        name: name.clone(),
                        parent_id: parent_id.clone(),
                        path: path_for(id, &raw, &mut memo),
                        atom_count: *atom_count,
                        child_count: *child_count,
                        has_wiki: *has_wiki,
                        is_autotag_target: *is_autotag_target,
                    },
                )
            },
        )
        .collect()
}

fn missing_tag_signal(
    candidate: &MissingTagCandidate,
    tag: &TagCleanupTag,
    current_tag_ids: &[String],
    tags: &HashMap<String, TagCleanupTag>,
    now: &str,
) -> Result<Option<KnowledgeSignal>, AtomicCoreError> {
    if is_structural_tag(tag) {
        return Ok(None);
    }
    if tag.atom_count < 5 && candidate.average_similarity < 0.70 {
        return Ok(None);
    }
    if current_tag_ids
        .iter()
        .any(|current| tags_are_hierarchically_related(current, &tag.id, tags))
    {
        return Ok(None);
    }

    let tag_count_penalty = if current_tag_ids.len() >= 8 {
        0.85
    } else {
        1.0
    };
    let neighbor_strength = (candidate.nearby_tagged_atom_count as f32 / 5.0).min(1.0);
    let tag_size = scaled_ln(tag.atom_count, 25.0);
    let score = (100.0
        * (0.42 * candidate.average_similarity
            + 0.22 * candidate.strongest_similarity
            + 0.24 * neighbor_strength
            + 0.12 * tag_size)
        * tag_count_penalty)
        .clamp(0.0, 100.0);
    let confidence = (0.48 * candidate.average_similarity
        + 0.24 * candidate.strongest_similarity
        + 0.20 * neighbor_strength
        + 0.08 * if current_tag_ids.len() <= 6 { 1.0 } else { 0.5 })
    .clamp(0.0, 1.0);

    let evidence = MissingTagOverlapEvidence {
        atom_id: candidate.atom_id.clone(),
        atom_title: candidate.atom_title.clone(),
        current_tag_count: current_tag_ids.len() as i32,
        suggested_tag: tag.into(),
        nearby_tagged_atom_count: candidate.nearby_tagged_atom_count,
        strongest_similarity: candidate.strongest_similarity,
        average_similarity: candidate.average_similarity,
    };

    Ok(Some(KnowledgeSignal {
        id: format!(
            "{}:atom_tag:{}:{}",
            MISSING_TAG_OVERLAP_PROVIDER_ID, candidate.atom_id, tag.id
        ),
        provider_id: MISSING_TAG_OVERLAP_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::atom(
            candidate.atom_id.clone(),
            candidate.atom_title.clone(),
        ),
        score,
        confidence,
        severity: KnowledgeSignalSeverity::Opportunity,
        title: format!("Add {} to {}", tag.name, candidate.atom_title),
        summary: format!(
            "{} nearby atoms use {}, but this atom does not.",
            candidate.nearby_tagged_atom_count, tag.name
        ),
        reasons: vec![
            KnowledgeSignalReason {
                kind: "nearby_tagged_atoms".to_string(),
                label: format!(
                    "{} nearby atoms use this tag",
                    candidate.nearby_tagged_atom_count
                ),
                value: json!(candidate.nearby_tagged_atom_count),
                contribution: neighbor_strength * 100.0,
            },
            KnowledgeSignalReason {
                kind: "average_similarity".to_string(),
                label: format!(
                    "{:.0}% average similarity",
                    candidate.average_similarity * 100.0
                ),
                value: json!(candidate.average_similarity),
                contribution: candidate.average_similarity * 100.0,
            },
            KnowledgeSignalReason {
                kind: "tag_size".to_string(),
                label: format!("{} atoms already tagged", tag.atom_count),
                value: json!(tag.atom_count),
                contribution: tag_size * 100.0,
            },
        ],
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "add_tag_to_atom".to_string(),
                label: "Add tag".to_string(),
                kind: "update".to_string(),
            },
            KnowledgeSignalAction {
                id: "open_atom".to_string(),
                label: "Open atom".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "dismiss".to_string(),
                label: "Dismiss".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    }))
}

fn limit_missing_tag_signals(
    mut signals: Vec<KnowledgeSignal>,
) -> Result<Vec<KnowledgeSignal>, AtomicCoreError> {
    signals.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal)
            })
    });

    let mut per_atom: HashMap<String, usize> = HashMap::new();
    signals.retain(|signal| {
        let count = per_atom.entry(signal.target.id.clone()).or_default();
        if *count >= 2 {
            return false;
        }
        *count += 1;
        true
    });
    signals.truncate(100);
    Ok(signals)
}

fn tag_pair_signal(
    a: &TagCleanupTag,
    b: &TagCleanupTag,
    shared_atom_count: i32,
    centroid_similarity: Option<f32>,
    now: &str,
) -> Result<Option<KnowledgeSignal>, AtomicCoreError> {
    if is_structural_tag(a) || is_structural_tag(b) {
        return Ok(None);
    }

    let min_count = a.atom_count.min(b.atom_count).max(0);
    let union = (a.atom_count + b.atom_count - shared_atom_count).max(1);
    let jaccard = shared_atom_count as f32 / union as f32;
    let containment = if min_count == 0 {
        0.0
    } else {
        shared_atom_count as f32 / min_count as f32
    };
    let name_similarity = name_similarity(&a.name, &b.name);
    let relationship = hierarchy_relationship(a, b);

    if min_count < 5 && !(containment >= 1.0 && name_similarity >= 0.75) {
        return Ok(None);
    }
    if matches!(
        relationship.as_str(),
        "parent_child" | "ancestor_descendant" | "cross_category"
    ) && jaccard < 0.90
    {
        return Ok(None);
    }

    let duplicate_like = jaccard >= 0.80
        || (jaccard >= 0.65
            && name_similarity >= 0.70
            && matches!(relationship.as_str(), "sibling" | "unrelated"));
    let subsumed_like = containment >= 0.85
        && !matches!(
            relationship.as_str(),
            "parent_child" | "ancestor_descendant"
        );

    if !duplicate_like && !subsumed_like {
        return Ok(None);
    }

    let review_posture = if duplicate_like {
        "possible_duplicate"
    } else {
        "possible_subsumption"
    };
    let hierarchy_boost = match relationship.as_str() {
        "sibling" => 0.12,
        "unrelated" => 0.06,
        "cross_category" => -0.20,
        _ => -0.10,
    };
    let semantic = centroid_similarity.unwrap_or(0.0).max(0.0);
    let score = if duplicate_like {
        100.0
            * (0.58 * jaccard
                + 0.16 * containment
                + 0.12 * name_similarity
                + 0.08 * semantic
                + hierarchy_boost)
    } else {
        100.0
            * (0.62 * containment
                + 0.12 * jaccard
                + 0.10 * name_similarity
                + 0.08 * semantic
                + hierarchy_boost)
    }
    .clamp(0.0, 100.0);

    let confidence = (0.45 * jaccard.max(containment)
        + 0.20 * (shared_atom_count as f32 / 12.0).min(1.0)
        + 0.15 * name_similarity
        + 0.10 * semantic
        + 0.10
            * if matches!(relationship.as_str(), "sibling" | "unrelated") {
                1.0
            } else {
                0.4
            })
    .clamp(0.0, 1.0);

    let (primary, secondary) = if a.atom_count >= b.atom_count {
        (a, b)
    } else {
        (b, a)
    };
    let evidence = TagRedundancyEvidence {
        primary_tag: primary.into(),
        secondary_tag: secondary.into(),
        shared_atom_count,
        primary_unique_atom_count: (primary.atom_count - shared_atom_count).max(0),
        secondary_unique_atom_count: (secondary.atom_count - shared_atom_count).max(0),
        jaccard_overlap: jaccard,
        containment_overlap: containment,
        centroid_similarity,
        name_similarity,
        hierarchy_relationship: relationship.clone(),
        review_posture: review_posture.to_string(),
    };

    let title = if duplicate_like {
        format!("Review similar tags: {} and {}", a.name, b.name)
    } else {
        format!("Review overlapping tags: {} and {}", a.name, b.name)
    };
    let mut reasons = vec![
        KnowledgeSignalReason {
            kind: "shared_atoms".to_string(),
            label: format!("{shared_atom_count} shared atoms"),
            value: json!(shared_atom_count),
            contribution: shared_atom_count as f32,
        },
        KnowledgeSignalReason {
            kind: "overlap".to_string(),
            label: format!("{:.0}% overlap", jaccard * 100.0),
            value: json!(jaccard),
            contribution: jaccard * 100.0,
        },
    ];
    if containment >= 0.85 && !duplicate_like {
        reasons.push(KnowledgeSignalReason {
            kind: "containment".to_string(),
            label: "one tag is mostly contained in the other".to_string(),
            value: json!(containment),
            contribution: containment * 100.0,
        });
    }
    if matches!(relationship.as_str(), "sibling" | "unrelated") {
        reasons.push(KnowledgeSignalReason {
            kind: "hierarchy".to_string(),
            label: relationship.replace('_', " "),
            value: json!(relationship),
            contribution: 10.0,
        });
    }

    Ok(Some(KnowledgeSignal {
        id: tag_pair_signal_key(&a.id, &b.id),
        provider_id: TAG_REDUNDANCY_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::tag(primary.id.clone(), primary.name.clone()),
        score,
        confidence,
        severity: KnowledgeSignalSeverity::Review,
        title,
        summary: "These tags share enough atom membership to be worth reviewing together."
            .to_string(),
        reasons,
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "review_overlap".to_string(),
                label: "Review overlap".to_string(),
                kind: "open".to_string(),
            },
            KnowledgeSignalAction {
                id: "merge_tags".to_string(),
                label: "Merge tags".to_string(),
                kind: "merge".to_string(),
            },
            KnowledgeSignalAction {
                id: "keep_separate".to_string(),
                label: "Keep separate".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    }))
}

fn empty_tag_signal(tag: &TagCleanupTag, now: &str) -> Result<KnowledgeSignal, AtomicCoreError> {
    let evidence = EmptyTagEvidence { tag: tag.into() };
    Ok(KnowledgeSignal {
        id: format!("empty_tag:tag:{}", tag.id),
        provider_id: EMPTY_TAG_PROVIDER_ID.to_string(),
        target: KnowledgeSignalTarget::tag(tag.id.clone(), tag.name.clone()),
        score: 35.0,
        confidence: 1.0,
        severity: KnowledgeSignalSeverity::Review,
        title: format!("Review empty tag: {}", tag.name),
        summary: "This tag has no atoms and no child tags.".to_string(),
        reasons: vec![
            KnowledgeSignalReason {
                kind: "atom_count".to_string(),
                label: "0 atoms".to_string(),
                value: json!(0),
                contribution: 20.0,
            },
            KnowledgeSignalReason {
                kind: "children".to_string(),
                label: "no child tags".to_string(),
                value: json!(0),
                contribution: 15.0,
            },
        ],
        evidence: evidence.to_value()?,
        suggested_actions: vec![
            KnowledgeSignalAction {
                id: "delete_empty_tag".to_string(),
                label: "Delete tag".to_string(),
                kind: "delete".to_string(),
            },
            KnowledgeSignalAction {
                id: "keep".to_string(),
                label: "Keep".to_string(),
                kind: "dismiss".to_string(),
            },
        ],
        created_at: now.to_string(),
        expires_at: None,
    })
}

fn tag_pair_signal_key(a: &str, b: &str) -> String {
    let (left, right) = if a <= b { (a, b) } else { (b, a) };
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(format!("{left}:{right}").as_bytes());
    format!("tag_redundancy:pair:{:x}", digest)
}

fn is_structural_tag(tag: &TagCleanupTag) -> bool {
    tag.parent_id.is_none()
        && (tag.is_autotag_target
            || matches!(
                tag.name.as_str(),
                "Topics" | "People" | "Locations" | "Organizations" | "Events"
            ))
}

fn tags_are_hierarchically_related(
    a_id: &str,
    b_id: &str,
    tags: &HashMap<String, TagCleanupTag>,
) -> bool {
    a_id == b_id || tag_is_ancestor_of(a_id, b_id, tags) || tag_is_ancestor_of(b_id, a_id, tags)
}

fn tag_is_ancestor_of(
    ancestor_id: &str,
    child_id: &str,
    tags: &HashMap<String, TagCleanupTag>,
) -> bool {
    let mut current = tags.get(child_id).and_then(|tag| tag.parent_id.as_deref());
    while let Some(parent_id) = current {
        if parent_id == ancestor_id {
            return true;
        }
        current = tags.get(parent_id).and_then(|tag| tag.parent_id.as_deref());
    }
    false
}

fn hierarchy_relationship(a: &TagCleanupTag, b: &TagCleanupTag) -> String {
    if a.parent_id.is_some() && a.parent_id == b.parent_id {
        return "sibling".to_string();
    }
    if a.parent_id.as_deref() == Some(&b.id) || b.parent_id.as_deref() == Some(&a.id) {
        return "parent_child".to_string();
    }
    if a.path
        .first()
        .zip(b.path.first())
        .is_some_and(|(x, y)| x != y)
    {
        return "cross_category".to_string();
    }
    "unrelated".to_string()
}

fn name_similarity(a: &str, b: &str) -> f32 {
    let norm_a = normalize_tag_name(a);
    let norm_b = normalize_tag_name(b);
    if norm_a.is_empty() || norm_b.is_empty() {
        return 0.0;
    }
    if norm_a == norm_b {
        return 1.0;
    }
    let tokens_a: std::collections::HashSet<&str> = norm_a.split_whitespace().collect();
    let tokens_b: std::collections::HashSet<&str> = norm_b.split_whitespace().collect();
    let intersection = tokens_a.intersection(&tokens_b).count() as f32;
    let union = tokens_a.union(&tokens_b).count().max(1) as f32;
    let token_score = intersection / union;
    let containment = if norm_a.contains(&norm_b) || norm_b.contains(&norm_a) {
        0.75
    } else {
        0.0
    };
    token_score.max(containment)
}

fn normalize_tag_name(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn scaled_ln(value: i32, cap_at: f32) -> f32 {
    if value <= 0 {
        0.0
    } else {
        ((value as f32 + 1.0).ln() / cap_at.ln()).min(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CreateAtomRequest;
    use tempfile::NamedTempFile;
    use tokio::time::{sleep, Duration as TokioDuration};

    fn long_note(seed: &str) -> String {
        format!(
            "# {seed}\n\n{}",
            "This note contains enough substantive content for wiki candidate scoring. ".repeat(5)
        )
    }

    async fn test_core() -> (AtomicCore, NamedTempFile) {
        let temp = NamedTempFile::new().unwrap();
        let core = AtomicCore::open_or_create(temp.path()).unwrap();
        (core, temp)
    }

    async fn create_child_tag(core: &AtomicCore, name: &str) -> crate::Tag {
        let parent = core.create_tag("Research Areas", None).await.unwrap();
        core.create_tag(name, Some(&parent.id)).await.unwrap()
    }

    #[tokio::test]
    async fn wiki_candidate_signal_has_typed_evidence_and_reasons() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Distributed Systems").await;

        for i in 0..3 {
            core.create_atom(
                CreateAtomRequest {
                    content: long_note(&format!("Distributed Systems {i}")),
                    source_url: Some(format!("https://example.com/systems/{i}")),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("wiki candidate signal");

        assert_eq!(signal.id, format!("wiki_candidate:tag:{}", tag.id));
        assert_eq!(signal.provider_id, WIKI_CANDIDATE_PROVIDER_ID);
        assert!(signal.score > 0.0);
        assert!(signal.confidence > 0.0);
        assert!(signal
            .reasons
            .iter()
            .any(|reason| reason.kind == "atom_volume"));
        assert!(signal
            .reasons
            .iter()
            .any(|reason| reason.kind == "source_diversity"));
        assert_eq!(signal.evidence["schema"], "wiki_candidate");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(signal.evidence["tag_id"], tag.id);
        assert_eq!(signal.evidence["tag_name"], "Distributed Systems");
        assert_eq!(signal.evidence["atom_count"], 3);
        assert_eq!(signal.evidence["source_count"], 3);
    }

    #[tokio::test]
    async fn dismissed_wiki_candidate_is_hidden_until_included_or_restored() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Compiler Design").await;

        core.create_atom(
            CreateAtomRequest {
                content: long_note("Compiler Design"),
                source_url: Some("https://example.com/compiler-design".to_string()),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let key = format!("wiki_candidate:tag:{}", tag.id);
        dismiss_signal(&core, &key).await.unwrap();

        let visible = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!visible.iter().any(|signal| signal.id == key));

        let dismissed = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                include_dismissed: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(dismissed.iter().any(|signal| signal.id == key));

        restore_signal(&core, &key).await.unwrap();
        let restored = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(restored.iter().any(|signal| signal.id == key));
    }

    #[tokio::test]
    async fn dashboard_signal_listing_honors_provider_visibility_and_weight() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Database Internals").await;

        core.create_atom(
            CreateAtomRequest {
                content: long_note("Database Internals"),
                source_url: Some("https://example.com/database-internals".to_string()),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let baseline = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let baseline_score = baseline
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("baseline signal")
            .score;

        set_provider_config(
            &core,
            WIKI_CANDIDATE_PROVIDER_ID,
            KnowledgeSignalProviderConfig {
                provider_id: WIKI_CANDIDATE_PROVIDER_ID.to_string(),
                enabled: true,
                weight: 0.5,
                min_score: 0.0,
                min_confidence: 0.0,
                show_on_dashboard: true,
                include_in_briefing: true,
                config_json: json!({}),
            },
        )
        .await
        .unwrap();

        let weighted = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let weighted_score = weighted
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("weighted signal")
            .score;
        assert!((weighted_score - baseline_score * 0.5).abs() < 0.01);

        set_provider_config(
            &core,
            WIKI_CANDIDATE_PROVIDER_ID,
            KnowledgeSignalProviderConfig {
                provider_id: WIKI_CANDIDATE_PROVIDER_ID.to_string(),
                enabled: true,
                weight: 1.0,
                min_score: 0.0,
                min_confidence: 0.0,
                show_on_dashboard: false,
                include_in_briefing: true,
                config_json: json!({}),
            },
        )
        .await
        .unwrap();

        let hidden = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_CANDIDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(hidden.is_empty());
    }

    #[tokio::test]
    async fn wiki_update_signal_has_typed_evidence_and_reasons() {
        let (core, _temp) = test_core().await;
        let tag = create_child_tag(&core, "Knowledge Graphs").await;

        core.create_atom(
            CreateAtomRequest {
                content: long_note("Knowledge Graphs baseline"),
                source_url: Some("https://example.com/kg/baseline".to_string()),
                tag_ids: vec![tag.id.clone()],
                ..Default::default()
            },
            |_| {},
        )
        .await
        .unwrap()
        .unwrap();

        let storage = match core.storage() {
            crate::storage::StorageBackend::Sqlite(storage) => storage,
            #[cfg(feature = "postgres")]
            crate::storage::StorageBackend::Postgres(_) => panic!("test uses SQLite storage"),
        };
        storage
            .save_wiki_sync(&tag.id, "Existing wiki", &[], 1)
            .unwrap();

        sleep(TokioDuration::from_millis(5)).await;

        for i in 0..2 {
            core.create_atom(
                CreateAtomRequest {
                    content: long_note(&format!("Knowledge Graphs update {i}")),
                    source_url: Some(format!("https://example.com/kg/update/{i}")),
                    tag_ids: vec![tag.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(WIKI_UPDATE_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == tag.id)
            .expect("wiki update signal");

        assert_eq!(signal.id, format!("wiki_update:tag:{}", tag.id));
        assert_eq!(signal.provider_id, WIKI_UPDATE_PROVIDER_ID);
        assert_eq!(signal.evidence["schema"], "wiki_update");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(signal.evidence["tag_id"], tag.id);
        assert_eq!(signal.evidence["tag_name"], "Knowledge Graphs");
        assert_eq!(signal.evidence["article_atom_count"], 1);
        assert_eq!(signal.evidence["current_atom_count"], 3);
        assert_eq!(signal.evidence["new_atom_count"], 2);
        assert!(signal
            .reasons
            .iter()
            .any(|reason| reason.kind == "new_atom_volume"));
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "update_wiki"));
    }

    #[tokio::test]
    async fn tag_redundancy_signal_has_typed_evidence_and_merge_action() {
        let (core, _temp) = test_core().await;
        let parent = core.create_tag("Topics", None).await.unwrap();
        let tag_a = core
            .create_tag("AI Agents", Some(&parent.id))
            .await
            .unwrap();
        let tag_b = core
            .create_tag("Agentic AI", Some(&parent.id))
            .await
            .unwrap();

        for i in 0..6 {
            core.create_atom(
                CreateAtomRequest {
                    content: long_note(&format!("Agent systems {i}")),
                    source_url: Some(format!("https://example.com/agents/{i}")),
                    tag_ids: vec![tag_a.id.clone(), tag_b.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(TAG_REDUNDANCY_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| {
                signal.evidence["primary_tag"]["id"] == tag_a.id
                    || signal.evidence["secondary_tag"]["id"] == tag_a.id
            })
            .expect("tag redundancy signal");

        assert_eq!(signal.provider_id, TAG_REDUNDANCY_PROVIDER_ID);
        assert!(signal.id.starts_with("tag_redundancy:pair:"));
        assert_eq!(signal.evidence["schema"], "tag_redundancy");
        assert_eq!(signal.evidence["schema_version"], 1);
        assert_eq!(signal.evidence["shared_atom_count"], 6);
        assert_eq!(signal.evidence["jaccard_overlap"], 1.0);
        assert_eq!(signal.evidence["hierarchy_relationship"], "sibling");
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "merge_tags"));

        let result = core.merge_tags(&tag_b.id, &tag_a.id).await.unwrap();
        assert_eq!(result.atoms_retagged, 0);
        assert_eq!(result.children_reparented, 0);

        let remaining = core.get_all_tags().await.unwrap();
        let flat = flatten_test_tags(&remaining);
        assert!(flat.iter().any(|tag| tag.id == tag_a.id));
        assert!(!flat.iter().any(|tag| tag.id == tag_b.id));
    }

    #[tokio::test]
    async fn empty_tag_signal_is_dashboard_only_cleanup() {
        let (core, _temp) = test_core().await;
        let parent = core.create_tag("Projects", None).await.unwrap();
        let empty = core
            .create_tag("Abandoned Draft", Some(&parent.id))
            .await
            .unwrap();
        let structural = core.create_tag("People", None).await.unwrap();
        core.set_tag_autotag_target(&structural.id, true)
            .await
            .unwrap();

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(EMPTY_TAG_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        assert!(signals.iter().any(|signal| signal.target.id == empty.id));
        assert!(!signals.iter().any(|signal| signal.target.id == parent.id));
        assert!(!signals
            .iter()
            .any(|signal| signal.target.id == structural.id));

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == empty.id)
            .expect("empty tag signal");
        assert_eq!(signal.id, format!("empty_tag:tag:{}", empty.id));
        assert_eq!(signal.evidence["schema"], "empty_tag");
        assert_eq!(signal.evidence["tag"]["id"], empty.id);
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "delete_empty_tag"));
    }

    #[tokio::test]
    async fn missing_tag_overlap_signal_can_add_suggested_tag() {
        let (core, _temp) = test_core().await;
        let parent = core.create_tag("Topics", None).await.unwrap();
        let suggested = core
            .create_tag("Distributed Systems", Some(&parent.id))
            .await
            .unwrap();
        let existing = core
            .create_tag("Databases", Some(&parent.id))
            .await
            .unwrap();

        let target = core
            .create_atom(
                CreateAtomRequest {
                    content: long_note("Consensus overview"),
                    tag_ids: vec![existing.id.clone()],
                    ..Default::default()
                },
                |_| {},
            )
            .await
            .unwrap()
            .unwrap();

        let mut neighbors = Vec::new();
        for i in 0..3 {
            let atom = core
                .create_atom(
                    CreateAtomRequest {
                        content: long_note(&format!("Distributed systems neighbor {i}")),
                        tag_ids: vec![suggested.id.clone()],
                        ..Default::default()
                    },
                    |_| {},
                )
                .await
                .unwrap()
                .unwrap();
            neighbors.push(atom.atom.id);
        }

        let storage = match core.storage() {
            crate::storage::StorageBackend::Sqlite(storage) => storage,
            #[cfg(feature = "postgres")]
            crate::storage::StorageBackend::Postgres(_) => panic!("test uses SQLite storage"),
        };
        {
            let conn = storage.database().conn.lock().unwrap();
            let now = Utc::now().to_rfc3339();
            for (idx, neighbor_id) in neighbors.iter().enumerate() {
                conn.execute(
                    "INSERT INTO semantic_edges
                        (id, source_atom_id, target_atom_id, similarity_score,
                         source_chunk_index, target_chunk_index, created_at)
                     VALUES (?1, ?2, ?3, ?4, 0, 0, ?5)",
                    params![
                        format!("edge-{idx}"),
                        target.atom.id,
                        neighbor_id,
                        0.82_f32,
                        now
                    ],
                )
                .unwrap();
            }
        }

        let signals = list_knowledge_signals(
            &core,
            KnowledgeSignalFilter {
                provider_id: Some(MISSING_TAG_OVERLAP_PROVIDER_ID.to_string()),
                ..Default::default()
            },
        )
        .await
        .unwrap();

        let signal = signals
            .iter()
            .find(|signal| signal.target.id == target.atom.id)
            .expect("missing tag signal");
        assert_eq!(signal.provider_id, MISSING_TAG_OVERLAP_PROVIDER_ID);
        assert_eq!(signal.evidence["schema"], "missing_tag_overlap");
        assert_eq!(signal.evidence["atom_id"], target.atom.id);
        assert_eq!(signal.evidence["suggested_tag"]["id"], suggested.id);
        assert_eq!(signal.evidence["nearby_tagged_atom_count"], 3);
        assert!(signal
            .suggested_actions
            .iter()
            .any(|action| action.id == "add_tag_to_atom"));

        let updated = core
            .add_tag_to_atom(&target.atom.id, &suggested.id)
            .await
            .unwrap();
        assert!(updated.tags.iter().any(|tag| tag.id == suggested.id));
        assert!(updated.tags.iter().any(|tag| tag.id == existing.id));
    }

    fn flatten_test_tags(tags: &[crate::TagWithCount]) -> Vec<crate::Tag> {
        let mut out = Vec::new();
        for tag in tags {
            out.push(tag.tag.clone());
            out.extend(flatten_test_tags(&tag.children));
        }
        out
    }
}
