#![allow(clippy::pedantic, clippy::needless_raw_string_hashes)]
//! Durable SQLite storage for Airborne.
//!
//! The connection is serialized deliberately.  Airborne writes small subject
//! transactions and SQLite's WAL mode still permits readers in other processes.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use airborne_core::{
    Alert, AlertEventKind, AlertId, AlertKey, CandidateState, Observation, Rule, RuleConfig,
    RuleDefinition, RuleHistory, RuleHistorySet, RuleId, RuleKey, RuleKind, RuleVersion,
    SourceIdentity, Subject, SubjectKey, Timestamp, VersionedRule, Watch, WatchId, WatchState,
};
use airborne_runtime::{
    LeaseError, LeaseGuard, LeaseStore, RefreshLease, RefreshScope, RefreshTarget, RunnerLease,
    RuntimeStore, StoreError,
};
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use uuid::Uuid;

const LATEST_SCHEMA: i64 = 5;

#[derive(Debug, Error)]
pub enum SqliteStoreError {
    #[error("Airborne could not open its local database")]
    Open(#[source] rusqlite::Error),
    #[error("Airborne could not update its local database")]
    Storage(#[source] rusqlite::Error),
    #[error("the database integrity check failed: {0}")]
    Corrupt(String),
    #[error("the prototype database cannot be imported: {0}")]
    Import(String),
}

pub type Result<T> = std::result::Result<T, SqliteStoreError>;

/// A serialized SQLite connection.  It is safe to share across async callers:
/// every operation takes the mutex only while it executes synchronous SQL.
pub struct SqliteStore {
    path: Option<PathBuf>,
    connection: Arc<Mutex<Connection>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportReport {
    pub watches: u64,
    pub rules: u64,
    pub observations: u64,
    pub alerts: u64,
    pub settings: u64,
    pub skipped: u64,
    pub repaired: u64,
    pub already_imported: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct StatusView {
    pub watches: Vec<WatchStatus>,
}
#[derive(Clone, Debug, Serialize)]
pub struct WatchStatus {
    pub watch: Watch,
    pub subject: Subject,
    pub latest_poll_outcome: Option<String>,
    pub latest_issue: Option<StatusIssue>,
    pub rules: Vec<RuleStatus>,
}
#[derive(Clone, Debug, Serialize)]
pub struct RuleStatus {
    pub rule: Rule,
    pub latest_observation: Option<Observation>,
    pub latest_issue: Option<StatusIssue>,
}
#[derive(Clone, Debug, Serialize)]
pub struct StatusIssue {
    pub scope: String,
    pub provider: String,
    pub kind: String,
    pub retryable: bool,
    pub safe_message: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct LeaseStatus {
    pub name: String,
    pub active: bool,
    pub expires_at: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewWatch {
    pub subject: Subject,
    pub state: WatchState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewRule {
    pub watch_id: WatchId,
    pub config: RuleConfig,
    pub enabled: bool,
    pub created_at: Timestamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleChange {
    pub id: RuleId,
    pub config: RuleConfig,
    pub at: Timestamp,
}

/// Command repositories intentionally use domain records.  The CLI has no SQL
/// knowledge and can use these narrow methods directly.
#[async_trait]
pub trait CatalogRepository: Send + Sync {
    async fn add_watch(&self, draft: NewWatch) -> std::result::Result<Watch, StoreError>;
    async fn list_watches(
        &self,
        state: Option<WatchState>,
    ) -> std::result::Result<Vec<Watch>, StoreError>;
    async fn change_watch_state(
        &self,
        id: WatchId,
        state: WatchState,
        at: Timestamp,
    ) -> std::result::Result<Watch, StoreError>;
    async fn add_rule(&self, draft: NewRule) -> std::result::Result<Rule, StoreError>;
    async fn update_rule(&self, change: RuleChange) -> std::result::Result<Rule, StoreError>;
    async fn change_rule_state(
        &self,
        id: RuleId,
        enabled: bool,
        at: Timestamp,
    ) -> std::result::Result<Rule, StoreError>;
}

#[async_trait]
pub trait AlertRepository: Send + Sync {
    async fn list_alerts(
        &self,
        pending_only: bool,
        watch: Option<WatchId>,
    ) -> std::result::Result<Vec<Alert>, StoreError>;
    async fn acknowledge(
        &self,
        ids: &[AlertId],
        at: Timestamp,
    ) -> std::result::Result<usize, StoreError>;
}

impl SqliteStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let connection = Connection::open(&path).map_err(SqliteStoreError::Open)?;
        let store = Self {
            path: Some(path),
            connection: Arc::new(Mutex::new(connection)),
        };
        store.configure_and_migrate()?;
        Ok(store)
    }

    pub fn memory() -> Result<Self> {
        let connection = Connection::open_in_memory().map_err(SqliteStoreError::Open)?;
        let store = Self {
            path: None,
            connection: Arc::new(Mutex::new(connection)),
        };
        store.configure_and_migrate()?;
        Ok(store)
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn integrity_check(&self) -> Result<()> {
        let connection = self.connection.lock().expect("SQLite mutex poisoned");
        let result: String = connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(SqliteStoreError::Storage)?;
        if result == "ok" {
            Ok(())
        } else {
            Err(SqliteStoreError::Corrupt(result))
        }
    }

    pub fn schema_version(&self) -> Result<i64> {
        let connection = self.connection.lock().expect("SQLite mutex poisoned");
        connection
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migration",
                [],
                |row| row.get(0),
            )
            .map_err(SqliteStoreError::Storage)
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        self.connection
            .lock()
            .expect("SQLite mutex poisoned")
            .query_row("SELECT value FROM setting WHERE key=?", [key], |r| r.get(0))
            .optional()
            .map_err(SqliteStoreError::Storage)
    }
    pub fn lease_status(&self) -> Result<Vec<LeaseStatus>> {
        let c = self.connection.lock().expect("SQLite mutex poisoned");
        let now = now_epoch();
        let mut statement = c
            .prepare("SELECT name,expires_at FROM lease ORDER BY name")
            .map_err(SqliteStoreError::Storage)?;
        let rows = statement
            .query_map([], |r| {
                let name: String = r.get(0)?;
                let expires_at: i64 = r.get(1)?;
                Ok(LeaseStatus {
                    active: expires_at > now,
                    name,
                    expires_at,
                })
            })
            .map_err(SqliteStoreError::Storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(SqliteStoreError::Storage)?;
        Ok(rows)
    }
    pub fn set_setting(&self, key: &str, value: &str, at: &Timestamp) -> Result<()> {
        self.connection.lock().expect("SQLite mutex poisoned").execute("INSERT INTO setting(key,value,updated_at) VALUES (?,?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value,updated_at=excluded.updated_at",params![key,value,sql_timestamp(at)]).map_err(SqliteStoreError::Storage)?;
        Ok(())
    }
    pub fn archive_watch(&self, id: &WatchId, at: &Timestamp) -> Result<()> {
        let mut c = self.connection.lock().expect("SQLite mutex poisoned");
        let tx = c.transaction().map_err(SqliteStoreError::Storage)?;
        if tx.execute("UPDATE watch SET state='archived',updated_at=?,archived_at=? WHERE id=? AND state!='archived'",params![sql_timestamp(at),sql_timestamp(at),id.as_str()]).map_err(SqliteStoreError::Storage)?==0 { return Err(SqliteStoreError::Storage(rusqlite::Error::QueryReturnedNoRows)); }
        tx.execute("UPDATE rule SET state='archived',enabled=0,updated_at=?,archived_at=? WHERE watch_id=? AND state='active'",params![sql_timestamp(at),sql_timestamp(at),id.as_str()]).map_err(SqliteStoreError::Storage)?;
        tx.commit().map_err(SqliteStoreError::Storage)?;
        Ok(())
    }
    pub fn archive_rule(&self, id: &RuleId, at: &Timestamp) -> Result<()> {
        let changed=self.connection.lock().expect("SQLite mutex poisoned").execute("UPDATE rule SET state='archived',enabled=0,updated_at=?,archived_at=? WHERE id=? AND state='active'",params![sql_timestamp(at),sql_timestamp(at),id.as_str()]).map_err(SqliteStoreError::Storage)?;
        if changed == 0 {
            return Err(SqliteStoreError::Storage(
                rusqlite::Error::QueryReturnedNoRows,
            ));
        }
        Ok(())
    }
    pub fn get_watch(&self, id: &WatchId) -> Result<Option<Watch>> {
        self.connection.lock().expect("SQLite mutex poisoned").query_row("SELECT w.id,s.subject_key,w.state,w.created_at,w.updated_at,w.archived_at FROM watch w JOIN subject s ON s.id=w.subject_id WHERE w.id=?",[id.as_str()],read_watch).optional().map_err(SqliteStoreError::Storage)
    }
    pub fn get_rule(&self, id: &RuleId) -> Result<Option<Rule>> {
        self.connection.lock().expect("SQLite mutex poisoned").query_row("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE id=?",[id.as_str()],read_rule).optional().map_err(SqliteStoreError::Storage)
    }
    pub fn get_rule_definition(&self, id: &RuleId) -> Result<Option<RuleDefinition>> {
        let c = self.connection.lock().expect("SQLite mutex poisoned");
        c.query_row("SELECT version,definition,created_at FROM rule_definition WHERE rule_id=? ORDER BY version DESC LIMIT 1",[id.as_str()],|r| { let version:u64=r.get(0)?;let definition:String=r.get(1)?;let created:String=r.get(2)?; Ok(RuleDefinition {rule_id:id.clone(),version:RuleVersion::new(version).map_err(domain_sql)?,config:serde_json::from_str(&definition).map_err(|e|rusqlite::Error::FromSqlConversionFailure(1,rusqlite::types::Type::Text,Box::new(e)))?,created_at:text_timestamp(created).map_err(store_sql)?}) }).optional().map_err(SqliteStoreError::Storage)
    }
    pub fn list_rules(&self, watch: Option<&WatchId>) -> Result<Vec<Rule>> {
        let c = self.connection.lock().expect("SQLite mutex poisoned");
        let mut statement=c.prepare("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE (?1 IS NULL OR watch_id=?1) ORDER BY created_at,id").map_err(SqliteStoreError::Storage)?;
        let rows = statement
            .query_map([watch.map(|id| id.as_str())], read_rule)
            .map_err(SqliteStoreError::Storage)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(SqliteStoreError::Storage)?;
        Ok(rows)
    }
    pub fn get_alert(&self, id: &AlertId) -> Result<Option<Alert>> {
        let c = self.connection.lock().expect("SQLite mutex poisoned");
        let mut statement=c.prepare("SELECT a.id,a.rule_id,a.rule_version,a.revision,a.event_kind,a.source_identity,r.watch_id,s.subject_key,r.kind,a.title,a.body,a.source_url,a.created_at,a.acknowledged_at FROM alert a JOIN rule r ON r.id=a.rule_id JOIN watch w ON w.id=r.watch_id JOIN subject s ON s.id=w.subject_id WHERE a.id=?").map_err(SqliteStoreError::Storage)?;
        let mut rows = statement
            .query([id.as_str()])
            .map_err(SqliteStoreError::Storage)?;
        let Some(row) = rows.next().map_err(SqliteStoreError::Storage)? else {
            return Ok(None);
        };
        let id: String = row.get(0).map_err(SqliteStoreError::Storage)?;
        let rule: String = row.get(1).map_err(SqliteStoreError::Storage)?;
        let version: u64 = row.get(2).map_err(SqliteStoreError::Storage)?;
        let revision: String = row.get(3).map_err(SqliteStoreError::Storage)?;
        let event_kind: String = row.get(4).map_err(SqliteStoreError::Storage)?;
        let source: String = row.get(5).map_err(SqliteStoreError::Storage)?;
        let watch: String = row.get(6).map_err(SqliteStoreError::Storage)?;
        let subject: String = row.get(7).map_err(SqliteStoreError::Storage)?;
        let kind: String = row.get(8).map_err(SqliteStoreError::Storage)?;
        let title: String = row.get(9).map_err(SqliteStoreError::Storage)?;
        let body: String = row.get(10).map_err(SqliteStoreError::Storage)?;
        let source_url: Option<String> = row.get(11).map_err(SqliteStoreError::Storage)?;
        let created: String = row.get(12).map_err(SqliteStoreError::Storage)?;
        let acknowledged: Option<String> = row.get(13).map_err(SqliteStoreError::Storage)?;
        Ok(Some(Alert {
            id: AlertId::new(id).map_err(|e| SqliteStoreError::Import(e.to_string()))?,
            key: AlertKey {
                rule_id: RuleId::new(rule).map_err(|e| SqliteStoreError::Import(e.to_string()))?,
                rule_version: RuleVersion::new(version)
                    .map_err(|e| SqliteStoreError::Import(e.to_string()))?,
                revision: airborne_core::Revision::new(revision)
                    .map_err(|e| SqliteStoreError::Import(e.to_string()))?,
                event_kind: parse_alert_event_kind(event_kind)
                    .map_err(|e| SqliteStoreError::Import(e.to_string()))?,
                source_identity: SourceIdentity::new(source)
                    .map_err(|e| SqliteStoreError::Import(e.to_string()))?,
            },
            watch_id: WatchId::new(watch).map_err(|e| SqliteStoreError::Import(e.to_string()))?,
            subject_key: SubjectKey::new(subject)
                .map_err(|e| SqliteStoreError::Import(e.to_string()))?,
            rule_kind: parse_rule_kind(kind)
                .map_err(|e| SqliteStoreError::Import(e.to_string()))?,
            title,
            body,
            source_url,
            created_at: text_timestamp(created)
                .map_err(|e| SqliteStoreError::Import(e.to_string()))?,
            acknowledged_at: acknowledged
                .map(text_timestamp)
                .transpose()
                .map_err(|e| SqliteStoreError::Import(e.to_string()))?,
        }))
    }
    pub fn status(&self, watch_id: Option<&WatchId>) -> Result<StatusView> {
        let c = self.connection.lock().expect("SQLite mutex poisoned");
        let mut watches=c.prepare("SELECT w.id,s.subject_key,w.state,w.created_at,w.updated_at,w.archived_at,s.kind,s.canonical_url,s.title,s.revision,s.metadata_refreshed_at,s.created_at,s.id FROM watch w JOIN subject s ON s.id=w.subject_id WHERE (?1 IS NULL OR w.id=?1) ORDER BY w.created_at,w.id").map_err(SqliteStoreError::Storage)?;
        let rows = watches
            .query_map([watch_id.map(WatchId::as_str)], |r| {
                let watch = read_watch(r)?;
                let kind: String = r.get(6)?;
                if kind != "github_pull_request" {
                    return Err(store_sql(store_error(
                        "database has an unknown subject kind",
                    )));
                }
                let key: String = r.get(7 - 6)?;
                let url: String = r.get(7)?;
                let title: String = r.get(8)?;
                let revision: Option<String> = r.get(9)?;
                let refreshed: Option<String> = r.get(10)?;
                let created: String = r.get(11)?;
                let subject_id: i64 = r.get(12)?;
                Ok((
                    watch,
                    Subject {
                        key: SubjectKey::new(key).map_err(domain_sql)?,
                        kind: airborne_core::SubjectKind::GitHubPullRequest,
                        canonical_url: url,
                        display_title: title,
                        current_revision: revision
                            .map(airborne_core::Revision::new)
                            .transpose()
                            .map_err(domain_sql)?,
                        metadata_refreshed_at: refreshed
                            .map(text_timestamp)
                            .transpose()
                            .map_err(store_sql)?,
                        created_at: text_timestamp(created).map_err(store_sql)?,
                    },
                    subject_id,
                ))
            })
            .map_err(SqliteStoreError::Storage)?;
        let mut result = Vec::new();
        for row in rows {
            let (watch, subject, subject_id) = row.map_err(SqliteStoreError::Storage)?;
            let outcome=c.query_row("SELECT outcome FROM poll_attempt WHERE subject_id=? ORDER BY finished_at DESC,id DESC LIMIT 1",[subject_id],|r|r.get(0)).optional().map_err(SqliteStoreError::Storage)?;
            let mut rules=c.prepare("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE watch_id=? AND enabled=1 AND state='active' ORDER BY id").map_err(SqliteStoreError::Storage)?;
            let rows = rules
                .query_map([watch.id.as_str()], read_rule)
                .map_err(SqliteStoreError::Storage)?;
            let mut statuses = Vec::new();
            for rule in rows {
                let rule = rule.map_err(SqliteStoreError::Storage)?;
                let observation=c.query_row("SELECT rule_id,rule_version,revision,state,source_identity,source_url,detail,observed_at,first_observed_at,first_source_seen_at FROM observation WHERE rule_id=? AND rule_version=? ORDER BY observed_at DESC,id DESC LIMIT 1",params![rule.id.as_str(),rule.current_version.get()],|r| {let id:String=r.get(0)?;let version:u64=r.get(1)?;let revision:String=r.get(2)?;let state:String=r.get(3)?;let source:Option<String>=r.get(4)?;let url:Option<String>=r.get(5)?;let detail:Option<String>=r.get(6)?;let at:String=r.get(7)?;let first:String=r.get(8)?;let first_source:Option<String>=r.get(9)?;Ok(Observation{rule_id:RuleId::new(id).map_err(domain_sql)?,rule_version:RuleVersion::new(version).map_err(domain_sql)?,revision:airborne_core::Revision::new(revision).map_err(domain_sql)?,state:parse_state(state).map_err(store_sql)?,source_identity:source.map(SourceIdentity::new).transpose().map_err(domain_sql)?,source_url:url,detail,observed_at:text_timestamp(at).map_err(store_sql)?,first_observed_at:text_timestamp(first).map_err(store_sql)?,first_source_seen_at:first_source.map(text_timestamp).transpose().map_err(store_sql)?})}).optional().map_err(SqliteStoreError::Storage)?;
                let issue=c.query_row("SELECT si.scope,si.provider,si.kind,si.retryable,si.message FROM source_issue si JOIN poll_attempt pa ON pa.id=si.poll_attempt_id WHERE si.rule_id=? AND pa.watch_id=? AND pa.id=(SELECT id FROM poll_attempt WHERE watch_id=? ORDER BY finished_at DESC,id DESC LIMIT 1) ORDER BY si.id DESC LIMIT 1",params![rule.id.as_str(),watch.id.as_str(),watch.id.as_str()],|r|Ok(StatusIssue{scope:r.get(0)?,provider:r.get(1)?,kind:r.get(2)?,retryable:r.get(3)?,safe_message:r.get(4)?})).optional().map_err(SqliteStoreError::Storage)?;
                statuses.push(RuleStatus {
                    rule,
                    latest_observation: observation,
                    latest_issue: issue,
                });
            }
            let latest_issue=c.query_row("SELECT si.scope,si.provider,si.kind,si.retryable,si.message FROM source_issue si JOIN poll_attempt pa ON pa.id=si.poll_attempt_id WHERE si.rule_id IS NULL AND pa.watch_id=? AND pa.id=(SELECT id FROM poll_attempt WHERE watch_id=? ORDER BY finished_at DESC,id DESC LIMIT 1) ORDER BY si.id DESC LIMIT 1",params![watch.id.as_str(),watch.id.as_str()],|r|Ok(StatusIssue{scope:r.get(0)?,provider:r.get(1)?,kind:r.get(2)?,retryable:r.get(3)?,safe_message:r.get(4)?})).optional().map_err(SqliteStoreError::Storage)?;
            result.push(WatchStatus {
                watch,
                subject,
                latest_poll_outcome: outcome,
                latest_issue,
                rules: statuses,
            });
        }
        Ok(StatusView { watches: result })
    }
    /// Imports only the supported, non-secret prototype records.  It opens the
    /// old database read-only and records its content fingerprint after a fully
    /// committed import, so a second call is a no-op.
    pub fn import_prototype(
        &self,
        source: impl AsRef<Path>,
        dry_run: bool,
    ) -> Result<ImportReport> {
        let source = source.as_ref();
        let bytes = std::fs::read(source).map_err(|e| SqliteStoreError::Import(e.to_string()))?;
        let fingerprint = format!("{:x}", Sha256::digest(bytes));
        let old = Connection::open_with_flags(source, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| SqliteStoreError::Import(e.to_string()))?;
        let mut report = ImportReport {
            watches: 0,
            rules: 0,
            observations: 0,
            alerts: 0,
            settings: 0,
            skipped: 0,
            repaired: 0,
            already_imported: false,
        };
        if self
            .connection
            .lock()
            .expect("SQLite mutex poisoned")
            .query_row(
                "SELECT 1 FROM import_record WHERE source_fingerprint=?",
                [&fingerprint],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .map_err(SqliteStoreError::Storage)?
            .is_some()
        {
            report.already_imported = true;
            return Ok(report);
        }
        let mut watches=old.prepare("SELECT id,github_owner,github_repo,github_pr_number,title,active,created_at,head_sha FROM watch").map_err(|e|SqliteStoreError::Import(e.to_string()))?;
        let rows = watches
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, bool>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, Option<String>>(7)?,
                ))
            })
            .map_err(|e| SqliteStoreError::Import(e.to_string()))?;
        let mut c = self.connection.lock().expect("SQLite mutex poisoned");
        let tx = c.transaction().map_err(SqliteStoreError::Storage)?;
        let mut imported_watches = BTreeMap::new();
        for row in rows {
            let (old_watch, owner, repo, number, title, active, created, revision) =
                row.map_err(|e| SqliteStoreError::Import(e.to_string()))?;
            if number < 1 {
                report.skipped += 1;
                continue;
            };
            let key = format!("github.com/{owner}/{repo}/pull/{number}");
            let url = format!("https://github.com/{owner}/{repo}/pull/{number}");
            tx.execute("INSERT INTO subject(subject_key,kind,canonical_url,title,revision,created_at,updated_at) VALUES (?,'github_pull_request',?,?,?,?,?) ON CONFLICT(subject_key) DO NOTHING",params![key,url,title,revision,created,created]).map_err(SqliteStoreError::Storage)?;
            let subject: i64 = tx
                .query_row("SELECT id FROM subject WHERE subject_key=?", [&key], |r| {
                    r.get(0)
                })
                .map_err(SqliteStoreError::Storage)?;
            let id = uid("watch");
            if tx.execute("INSERT OR IGNORE INTO watch(id,subject_id,state,created_at,updated_at) VALUES (?,? ,?, ?, ?)",params![id,subject,if active{"active"}else{"paused"},created,created]).map_err(SqliteStoreError::Storage)?==1 {report.watches+=1; imported_watches.insert(old_watch,id);}
        }
        let mut rules = old
            .prepare("SELECT id,watch_id,kind,config_json,enabled,created_at FROM rule ORDER BY created_at,id")
            .map_err(|e| SqliteStoreError::Import(e.to_string()))?;
        let old_rules = rules
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, bool>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })
            .map_err(|e| SqliteStoreError::Import(e.to_string()))?;
        let mut imported_rules = BTreeMap::new();
        let mut imported_live_rules = BTreeMap::new();
        for row in old_rules {
            let (old_rule, old_watch, kind, json, enabled, created) =
                row.map_err(|e| SqliteStoreError::Import(e.to_string()))?;
            let Some(watch) = imported_watches.get(&old_watch) else {
                report.skipped += 1;
                continue;
            };
            let value: serde_json::Value = match serde_json::from_str(&json) {
                Ok(value) => value,
                Err(_) => {
                    report.skipped += 1;
                    continue;
                }
            };
            let config = match kind.as_str() {
                "cursor_bugbot_completed" => {
                    serde_json::json!({"kind":"git_hub_check_completes","check_name":value.get("check_name").and_then(|x|x.as_str()).unwrap_or("Cursor Bugbot"),"alert_on_start":value.get("alert_on_start").and_then(|x|x.as_bool()).unwrap_or(false),"alert_if_missing_after_seconds":value.get("alert_if_missing_after_seconds").and_then(|x|x.as_u64())})
                }
                "buildkite_job_completed" => {
                    serde_json::json!({"kind":"buildkite_job_completes","github_status_context":value.get("github_status_context").and_then(|x|x.as_str()).unwrap_or(""),"expected_organization":value.get("organization").and_then(|x|x.as_str()).unwrap_or(""),"expected_pipeline":value.get("pipeline").and_then(|x|x.as_str()).unwrap_or(""),"job_name":value.get("job_name").and_then(|x|x.as_str()).unwrap_or(""),"notify_on":value.get("notify_on").and_then(|x|x.as_str()).unwrap_or("terminal")})
                }
                _ => {
                    report.skipped += 1;
                    continue;
                }
            };
            if serde_json::from_value::<RuleConfig>(config.clone()).is_err() {
                report.skipped += 1;
                continue;
            };
            let config: RuleConfig = match serde_json::from_value::<RuleConfig>(config.clone()) {
                Ok(config) if config.validate().is_ok() => config,
                _ => {
                    report.skipped += 1;
                    continue;
                }
            };
            let id = uid("rule");
            let kind = rule_kind_text(config.kind());
            let duplicate_of = imported_live_rules
                .get(&(watch.clone(), kind.to_owned()))
                .cloned();
            let state = if duplicate_of.is_some() {
                "archived"
            } else {
                "active"
            };
            tx.execute("INSERT INTO rule(id,watch_id,kind,enabled,current_version,state,created_at,updated_at,archived_at) VALUES (?,?,?,?,1,?,?,?,?)",params![id,watch,kind,if duplicate_of.is_some(){false}else{enabled},state,created,created,if duplicate_of.is_some(){Some(created.clone())}else{None}]).map_err(SqliteStoreError::Storage)?;
            tx.execute("INSERT INTO rule_definition(rule_id,version,encoding_version,definition,created_at) VALUES (?,1,1,?,?)",params![id,serde_json::to_string(&config).map_err(|e|SqliteStoreError::Import(e.to_string()))?,created]).map_err(SqliteStoreError::Storage)?;
            if let Some(surviving_rule_id) = duplicate_of {
                tx.execute("INSERT INTO migration_repair(migration_version,surviving_rule_id,archived_rule_id,reason,repaired_at) VALUES (5,?,?,?,strftime('%Y-%m-%dT%H:%M:%fZ','now'))", params![surviving_rule_id, id, "prototype duplicate rule kind; kept earliest created_at then id"]).map_err(SqliteStoreError::Storage)?;
                tx.execute("UPDATE rule SET updated_at=(SELECT repaired_at FROM migration_repair WHERE archived_rule_id=?),archived_at=(SELECT repaired_at FROM migration_repair WHERE archived_rule_id=?) WHERE id=?", params![id, id, id]).map_err(SqliteStoreError::Storage)?;
                report.repaired += 1;
            } else {
                imported_live_rules.insert((watch.clone(), kind.to_owned()), id.clone());
            }
            imported_rules.insert(old_rule, id);
            report.rules += 1;
        }
        let mut observations = old.prepare("SELECT rule_id,head_sha,state,source_identity,source_url,detail,observed_at FROM rule_observation").map_err(|e|SqliteStoreError::Import(e.to_string()))?;
        let old_observations = observations
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| SqliteStoreError::Import(e.to_string()))?;
        for row in old_observations {
            let (old_rule, revision, state, source, url, detail, at) =
                row.map_err(|e| SqliteStoreError::Import(e.to_string()))?;
            let Some(rule) = imported_rules.get(&old_rule) else {
                report.skipped += 1;
                continue;
            };
            if !matches!(
                state.as_str(),
                "waiting" | "in_progress" | "completed" | "failed" | "unavailable"
            ) {
                report.skipped += 1;
                continue;
            };
            if tx.execute("INSERT OR IGNORE INTO observation(rule_id,rule_version,revision,state,source_identity,source_url,detail,observed_at,first_observed_at,first_source_seen_at) VALUES (?,1,?,?,?,?,?,?,?,?)",params![rule,revision,state,source,url,detail,at,at,if source.is_some() { Some(at.clone()) } else { None }]).map_err(SqliteStoreError::Storage)?==1{report.observations+=1;}
        }
        let mut alerts = old
            .prepare(
                "SELECT rule_id,head_sha,source_identity,title,body,status,created_at FROM alert",
            )
            .map_err(|e| SqliteStoreError::Import(e.to_string()))?;
        let old_alerts = alerts
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            })
            .map_err(|e| SqliteStoreError::Import(e.to_string()))?;
        for row in old_alerts {
            let (old_rule, revision, source, title, body, status, created) =
                row.map_err(|e| SqliteStoreError::Import(e.to_string()))?;
            let Some(rule) = imported_rules.get(&old_rule) else {
                report.skipped += 1;
                continue;
            };
            let (new_status, ack) = match status.as_str() {
                "read" => ("acknowledged", Some(created.clone())),
                "unread" => ("pending", None),
                _ => {
                    report.repaired += 1;
                    ("pending", None)
                }
            };
            let (watch, subject, rule_kind):(String,String,String)=tx.query_row("SELECT r.watch_id,s.subject_key,r.kind FROM rule r JOIN watch w ON w.id=r.watch_id JOIN subject s ON s.id=w.subject_id WHERE r.id=?",[rule],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(SqliteStoreError::Storage)?;
            if tx.execute("INSERT OR IGNORE INTO alert(id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,event_kind,source_identity,title,body,status,created_at,acknowledged_at) VALUES (?, ?, 1, ?, ?, ?, ?, 'terminal', ?, ?, ?, ?, ?, ?)",params![uid("alert"),rule,watch,subject,rule_kind,revision,source,title,body,new_status,created,ack]).map_err(SqliteStoreError::Storage)?==1{report.alerts+=1;}
        }
        if let Some(settings) = old
            .query_row("SELECT value FROM setting WHERE key='settings'", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .map_err(|e| SqliteStoreError::Import(e.to_string()))?
        {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&settings) {
                if let Some(interval) = value.get("poll_interval_seconds").and_then(|v| v.as_u64())
                {
                    tx.execute("INSERT INTO setting(key,value,updated_at) VALUES ('poll-interval',?,?) ON CONFLICT(key) DO NOTHING",params![interval.to_string(),"1970-01-01T00:00:00Z"]).map_err(SqliteStoreError::Storage)?;
                    report.settings += 1;
                }
            }
        }
        if !dry_run {
            tx.execute(
                "INSERT INTO import_record(source_fingerprint,imported_at,source_path) VALUES (?,strftime('%Y-%m-%dT%H:%M:%fZ','now'),?)",
                params![fingerprint, source.display().to_string()],
            )
            .map_err(SqliteStoreError::Storage)?;
            tx.commit().map_err(SqliteStoreError::Storage)?;
        }
        Ok(report)
    }

    fn configure_and_migrate(&self) -> Result<()> {
        let mut connection = self.connection.lock().expect("SQLite mutex poisoned");
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(SqliteStoreError::Storage)?;
        connection
            .execute_batch(
                "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;",
            )
            .map_err(SqliteStoreError::Storage)?;
        migrate(&mut connection)
    }
}

fn store_error(error: impl std::fmt::Display) -> StoreError {
    StoreError::Failed {
        message: error.to_string(),
    }
}
fn lease_error(error: impl std::fmt::Display) -> LeaseError {
    LeaseError::Failed {
        message: error.to_string(),
    }
}
fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}
fn state_text(value: CandidateState) -> &'static str {
    match value {
        CandidateState::NotDetected => "not_detected",
        CandidateState::Waiting => "waiting",
        CandidateState::InProgress => "in_progress",
        CandidateState::Completed => "completed",
        CandidateState::Failed => "failed",
        CandidateState::Unavailable => "unavailable",
    }
}
fn parse_state(value: String) -> std::result::Result<CandidateState, StoreError> {
    match value.as_str() {
        "not_detected" => Ok(CandidateState::NotDetected),
        "waiting" => Ok(CandidateState::Waiting),
        "in_progress" => Ok(CandidateState::InProgress),
        "completed" => Ok(CandidateState::Completed),
        "failed" => Ok(CandidateState::Failed),
        "unavailable" => Ok(CandidateState::Unavailable),
        _ => Err(store_error("database has an unknown candidate state")),
    }
}
fn watch_state_text(value: WatchState) -> &'static str {
    match value {
        WatchState::Active => "active",
        WatchState::Paused => "paused",
        WatchState::Archived => "archived",
    }
}
fn parse_watch_state(value: String) -> std::result::Result<WatchState, StoreError> {
    match value.as_str() {
        "active" => Ok(WatchState::Active),
        "paused" => Ok(WatchState::Paused),
        "archived" => Ok(WatchState::Archived),
        _ => Err(store_error("database has an unknown watch state")),
    }
}
fn rule_kind_text(value: RuleKind) -> &'static str {
    match value {
        RuleKind::GitHubCheckCompletes => "github_check_completes",
        RuleKind::BuildkiteJobCompletes => "buildkite_job_completes",
    }
}
fn parse_rule_kind(value: String) -> std::result::Result<RuleKind, StoreError> {
    match value.as_str() {
        "github_check_completes" => Ok(RuleKind::GitHubCheckCompletes),
        "buildkite_job_completes" => Ok(RuleKind::BuildkiteJobCompletes),
        _ => Err(store_error("database has an unknown rule kind")),
    }
}
fn alert_event_kind_text(value: AlertEventKind) -> &'static str {
    match value {
        AlertEventKind::Missing => "missing",
        AlertEventKind::Started => "started",
        AlertEventKind::Terminal => "terminal",
    }
}
fn parse_alert_event_kind(value: String) -> std::result::Result<AlertEventKind, StoreError> {
    match value.as_str() {
        "missing" => Ok(AlertEventKind::Missing),
        "started" => Ok(AlertEventKind::Started),
        "terminal" => Ok(AlertEventKind::Terminal),
        _ => Err(store_error("database has an unknown alert event kind")),
    }
}
fn text_timestamp(value: String) -> std::result::Result<Timestamp, StoreError> {
    Timestamp::parse(&value).map_err(store_error)
}
fn sql_timestamp(value: &Timestamp) -> String {
    value.to_string()
}
fn uid(prefix: &str) -> String {
    format!("{prefix}-{}", Uuid::new_v4())
}
fn existing_live_rule(
    tx: &Transaction<'_>,
    watch_id: &WatchId,
    kind: RuleKind,
) -> std::result::Result<Option<RuleId>, StoreError> {
    tx.query_row(
        "SELECT id FROM rule WHERE watch_id=? AND kind=? AND state != 'archived' ORDER BY created_at,id LIMIT 1",
        params![watch_id.as_str(), rule_kind_text(kind)],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(store_error)?
    .map(RuleId::new)
    .transpose()
    .map_err(store_error)
}

fn read_watch(row: &rusqlite::Row<'_>) -> rusqlite::Result<Watch> {
    let id: String = row.get(0)?;
    let key: String = row.get(1)?;
    let state: String = row.get(2)?;
    let created: String = row.get(3)?;
    let updated: String = row.get(4)?;
    let archived: Option<String> = row.get(5)?;
    Ok(Watch {
        id: WatchId::new(id).map_err(domain_sql)?,
        subject_key: SubjectKey::new(key).map_err(domain_sql)?,
        state: parse_watch_state(state).map_err(store_sql)?,
        created_at: text_timestamp(created).map_err(store_sql)?,
        updated_at: text_timestamp(updated).map_err(store_sql)?,
        archived_at: archived
            .map(text_timestamp)
            .transpose()
            .map_err(store_sql)?,
    })
}
fn read_watch_at(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<Watch> {
    let id: String = row.get(offset)?;
    let key: String = row.get(offset + 1)?;
    let state: String = row.get(offset + 2)?;
    let created: String = row.get(offset + 3)?;
    let updated: String = row.get(offset + 4)?;
    let archived: Option<String> = row.get(offset + 5)?;
    Ok(Watch {
        id: WatchId::new(id).map_err(domain_sql)?,
        subject_key: SubjectKey::new(key).map_err(domain_sql)?,
        state: parse_watch_state(state).map_err(store_sql)?,
        created_at: text_timestamp(created).map_err(store_sql)?,
        updated_at: text_timestamp(updated).map_err(store_sql)?,
        archived_at: archived
            .map(text_timestamp)
            .transpose()
            .map_err(store_sql)?,
    })
}
fn read_subject(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, Subject)> {
    let id = row.get(0)?;
    let key: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let url: String = row.get(3)?;
    let title: String = row.get(4)?;
    let revision: Option<String> = row.get(5)?;
    let refreshed: Option<String> = row.get(6)?;
    let created: String = row.get(7)?;
    if kind != "github_pull_request" {
        return Err(store_sql(store_error(
            "database has an unknown subject kind",
        )));
    }
    Ok((
        id,
        Subject {
            key: SubjectKey::new(key).map_err(domain_sql)?,
            kind: airborne_core::SubjectKind::GitHubPullRequest,
            canonical_url: url,
            display_title: title,
            current_revision: revision
                .map(airborne_core::Revision::new)
                .transpose()
                .map_err(domain_sql)?,
            metadata_refreshed_at: refreshed
                .map(text_timestamp)
                .transpose()
                .map_err(store_sql)?,
            created_at: text_timestamp(created).map_err(store_sql)?,
        },
    ))
}
fn read_rule(row: &rusqlite::Row<'_>) -> rusqlite::Result<Rule> {
    let id: String = row.get(0)?;
    let watch: String = row.get(1)?;
    let kind: String = row.get(2)?;
    let enabled: bool = row.get(3)?;
    let version: u64 = row.get(4)?;
    let created: String = row.get(5)?;
    let updated: String = row.get(6)?;
    let archived: Option<String> = row.get(7)?;
    Ok(Rule {
        id: RuleId::new(id).map_err(domain_sql)?,
        watch_id: WatchId::new(watch).map_err(domain_sql)?,
        kind: parse_rule_kind(kind).map_err(store_sql)?,
        enabled,
        current_version: RuleVersion::new(version).map_err(domain_sql)?,
        created_at: text_timestamp(created).map_err(store_sql)?,
        updated_at: text_timestamp(updated).map_err(store_sql)?,
        archived_at: archived
            .map(text_timestamp)
            .transpose()
            .map_err(store_sql)?,
    })
}
fn domain_sql(error: airborne_core::DomainError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}
fn store_sql(error: StoreError) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn migrate(connection: &mut Connection) -> Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS schema_migration (version INTEGER PRIMARY KEY CHECK(version > 0), applied_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')));")
        .map_err(SqliteStoreError::Storage)?;
    let current: i64 = connection
        .query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migration",
            [],
            |row| row.get(0),
        )
        .map_err(SqliteStoreError::Storage)?;
    if current > LATEST_SCHEMA {
        return Err(SqliteStoreError::Corrupt(
            "database was created by a newer Airborne".into(),
        ));
    }
    if current == 0 {
        let tx = connection
            .transaction()
            .map_err(SqliteStoreError::Storage)?;
        create_v1(&tx)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (1)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (2)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (3)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (4)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (5)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.commit().map_err(SqliteStoreError::Storage)?;
    } else if current == 1 {
        let tx = connection
            .transaction()
            .map_err(SqliteStoreError::Storage)?;
        tx.execute("ALTER TABLE alert ADD COLUMN source_url TEXT", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (2)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.execute_batch("ALTER TABLE poll_attempt ADD COLUMN refresh_id TEXT; ALTER TABLE poll_attempt ADD COLUMN watch_id TEXT; ALTER TABLE poll_attempt ADD COLUMN revision TEXT; ALTER TABLE source_issue ADD COLUMN scope TEXT; ALTER TABLE source_issue ADD COLUMN provider TEXT; ALTER TABLE source_issue ADD COLUMN retryable INTEGER;").map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (3)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.execute_batch("ALTER TABLE alert ADD COLUMN watch_id TEXT; ALTER TABLE alert ADD COLUMN subject_key TEXT; ALTER TABLE alert ADD COLUMN rule_kind TEXT;").map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (4)", [])
            .map_err(SqliteStoreError::Storage)?;
        migrate_v5(&tx)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (5)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.commit().map_err(SqliteStoreError::Storage)?;
    }
    if current == 2 {
        let tx = connection
            .transaction()
            .map_err(SqliteStoreError::Storage)?;
        tx.execute_batch("ALTER TABLE poll_attempt ADD COLUMN refresh_id TEXT; ALTER TABLE poll_attempt ADD COLUMN watch_id TEXT; ALTER TABLE poll_attempt ADD COLUMN revision TEXT; ALTER TABLE source_issue ADD COLUMN scope TEXT; ALTER TABLE source_issue ADD COLUMN provider TEXT; ALTER TABLE source_issue ADD COLUMN retryable INTEGER;").map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (3)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.execute_batch("ALTER TABLE alert ADD COLUMN watch_id TEXT; ALTER TABLE alert ADD COLUMN subject_key TEXT; ALTER TABLE alert ADD COLUMN rule_kind TEXT;").map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (4)", [])
            .map_err(SqliteStoreError::Storage)?;
        migrate_v5(&tx)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (5)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.commit().map_err(SqliteStoreError::Storage)?;
    }
    if current == 3 {
        let tx = connection
            .transaction()
            .map_err(SqliteStoreError::Storage)?;
        tx.execute_batch("ALTER TABLE alert ADD COLUMN watch_id TEXT; ALTER TABLE alert ADD COLUMN subject_key TEXT; ALTER TABLE alert ADD COLUMN rule_kind TEXT;").map_err(SqliteStoreError::Storage)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (4)", [])
            .map_err(SqliteStoreError::Storage)?;
        migrate_v5(&tx)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (5)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.commit().map_err(SqliteStoreError::Storage)?;
    }
    if current == 4 {
        let tx = connection
            .transaction()
            .map_err(SqliteStoreError::Storage)?;
        migrate_v5(&tx)?;
        tx.execute("INSERT INTO schema_migration(version) VALUES (5)", [])
            .map_err(SqliteStoreError::Storage)?;
        tx.commit().map_err(SqliteStoreError::Storage)?;
    }
    Ok(())
}

fn migrate_v5(tx: &Transaction<'_>) -> Result<()> {
    let has_observation: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='observation')",
            [],
            |row| row.get(0),
        )
        .map_err(SqliteStoreError::Storage)?;
    let has_rule: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='rule')",
            [],
            |row| row.get(0),
        )
        .map_err(SqliteStoreError::Storage)?;
    let has_rule_definition: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='rule_definition')", [], |row| row.get(0)).map_err(SqliteStoreError::Storage)?;
    if has_observation {
        tx.execute_batch("ALTER TABLE observation ADD COLUMN first_observed_at TEXT; ALTER TABLE observation ADD COLUMN first_source_seen_at TEXT; UPDATE observation SET first_observed_at=observed_at WHERE first_observed_at IS NULL; UPDATE observation SET first_source_seen_at=observed_at WHERE source_identity IS NOT NULL AND first_source_seen_at IS NULL;").map_err(SqliteStoreError::Storage)?;
    }
    if !has_rule || !has_rule_definition {
        return Ok(());
    }
    tx.execute_batch(
        "CREATE TABLE migration_repair (id INTEGER PRIMARY KEY, migration_version INTEGER NOT NULL, surviving_rule_id TEXT NOT NULL, archived_rule_id TEXT NOT NULL UNIQUE, reason TEXT NOT NULL, repaired_at TEXT NOT NULL); \
         CREATE TABLE alert_v5 (id TEXT PRIMARY KEY, rule_id TEXT NOT NULL REFERENCES rule(id), rule_version INTEGER NOT NULL, watch_id TEXT NOT NULL REFERENCES watch(id), subject_key TEXT NOT NULL, rule_kind TEXT NOT NULL, revision TEXT NOT NULL, event_kind TEXT NOT NULL CHECK(event_kind IN ('missing','started','terminal')), source_identity TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('pending','acknowledged')), source_url TEXT, created_at TEXT NOT NULL, acknowledged_at TEXT, UNIQUE(rule_id, rule_version, revision, event_kind, source_identity), FOREIGN KEY(rule_id, rule_version) REFERENCES rule_definition(rule_id, version)); \
         UPDATE alert SET watch_id=(SELECT watch_id FROM rule WHERE rule.id=alert.rule_id) WHERE watch_id IS NULL; \
         UPDATE alert SET rule_kind=(SELECT kind FROM rule WHERE rule.id=alert.rule_id) WHERE rule_kind IS NULL; \
         UPDATE alert SET subject_key=(SELECT subject.subject_key FROM rule JOIN watch ON watch.id=rule.watch_id JOIN subject ON subject.id=watch.subject_id WHERE rule.id=alert.rule_id) WHERE subject_key IS NULL; \
         INSERT INTO alert_v5(id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,event_kind,source_identity,title,body,status,source_url,created_at,acknowledged_at) SELECT id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,'terminal',source_identity,title,body,status,source_url,created_at,acknowledged_at FROM alert; \
         DROP TABLE alert; ALTER TABLE alert_v5 RENAME TO alert; \
         CREATE INDEX alert_created ON alert(created_at DESC);",
    ).map_err(SqliteStoreError::Storage)?;
    let mut statement = tx.prepare("SELECT watch_id,kind,id FROM rule WHERE state != 'archived' ORDER BY watch_id,kind,created_at,id").map_err(SqliteStoreError::Storage)?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(SqliteStoreError::Storage)?;
    let mut survivors = BTreeMap::new();
    let mut repairs = Vec::new();
    for row in rows {
        let (watch_id, kind, id) = row.map_err(SqliteStoreError::Storage)?;
        match survivors.entry((watch_id, kind)) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(id);
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                repairs.push((entry.get().clone(), id))
            }
        }
    }
    drop(statement);
    for (surviving, archived) in repairs {
        tx.execute("INSERT INTO migration_repair(migration_version,surviving_rule_id,archived_rule_id,reason,repaired_at) VALUES (5,?,?,?,strftime('%Y-%m-%dT%H:%M:%fZ','now'))", params![surviving, archived, "duplicate active rule kind; kept earliest created_at then id"]).map_err(SqliteStoreError::Storage)?;
        tx.execute("UPDATE rule SET state='archived',enabled=0,updated_at=(SELECT repaired_at FROM migration_repair WHERE archived_rule_id=?),archived_at=(SELECT repaired_at FROM migration_repair WHERE archived_rule_id=?) WHERE id=?", params![archived, archived, archived]).map_err(SqliteStoreError::Storage)?;
    }
    tx.execute_batch("CREATE UNIQUE INDEX one_live_rule_kind_per_watch ON rule(watch_id, kind) WHERE state != 'archived';").map_err(SqliteStoreError::Storage)
}

fn create_v1(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch(r#"
        CREATE TABLE subject (
          id INTEGER PRIMARY KEY, subject_key TEXT NOT NULL UNIQUE, kind TEXT NOT NULL, canonical_url TEXT NOT NULL,
          title TEXT NOT NULL, revision TEXT, metadata_refreshed_at TEXT, created_at TEXT NOT NULL, updated_at TEXT NOT NULL
        );
        CREATE TABLE watch (
          id TEXT PRIMARY KEY, subject_id INTEGER NOT NULL REFERENCES subject(id), state TEXT NOT NULL CHECK(state IN ('active','paused','archived')),
          created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT
        );
        CREATE TABLE rule (
          id TEXT PRIMARY KEY, watch_id TEXT NOT NULL REFERENCES watch(id), kind TEXT NOT NULL,
          enabled INTEGER NOT NULL CHECK(enabled IN (0,1)), current_version INTEGER NOT NULL CHECK(current_version > 0),
          state TEXT NOT NULL CHECK(state IN ('active','archived')), created_at TEXT NOT NULL, updated_at TEXT NOT NULL, archived_at TEXT
        );
        CREATE TABLE rule_definition (
          rule_id TEXT NOT NULL REFERENCES rule(id), version INTEGER NOT NULL CHECK(version > 0), encoding_version INTEGER NOT NULL,
          definition TEXT NOT NULL, created_at TEXT NOT NULL, PRIMARY KEY(rule_id, version), UNIQUE(rule_id, version)
        );
        CREATE TABLE observation (
          id INTEGER PRIMARY KEY, rule_id TEXT NOT NULL REFERENCES rule(id), rule_version INTEGER NOT NULL,
          revision TEXT NOT NULL, state TEXT NOT NULL, source_identity TEXT, source_url TEXT, detail TEXT, observed_at TEXT NOT NULL,
          first_observed_at TEXT NOT NULL, first_source_seen_at TEXT,
          UNIQUE(rule_id, rule_version, revision), FOREIGN KEY(rule_id, rule_version) REFERENCES rule_definition(rule_id, version)
        );
        CREATE TABLE alert (
          id TEXT PRIMARY KEY, rule_id TEXT NOT NULL REFERENCES rule(id), rule_version INTEGER NOT NULL, watch_id TEXT NOT NULL REFERENCES watch(id), subject_key TEXT NOT NULL, rule_kind TEXT NOT NULL,
          revision TEXT NOT NULL, event_kind TEXT NOT NULL CHECK(event_kind IN ('missing','started','terminal')), source_identity TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
          status TEXT NOT NULL CHECK(status IN ('pending','acknowledged')), source_url TEXT, created_at TEXT NOT NULL, acknowledged_at TEXT,
          UNIQUE(rule_id, rule_version, revision, event_kind, source_identity), FOREIGN KEY(rule_id, rule_version) REFERENCES rule_definition(rule_id, version)
        );
        CREATE TABLE poll_attempt (
          id TEXT PRIMARY KEY, refresh_id TEXT NOT NULL, watch_id TEXT NOT NULL REFERENCES watch(id), subject_id INTEGER NOT NULL REFERENCES subject(id), revision TEXT, started_at TEXT NOT NULL, finished_at TEXT NOT NULL,
          outcome TEXT NOT NULL, error_category TEXT, detail TEXT
        );
        CREATE TABLE source_issue (
          id INTEGER PRIMARY KEY, poll_attempt_id TEXT NOT NULL REFERENCES poll_attempt(id), rule_id TEXT REFERENCES rule(id),
          scope TEXT NOT NULL, provider TEXT NOT NULL, kind TEXT NOT NULL, retryable INTEGER NOT NULL CHECK(retryable IN (0,1)), message TEXT NOT NULL, created_at TEXT NOT NULL
        );
        CREATE TABLE setting (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at TEXT NOT NULL);
        CREATE TABLE lease (name TEXT PRIMARY KEY, owner TEXT NOT NULL, expires_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
        CREATE TABLE import_record (source_fingerprint TEXT PRIMARY KEY, imported_at TEXT NOT NULL, source_path TEXT NOT NULL);
        CREATE TABLE migration_repair (id INTEGER PRIMARY KEY, migration_version INTEGER NOT NULL, surviving_rule_id TEXT NOT NULL, archived_rule_id TEXT NOT NULL UNIQUE, reason TEXT NOT NULL, repaired_at TEXT NOT NULL);
        CREATE UNIQUE INDEX one_live_watch_per_subject ON watch(subject_id) WHERE state != 'archived';
        CREATE UNIQUE INDEX one_live_rule_kind_per_watch ON rule(watch_id, kind) WHERE state != 'archived';
        CREATE INDEX observation_rule_latest ON observation(rule_id, rule_version, observed_at DESC);
        CREATE INDEX alert_created ON alert(created_at DESC);
        CREATE INDEX poll_subject_latest ON poll_attempt(subject_id, finished_at DESC);
    "#).map_err(SqliteStoreError::Storage)
}

#[async_trait]
impl CatalogRepository for SqliteStore {
    async fn add_watch(&self, draft: NewWatch) -> std::result::Result<Watch, StoreError> {
        let mut c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let tx = c.transaction().map_err(store_error)?;
        if let Some((id,state)) = tx
            .query_row(
                "SELECT w.id,w.state FROM subject s JOIN watch w ON w.subject_id=s.id WHERE s.subject_key=? ORDER BY w.created_at DESC LIMIT 1",
                [draft.subject.key.as_str()],
                |r| Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)),
            )
            .optional()
            .map_err(store_error)?
        {
            return Err(store_error(if state == "paused" { format!("watch {id} is paused; run airborne watch resume {id}") } else if state == "archived" { format!("watch {id} is archived; archived history prevents re-adding this subject") } else { format!("watch {id} already exists and is active") }));
        }
        tx.execute("INSERT INTO subject(subject_key,kind,canonical_url,title,revision,metadata_refreshed_at,created_at,updated_at) VALUES (?1,'github_pull_request',?2,?3,?4,?5,?6,?6) ON CONFLICT(subject_key) DO UPDATE SET canonical_url=excluded.canonical_url,title=excluded.title,revision=excluded.revision,metadata_refreshed_at=excluded.metadata_refreshed_at,updated_at=excluded.updated_at", params![draft.subject.key.as_str(), draft.subject.canonical_url, draft.subject.display_title, draft.subject.current_revision.as_ref().map(|x| x.as_str()), draft.subject.metadata_refreshed_at.as_ref().map(sql_timestamp), sql_timestamp(&draft.subject.created_at)]).map_err(store_error)?;
        let subject_id: i64 = tx
            .query_row(
                "SELECT id FROM subject WHERE subject_key=?",
                [&draft.subject.key.as_str()],
                |r| r.get(0),
            )
            .map_err(store_error)?;
        let id = WatchId::new(uid("watch")).map_err(store_error)?;
        tx.execute("INSERT INTO watch(id,subject_id,state,created_at,updated_at,archived_at) VALUES (?,?,?,?,?,?)", params![id.as_str(), subject_id, watch_state_text(draft.state), sql_timestamp(&draft.subject.created_at), sql_timestamp(&draft.subject.created_at), if draft.state == WatchState::Archived { Some(sql_timestamp(&draft.subject.created_at)) } else { None }]).map_err(store_error)?;
        let watch = tx.query_row("SELECT w.id,s.subject_key,w.state,w.created_at,w.updated_at,w.archived_at FROM watch w JOIN subject s ON s.id=w.subject_id WHERE w.id=?", [id.as_str()], read_watch).map_err(store_error)?;
        tx.commit().map_err(store_error)?;
        Ok(watch)
    }
    async fn list_watches(
        &self,
        state: Option<WatchState>,
    ) -> std::result::Result<Vec<Watch>, StoreError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let mut statement = c.prepare("SELECT w.id,s.subject_key,w.state,w.created_at,w.updated_at,w.archived_at FROM watch w JOIN subject s ON s.id=w.subject_id WHERE (?1 IS NULL OR w.state=?1) ORDER BY w.created_at,w.id").map_err(store_error)?;
        let watches = statement
            .query_map([state.map(watch_state_text)], read_watch)
            .map_err(store_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(store_error)?;
        Ok(watches)
    }
    async fn change_watch_state(
        &self,
        id: WatchId,
        state: WatchState,
        at: Timestamp,
    ) -> std::result::Result<Watch, StoreError> {
        let mut c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let tx = c.transaction().map_err(store_error)?;
        let previous: String = tx
            .query_row("SELECT state FROM watch WHERE id=?", [id.as_str()], |r| {
                r.get(0)
            })
            .optional()
            .map_err(store_error)?
            .ok_or_else(|| store_error("watch was not found"))?;
        let changed = tx.execute("UPDATE watch SET state=?,updated_at=?,archived_at=CASE WHEN ?='archived' THEN ? ELSE archived_at END WHERE id=?", params![watch_state_text(state),sql_timestamp(&at),watch_state_text(state),sql_timestamp(&at),id.as_str()]).map_err(store_error)?;
        if changed == 0 {
            return Err(store_error("watch was not found"));
        }
        if previous == "paused" && state == WatchState::Active {
            tx.execute("INSERT INTO rule_definition(rule_id,version,encoding_version,definition,created_at) SELECT d.rule_id,d.version + 1,d.encoding_version,d.definition,? FROM rule_definition d JOIN rule r ON r.id=d.rule_id WHERE r.watch_id=? AND r.enabled=1 AND r.state='active' AND d.version=r.current_version",params![sql_timestamp(&at),id.as_str()]).map_err(store_error)?;
            tx.execute("UPDATE rule SET current_version=current_version+1,updated_at=? WHERE watch_id=? AND enabled=1 AND state='active'",params![sql_timestamp(&at),id.as_str()]).map_err(store_error)?;
        }
        let watch=tx.query_row("SELECT w.id,s.subject_key,w.state,w.created_at,w.updated_at,w.archived_at FROM watch w JOIN subject s ON s.id=w.subject_id WHERE w.id=?", [id.as_str()], read_watch).map_err(store_error)?;
        tx.commit().map_err(store_error)?;
        Ok(watch)
    }
    async fn add_rule(&self, draft: NewRule) -> std::result::Result<Rule, StoreError> {
        draft.config.validate().map_err(store_error)?;
        let mut c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let tx = c
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        let state: Option<String> = tx
            .query_row(
                "SELECT state FROM watch WHERE id=?",
                [draft.watch_id.as_str()],
                |r| r.get(0),
            )
            .optional()
            .map_err(store_error)?;
        match state.as_deref() {
            None => {
                return Err(store_error(format!(
                    "watch {} was not found",
                    draft.watch_id
                )))
            }
            Some("archived") => {
                return Err(store_error(format!("watch {} is archived", draft.watch_id)))
            }
            _ => {}
        }
        if let Some(existing_rule_id) =
            existing_live_rule(&tx, &draft.watch_id, draft.config.kind())?
        {
            return Err(StoreError::RuleKindConflict {
                watch_id: draft.watch_id,
                kind: draft.config.kind(),
                existing_rule_id,
            });
        }
        let id = RuleId::new(uid("rule")).map_err(store_error)?;
        let at = sql_timestamp(&draft.created_at);
        let json = serde_json::to_string(&draft.config).map_err(store_error)?;
        if let Err(error) = tx.execute("INSERT INTO rule(id,watch_id,kind,enabled,current_version,state,created_at,updated_at) VALUES (?,?,?,?,1,'active',?,?)", params![id.as_str(),draft.watch_id.as_str(),rule_kind_text(draft.config.kind()),draft.enabled,at,at]) {
            if let Some(existing_rule_id) = existing_live_rule(&tx, &draft.watch_id, draft.config.kind())? {
                return Err(StoreError::RuleKindConflict {
                    watch_id: draft.watch_id,
                    kind: draft.config.kind(),
                    existing_rule_id,
                });
            }
            return Err(store_error(error));
        }
        tx.execute("INSERT INTO rule_definition(rule_id,version,encoding_version,definition,created_at) VALUES (?,1,1,?,?)", params![id.as_str(),json,sql_timestamp(&draft.created_at)]).map_err(store_error)?;
        let rule = tx.query_row("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE id=?", [id.as_str()], read_rule).map_err(store_error)?;
        tx.commit().map_err(store_error)?;
        Ok(rule)
    }
    async fn update_rule(&self, change: RuleChange) -> std::result::Result<Rule, StoreError> {
        change.config.validate().map_err(store_error)?;
        let mut c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let tx = c.transaction().map_err(store_error)?;
        let current: Rule = tx.query_row("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE id=? AND state='active'", [change.id.as_str()], read_rule).optional().map_err(store_error)?.ok_or_else(|| store_error("rule was not found"))?;
        if current.kind != change.config.kind() {
            return Err(store_error("rule kind cannot change"));
        }
        let next = current
            .current_version
            .next()
            .ok_or_else(|| store_error("rule version overflow"))?;
        let at = sql_timestamp(&change.at);
        tx.execute("INSERT INTO rule_definition(rule_id,version,encoding_version,definition,created_at) VALUES (?,?,?,?,?)", params![change.id.as_str(),next.get(),1,serde_json::to_string(&change.config).map_err(store_error)?,at]).map_err(store_error)?;
        tx.execute(
            "UPDATE rule SET current_version=?,updated_at=? WHERE id=?",
            params![next.get(), at, change.id.as_str()],
        )
        .map_err(store_error)?;
        let rule=tx.query_row("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE id=?",[change.id.as_str()],read_rule).map_err(store_error)?;
        tx.commit().map_err(store_error)?;
        Ok(rule)
    }
    async fn change_rule_state(
        &self,
        id: RuleId,
        enabled: bool,
        at: Timestamp,
    ) -> std::result::Result<Rule, StoreError> {
        let mut c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let tx = c.transaction().map_err(store_error)?;
        let current: Rule=tx.query_row("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE id=? AND state='active'",[id.as_str()],read_rule).optional().map_err(store_error)?.ok_or_else(||store_error("rule was not found"))?;
        let version = if enabled && !current.enabled {
            let config: String = tx
                .query_row(
                    "SELECT definition FROM rule_definition WHERE rule_id=? AND version=?",
                    params![id.as_str(), current.current_version.get()],
                    |r| r.get(0),
                )
                .map_err(store_error)?;
            let next = current
                .current_version
                .next()
                .ok_or_else(|| store_error("rule version overflow"))?;
            tx.execute("INSERT INTO rule_definition(rule_id,version,encoding_version,definition,created_at) VALUES (?,?,?,?,?)",params![id.as_str(),next.get(),1,config,sql_timestamp(&at)]).map_err(store_error)?;
            next
        } else {
            current.current_version
        };
        tx.execute(
            "UPDATE rule SET enabled=?,current_version=?,updated_at=? WHERE id=?",
            params![enabled, version.get(), sql_timestamp(&at), id.as_str()],
        )
        .map_err(store_error)?;
        let rule=tx.query_row("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE id=?",[id.as_str()],read_rule).map_err(store_error)?;
        tx.commit().map_err(store_error)?;
        Ok(rule)
    }
}

#[async_trait]
impl RuntimeStore for SqliteStore {
    async fn load_refresh_targets(
        &self,
        scope: RefreshScope,
    ) -> std::result::Result<Vec<RefreshTarget>, StoreError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let watch_filter = match scope {
            RefreshScope::AllActive => None,
            RefreshScope::Watch(ref id) => Some(id.as_str()),
        };
        let mut st=c.prepare("SELECT s.id,s.subject_key,s.kind,s.canonical_url,s.title,s.revision,s.metadata_refreshed_at,s.created_at,w.id,s.subject_key,w.state,w.created_at,w.updated_at,w.archived_at FROM watch w JOIN subject s ON s.id=w.subject_id WHERE w.state='active' AND (?1 IS NULL OR w.id=?1) ORDER BY w.id").map_err(store_error)?;
        let rows = st
            .query_map([watch_filter], |r| {
                let subject = read_subject(r)?;
                let watch = read_watch_at(r, 8)?;
                Ok((subject.0, subject.1, watch))
            })
            .map_err(store_error)?;
        let mut output = Vec::new();
        for row in rows {
            let (subject_id, subject, watch) = row.map_err(store_error)?;
            let mut rules=c.prepare("SELECT id,watch_id,kind,enabled,current_version,created_at,updated_at,archived_at FROM rule WHERE watch_id=? AND enabled=1 AND state='active' ORDER BY id").map_err(store_error)?;
            let active = rules
                .query_map([watch.id.as_str()], read_rule)
                .map_err(store_error)?;
            let mut versioned = Vec::new();
            for rule in active {
                let rule = rule.map_err(store_error)?;
                let (encoding,definition,created):(i64,String,String)=c.query_row("SELECT encoding_version,definition,created_at FROM rule_definition WHERE rule_id=? AND version=?",params![rule.id.as_str(),rule.current_version.get()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(store_error)?;
                if encoding != 1 {
                    return Err(store_error(
                        "database has an unsupported rule definition version",
                    ));
                }
                let config: RuleConfig = serde_json::from_str(&definition).map_err(store_error)?;
                if config.kind() != rule.kind {
                    return Err(store_error(
                        "stored rule definition does not match its rule",
                    ));
                }
                versioned.push(VersionedRule {
                    definition: RuleDefinition {
                        rule_id: rule.id.clone(),
                        version: rule.current_version,
                        config,
                        created_at: text_timestamp(created).map_err(store_error)?,
                    },
                    rule,
                });
            }
            let _ = subject_id;
            output.push(RefreshTarget {
                watch,
                subject,
                rules: versioned,
            });
        }
        Ok(output)
    }
    async fn load_rule_history(
        &self,
        candidates: &[(RuleKey, airborne_core::Revision)],
    ) -> std::result::Result<RuleHistorySet, StoreError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let mut result = RuleHistorySet::default();
        for (key, candidate_revision) in candidates {
            let (watch_id, kind): (String, String) = c
                .query_row(
                    "SELECT watch_id,kind FROM rule WHERE id=?",
                    [key.rule_id.as_str()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(store_error)?;
            let latest=c.query_row("SELECT rule_id,rule_version,revision,state,source_identity,source_url,detail,observed_at,first_observed_at,first_source_seen_at FROM observation WHERE rule_id=? AND rule_version=? ORDER BY observed_at DESC,id DESC LIMIT 1",params![key.rule_id.as_str(),key.rule_version.get()],|r| { let rid:String=r.get(0)?;let ver:u64=r.get(1)?;let rev:String=r.get(2)?;let state:String=r.get(3)?;let source:Option<String>=r.get(4)?;let url:Option<String>=r.get(5)?;let detail:Option<String>=r.get(6)?;let at:String=r.get(7)?;let first:String=r.get(8)?;let first_source:Option<String>=r.get(9)?; Ok(Observation { rule_id:RuleId::new(rid).map_err(domain_sql)?,rule_version:RuleVersion::new(ver).map_err(domain_sql)?,revision:airborne_core::Revision::new(rev).map_err(domain_sql)?,state:parse_state(state).map_err(store_sql)?,source_identity:source.map(SourceIdentity::new).transpose().map_err(domain_sql)?,source_url:url,detail,observed_at:text_timestamp(at).map_err(store_sql)?,first_observed_at:text_timestamp(first).map_err(store_sql)?,first_source_seen_at:first_source.map(text_timestamp).transpose().map_err(store_sql)? }) }).optional().map_err(store_error)?;
            let lifecycle = c.query_row("SELECT first_observed_at,first_source_seen_at FROM observation WHERE rule_id=? AND rule_version=? AND revision=?", params![key.rule_id.as_str(), key.rule_version.get(), candidate_revision.as_str()], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))).optional().map_err(store_error)?;
            let mut alerts = BTreeSet::new();
            let mut statement = c
                .prepare(
                    "SELECT revision,event_kind,source_identity FROM alert WHERE rule_id=? AND rule_version=?",
                )
                .map_err(store_error)?;
            for row in statement
                .query_map(params![key.rule_id.as_str(), key.rule_version.get()], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                    ))
                })
                .map_err(store_error)?
            {
                let (revision, event_kind, source) = row.map_err(store_error)?;
                alerts.insert(AlertKey {
                    rule_id: key.rule_id.clone(),
                    rule_version: key.rule_version,
                    revision: airborne_core::Revision::new(revision).map_err(store_error)?,
                    event_kind: parse_alert_event_kind(event_kind)?,
                    source_identity: SourceIdentity::new(source).map_err(store_error)?,
                });
            }
            result.insert(
                key.clone(),
                RuleHistory {
                    watch_id: WatchId::new(watch_id).map_err(store_error)?,
                    rule_kind: parse_rule_kind(kind)?,
                    first_observed_at: lifecycle
                        .as_ref()
                        .map(|(first, _)| text_timestamp(first.clone()))
                        .transpose()?,
                    first_source_seen_at: lifecycle
                        .as_ref()
                        .and_then(|(_, first)| first.clone())
                        .map(text_timestamp)
                        .transpose()?,
                    latest_observation: latest,
                    existing_alert_keys: alerts,
                },
            );
        }
        Ok(result)
    }
    async fn apply_poll(
        &self,
        commit: airborne_runtime::PollCommit,
    ) -> std::result::Result<airborne_runtime::AppliedPoll, StoreError> {
        let mut c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let tx = c.transaction().map_err(store_error)?;
        let subject_id: i64 = tx
            .query_row(
                "SELECT id FROM subject WHERE subject_key=?",
                [commit.attempt.subject_key.as_str()],
                |r| r.get(0),
            )
            .map_err(store_error)?;
        let attempt_id = uid("attempt");
        tx.execute("INSERT INTO poll_attempt(id,refresh_id,watch_id,subject_id,revision,started_at,finished_at,outcome) VALUES (?,?,?,?,?,?,?,?)",params![attempt_id,commit.attempt.refresh_id.as_str(),commit.attempt.watch_id.as_str(),subject_id,commit.attempt.revision.as_ref().map(airborne_core::Revision::as_str),sql_timestamp(&commit.attempt.started_at),sql_timestamp(&commit.attempt.finished_at),format!("{:?}",commit.attempt.outcome).to_lowercase()]).map_err(store_error)?;
        if let Some(metadata) = commit.metadata {
            tx.execute("UPDATE subject SET title=?,revision=?,metadata_refreshed_at=?,updated_at=? WHERE id=?",params![metadata.display_title,metadata.current_revision.as_str(),sql_timestamp(&metadata.metadata_refreshed_at),sql_timestamp(&metadata.metadata_refreshed_at),subject_id]).map_err(store_error)?;
        }
        for item in commit.observations {
            tx.execute("INSERT INTO observation(rule_id,rule_version,revision,state,source_identity,source_url,detail,observed_at,first_observed_at,first_source_seen_at) VALUES (?,?,?,?,?,?,?,?,?,?) ON CONFLICT(rule_id,rule_version,revision) DO UPDATE SET state=excluded.state,source_identity=excluded.source_identity,source_url=excluded.source_url,detail=excluded.detail,observed_at=excluded.observed_at,first_observed_at=observation.first_observed_at,first_source_seen_at=COALESCE(observation.first_source_seen_at, excluded.first_source_seen_at)",params![item.rule_id.as_str(),item.rule_version.get(),item.revision.as_str(),state_text(item.state),item.source_identity.as_ref().map(|x|x.as_str()),item.source_url,item.detail,sql_timestamp(&item.observed_at),sql_timestamp(&item.first_observed_at),item.first_source_seen_at.as_ref().map(sql_timestamp)]).map_err(store_error)?;
        }
        let mut inserted = Vec::new();
        for item in commit.alerts {
            let id = AlertId::new(uid("alert")).map_err(store_error)?;
            if tx.execute("INSERT OR IGNORE INTO alert(id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,event_kind,source_identity,title,body,status,source_url,created_at) VALUES (?,?,?,?,?,?,?,?,?,?,?,'pending',?,?)",params![id.as_str(),item.key.rule_id.as_str(),item.key.rule_version.get(),item.watch_id.as_str(),item.subject_key.as_str(),rule_kind_text(item.rule_kind),item.key.revision.as_str(),alert_event_kind_text(item.key.event_kind),item.key.source_identity.as_str(),item.title,item.body,item.source_url,sql_timestamp(&item.created_at)]).map_err(store_error)?==1 { inserted.push(Alert {id,key:item.key,watch_id:item.watch_id,subject_key:item.subject_key,rule_kind:item.rule_kind,title:item.title,body:item.body,source_url:item.source_url,created_at:item.created_at,acknowledged_at:None}); }
        }
        for issue in commit.issues {
            tx.execute("INSERT INTO source_issue(poll_attempt_id,rule_id,scope,provider,kind,retryable,message,created_at) VALUES (?,?,?,?,?,?,?,?)",params![attempt_id,issue.rule_id.as_ref().map(airborne_core::RuleId::as_str),format!("{:?}",issue.scope).to_lowercase(),format!("{:?}",issue.provider).to_lowercase(),issue.kind,issue.retryable,issue.safe_message,sql_timestamp(&commit.attempt.finished_at)]).map_err(store_error)?;
        }
        tx.commit().map_err(store_error)?;
        Ok(airborne_runtime::AppliedPoll {
            new_alerts: inserted,
        })
    }
}

#[derive(Debug)]
struct SqliteLeaseGuard {
    connection: Arc<Mutex<Connection>>,
    name: &'static str,
    owner: String,
}
impl SqliteLeaseGuard {
    fn renew_sync(&self) -> std::result::Result<(), LeaseError> {
        let now = now_epoch();
        let changed = self
            .connection
            .lock()
            .map_err(|_| lease_error("database lock failed"))?
            .execute(
                "UPDATE lease SET expires_at=?,updated_at=? WHERE name=? AND owner=? AND expires_at>?",
                params![now + 30, now, self.name, self.owner, now],
            )
            .map_err(lease_error)?;
        if changed == 1 {
            Ok(())
        } else {
            Err(LeaseError::Busy { kind: self.name })
        }
    }
}
#[async_trait]
impl LeaseGuard for SqliteLeaseGuard {
    async fn renew(&self) -> std::result::Result<(), LeaseError> {
        self.renew_sync()
    }
    async fn release(&self) -> std::result::Result<(), LeaseError> {
        self.connection
            .lock()
            .map_err(|_| lease_error("database lock failed"))?
            .execute(
                "DELETE FROM lease WHERE name=? AND owner=?",
                params![self.name, self.owner],
            )
            .map_err(lease_error)?;
        Ok(())
    }
}
impl SqliteStore {
    fn acquire_lease(
        &self,
        name: &'static str,
        wait: Duration,
    ) -> std::result::Result<Arc<dyn LeaseGuard>, LeaseError> {
        let owner = uid("lease");
        let deadline = std::time::Instant::now() + wait;
        loop {
            let now = now_epoch();
            let expires = now + 30;
            let mut c = self
                .connection
                .lock()
                .map_err(|_| lease_error("database lock failed"))?;
            let tx = c.transaction().map_err(lease_error)?;
            let changed=tx.execute("INSERT INTO lease(name,owner,expires_at,updated_at) VALUES (?,?,?,?) ON CONFLICT(name) DO UPDATE SET owner=excluded.owner,expires_at=excluded.expires_at,updated_at=excluded.updated_at WHERE lease.expires_at <= excluded.updated_at",params![name,owner,expires,now]).map_err(lease_error)?;
            if changed == 1 {
                tx.commit().map_err(lease_error)?;
                return Ok(Arc::new(SqliteLeaseGuard {
                    connection: Arc::clone(&self.connection),
                    name,
                    owner,
                }));
            }
            drop(tx);
            drop(c);
            if std::time::Instant::now() >= deadline {
                return Err(LeaseError::Busy { kind: name });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}
#[async_trait]
impl LeaseStore for SqliteStore {
    async fn acquire_refresh(
        &self,
        wait: Duration,
    ) -> std::result::Result<RefreshLease, LeaseError> {
        self.acquire_lease("refresh", wait).map(RefreshLease::new)
    }
    async fn acquire_runner(&self) -> std::result::Result<RunnerLease, LeaseError> {
        self.acquire_lease("runner", Duration::ZERO)
            .map(RunnerLease::new)
    }
}

#[async_trait]
impl AlertRepository for SqliteStore {
    async fn list_alerts(
        &self,
        pending_only: bool,
        watch: Option<WatchId>,
    ) -> std::result::Result<Vec<Alert>, StoreError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let mut st=c.prepare("SELECT a.id,a.rule_id,a.rule_version,a.revision,a.event_kind,a.source_identity,r.watch_id,s.subject_key,r.kind,a.title,a.body,a.source_url,a.created_at,a.acknowledged_at FROM alert a JOIN rule r ON r.id=a.rule_id JOIN watch w ON w.id=r.watch_id JOIN subject s ON s.id=w.subject_id WHERE (?1=0 OR a.status='pending') AND (?2 IS NULL OR r.watch_id=?2) ORDER BY a.created_at DESC,a.id DESC").map_err(store_error)?;
        let alerts = st
            .query_map(
                params![pending_only, watch.as_ref().map(|x| x.as_str())],
                |r| {
                    let id: String = r.get(0)?;
                    let rule: String = r.get(1)?;
                    let version: u64 = r.get(2)?;
                    let revision: String = r.get(3)?;
                    let event_kind: String = r.get(4)?;
                    let source: String = r.get(5)?;
                    let watch: String = r.get(6)?;
                    let subject: String = r.get(7)?;
                    let kind: String = r.get(8)?;
                    let title: String = r.get(9)?;
                    let body: String = r.get(10)?;
                    let source_url: Option<String> = r.get(11)?;
                    let created: String = r.get(12)?;
                    let acknowledged: Option<String> = r.get(13)?;
                    Ok(Alert {
                        id: AlertId::new(id).map_err(domain_sql)?,
                        key: AlertKey {
                            rule_id: RuleId::new(rule).map_err(domain_sql)?,
                            rule_version: RuleVersion::new(version).map_err(domain_sql)?,
                            revision: airborne_core::Revision::new(revision).map_err(domain_sql)?,
                            event_kind: parse_alert_event_kind(event_kind).map_err(store_sql)?,
                            source_identity: SourceIdentity::new(source).map_err(domain_sql)?,
                        },
                        watch_id: WatchId::new(watch).map_err(domain_sql)?,
                        subject_key: SubjectKey::new(subject).map_err(domain_sql)?,
                        rule_kind: parse_rule_kind(kind).map_err(store_sql)?,
                        title,
                        body,
                        source_url,
                        created_at: text_timestamp(created).map_err(store_sql)?,
                        acknowledged_at: acknowledged
                            .map(text_timestamp)
                            .transpose()
                            .map_err(store_sql)?,
                    })
                },
            )
            .map_err(store_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(store_error)?;
        Ok(alerts)
    }
    async fn acknowledge(
        &self,
        ids: &[AlertId],
        at: Timestamp,
    ) -> std::result::Result<usize, StoreError> {
        let c = self
            .connection
            .lock()
            .map_err(|_| store_error("database lock failed"))?;
        let mut count = 0;
        for id in ids {
            count+=c.execute("UPDATE alert SET status='acknowledged',acknowledged_at=? WHERE id=? AND status='pending'",params![sql_timestamp(&at),id.as_str()]).map_err(store_error)?;
        }
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn makes_all_logical_tables_and_reopens() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let store = SqliteStore::open(file.path()).unwrap();
        assert_eq!(store.schema_version().unwrap(), 5);
        store.integrity_check().unwrap();
        drop(store);
        assert_eq!(
            SqliteStore::open(file.path())
                .unwrap()
                .schema_version()
                .unwrap(),
            5
        );
    }
    #[test]
    fn foreign_keys_and_unique_active_watch_are_enforced() {
        let store = SqliteStore::memory().unwrap();
        let c = store.connection.lock().unwrap();
        assert!(c.execute("INSERT INTO watch(subject_id,state,created_at,updated_at) VALUES (99,'active','x','x')", []).is_err());
        c.execute("INSERT INTO subject(subject_key,kind,canonical_url,title,created_at,updated_at) VALUES ('x','github_pull_request','https://github.com/a/b/pull/1','t','x','x')", []).unwrap();
        c.execute("INSERT INTO watch(id,subject_id,state,created_at,updated_at) VALUES ('a',1,'active','x','x')", []).unwrap();
        assert!(c.execute("INSERT INTO watch(id,subject_id,state,created_at,updated_at) VALUES ('b',1,'paused','x','x')", []).is_err());
    }
    #[test]
    fn one_live_rule_kind_keeps_disabled_rows_and_archive_frees_the_kind() {
        let store = SqliteStore::memory().unwrap();
        let c = store.connection.lock().unwrap();
        c.execute_batch("INSERT INTO subject(subject_key,kind,canonical_url,title,created_at,updated_at) VALUES ('s','github_pull_request','u','t','x','x'); INSERT INTO watch(id,subject_id,state,created_at,updated_at) VALUES ('w',1,'active','x','x'); INSERT INTO rule(id,watch_id,kind,enabled,current_version,state,created_at,updated_at) VALUES ('first','w','github_check_completes',0,1,'active','x','x');").unwrap();
        assert!(c.execute("INSERT INTO rule(id,watch_id,kind,enabled,current_version,state,created_at,updated_at) VALUES ('second','w','github_check_completes',1,1,'active','y','y')", []).is_err());
        c.execute(
            "UPDATE rule SET state='archived',archived_at='z' WHERE id='first'",
            [],
        )
        .unwrap();
        c.execute("INSERT INTO rule(id,watch_id,kind,enabled,current_version,state,created_at,updated_at) VALUES ('second','w','github_check_completes',1,1,'active','y','y')", []).unwrap();
    }
    #[test]
    fn alert_event_kind_is_part_of_the_unique_key() {
        let store = SqliteStore::memory().unwrap();
        let c = store.connection.lock().unwrap();
        c.execute_batch("INSERT INTO subject(subject_key,kind,canonical_url,title,created_at,updated_at) VALUES ('s','github_pull_request','u','t','x','x'); INSERT INTO watch(id,subject_id,state,created_at,updated_at) VALUES ('w',1,'active','x','x'); INSERT INTO rule(id,watch_id,kind,enabled,current_version,state,created_at,updated_at) VALUES ('r','w','github_check_completes',1,1,'active','x','x'); INSERT INTO rule_definition VALUES ('r',1,1,'{\"kind\":\"git_hub_check_completes\",\"check_name\":\"x\"}','x'); INSERT INTO alert(id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,event_kind,source_identity,title,body,status,created_at) VALUES ('started','r',1,'w','s','github_check_completes','v','started','source','t','b','pending','x'); INSERT INTO alert(id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,event_kind,source_identity,title,body,status,created_at) VALUES ('terminal','r',1,'w','s','github_check_completes','v','terminal','source','t','b','pending','x');").unwrap();
        assert!(c.execute("INSERT INTO alert(id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,event_kind,source_identity,title,body,status,created_at) VALUES ('duplicate','r',1,'w','s','github_check_completes','v','terminal','source','t','b','pending','x')", []).is_err());
    }
    #[test]
    fn concurrent_file_database_rule_adds_return_a_typed_conflict() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let store = SqliteStore::open(file.path()).unwrap();
        {
            let c = store.connection.lock().unwrap();
            c.execute_batch("INSERT INTO subject(subject_key,kind,canonical_url,title,created_at,updated_at) VALUES ('s','github_pull_request','u','t','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z'); INSERT INTO watch(id,subject_id,state,created_at,updated_at) VALUES ('w',1,'active','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z');").unwrap();
        }
        drop(store);
        let path = file.path().to_path_buf();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let run = |path: PathBuf, barrier: Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                let store = SqliteStore::open(path).unwrap();
                barrier.wait();
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(store.add_rule(NewRule {
                        watch_id: WatchId::new("w").unwrap(),
                        config: RuleConfig::GitHubCheckCompletes {
                            check_name: "ci".into(),
                            alert_on_start: false,
                            alert_if_missing_after_seconds: None,
                        },
                        enabled: true,
                        created_at: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
                    }))
            })
        };
        let first = run(path.clone(), Arc::clone(&barrier));
        let second = run(path, Arc::clone(&barrier));
        barrier.wait();
        let first = first.join().unwrap();
        let second = second.join().unwrap();
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let successful_rule_id = first
            .as_ref()
            .ok()
            .or_else(|| second.as_ref().ok())
            .unwrap()
            .id
            .clone();
        let error = first.err().or(second.err()).unwrap();
        assert!(matches!(
            error,
            StoreError::RuleKindConflict {
                watch_id,
                kind: RuleKind::GitHubCheckCompletes,
                existing_rule_id,
            } if watch_id == WatchId::new("w").unwrap() && existing_rule_id == successful_rule_id
        ));
    }
    #[tokio::test]
    async fn add_and_update_reject_zero_missing_delay_before_writing() {
        let store = SqliteStore::memory().unwrap();
        {
            let c = store.connection.lock().unwrap();
            c.execute_batch("INSERT INTO subject(subject_key,kind,canonical_url,title,created_at,updated_at) VALUES ('s','github_pull_request','u','t','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z'); INSERT INTO watch(id,subject_id,state,created_at,updated_at) VALUES ('w',1,'active','2026-01-01T00:00:00Z','2026-01-01T00:00:00Z');").unwrap();
        }
        let invalid = || RuleConfig::GitHubCheckCompletes {
            check_name: "ci".into(),
            alert_on_start: false,
            alert_if_missing_after_seconds: Some(0),
        };
        assert!(store
            .add_rule(NewRule {
                watch_id: WatchId::new("w").unwrap(),
                config: invalid(),
                enabled: true,
                created_at: Timestamp::parse("2026-01-01T00:00:00Z").unwrap()
            })
            .await
            .is_err());
        assert!(store
            .update_rule(RuleChange {
                id: RuleId::new("missing").unwrap(),
                config: invalid(),
                at: Timestamp::parse("2026-01-01T00:00:00Z").unwrap()
            })
            .await
            .is_err());
        let c = store.connection.lock().unwrap();
        assert_eq!(
            c.query_row("SELECT count(*) FROM rule", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    #[test]
    fn imports_prototype_without_mutating_it_and_is_idempotent() {
        let legacy = tempfile::NamedTempFile::new().unwrap();
        let source = Connection::open(legacy.path()).unwrap();
        source.execute_batch("CREATE TABLE watch (id INTEGER PRIMARY KEY,github_owner TEXT,github_repo TEXT,github_pr_number INTEGER,title TEXT,active INTEGER,created_at TEXT,head_sha TEXT); CREATE TABLE rule (id INTEGER PRIMARY KEY,watch_id INTEGER,kind TEXT,config_json TEXT,enabled INTEGER,created_at TEXT); CREATE TABLE rule_observation (rule_id INTEGER,head_sha TEXT,state TEXT,source_identity TEXT,source_url TEXT,detail TEXT,observed_at TEXT); CREATE TABLE alert (rule_id INTEGER,head_sha TEXT,source_identity TEXT,title TEXT,body TEXT,status TEXT,created_at TEXT); CREATE TABLE setting (key TEXT PRIMARY KEY,value TEXT);").unwrap();
        source.execute("INSERT INTO watch VALUES(1,'owner','repo',7,'A title',1,'2026-09-08T00:00:00Z','abc')",[]).unwrap();
        source.execute("INSERT INTO rule VALUES(2,1,'cursor_bugbot_completed','{\"check_name\":\"Cursor Bugbot\"}',1,'2026-09-08T00:00:00Z')",[]).unwrap();
        source.execute("INSERT INTO rule_observation VALUES(2,'abc','completed','99','https://github.com','done','2026-09-08T00:01:00Z')",[]).unwrap();
        source.execute("INSERT INTO alert VALUES(2,'abc','99','Done','body','read','2026-09-08T00:01:00Z')",[]).unwrap();
        source
            .execute(
                "INSERT INTO setting VALUES('settings','{\"poll_interval_seconds\":60}')",
                [],
            )
            .unwrap();
        drop(source);
        let before = std::fs::read(legacy.path()).unwrap();
        let store = SqliteStore::memory().unwrap();
        let report = store.import_prototype(legacy.path(), false).unwrap();
        assert_eq!(
            (
                report.watches,
                report.rules,
                report.observations,
                report.alerts,
                report.settings
            ),
            (1, 1, 1, 1, 1)
        );
        assert_eq!(before, std::fs::read(legacy.path()).unwrap());
        assert!(
            store
                .import_prototype(legacy.path(), false)
                .unwrap()
                .already_imported
        );
        let c = store.connection.lock().unwrap();
        assert_eq!(
            c.query_row("SELECT status FROM alert", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "acknowledged"
        );
    }
    #[test]
    fn prototype_import_skips_an_invalid_alert_policy() {
        let legacy = tempfile::NamedTempFile::new().unwrap();
        let source = Connection::open(legacy.path()).unwrap();
        source.execute_batch("CREATE TABLE watch (id INTEGER PRIMARY KEY,github_owner TEXT,github_repo TEXT,github_pr_number INTEGER,title TEXT,active INTEGER,created_at TEXT,head_sha TEXT); CREATE TABLE rule (id INTEGER PRIMARY KEY,watch_id INTEGER,kind TEXT,config_json TEXT,enabled INTEGER,created_at TEXT); CREATE TABLE rule_observation (rule_id INTEGER,head_sha TEXT,state TEXT,source_identity TEXT,source_url TEXT,detail TEXT,observed_at TEXT); CREATE TABLE alert (rule_id INTEGER,head_sha TEXT,source_identity TEXT,title TEXT,body TEXT,status TEXT,created_at TEXT); CREATE TABLE setting (key TEXT PRIMARY KEY,value TEXT); INSERT INTO watch VALUES(1,'owner','repo',7,'A title',1,'2026-09-08T00:00:00Z','abc'); INSERT INTO rule VALUES(2,1,'cursor_bugbot_completed','{\"check_name\":\"Cursor Bugbot\",\"alert_if_missing_after_seconds\":0}',1,'2026-09-08T00:00:00Z');").unwrap();
        drop(source);
        let report = SqliteStore::memory()
            .unwrap()
            .import_prototype(legacy.path(), false)
            .unwrap();
        assert_eq!(report.rules, 0);
        assert_eq!(report.skipped, 1);
    }
    #[test]
    fn prototype_import_archives_later_duplicate_rules_and_keeps_history() {
        let legacy = tempfile::NamedTempFile::new().unwrap();
        let source = Connection::open(legacy.path()).unwrap();
        source.execute_batch("CREATE TABLE watch (id INTEGER PRIMARY KEY,github_owner TEXT,github_repo TEXT,github_pr_number INTEGER,title TEXT,active INTEGER,created_at TEXT,head_sha TEXT); CREATE TABLE rule (id INTEGER PRIMARY KEY,watch_id INTEGER,kind TEXT,config_json TEXT,enabled INTEGER,created_at TEXT); CREATE TABLE rule_observation (rule_id INTEGER,head_sha TEXT,state TEXT,source_identity TEXT,source_url TEXT,detail TEXT,observed_at TEXT); CREATE TABLE alert (rule_id INTEGER,head_sha TEXT,source_identity TEXT,title TEXT,body TEXT,status TEXT,created_at TEXT); CREATE TABLE setting (key TEXT PRIMARY KEY,value TEXT); INSERT INTO watch VALUES(1,'owner','repo',7,'A title',1,'2026-09-08T00:00:00Z','abc'); INSERT INTO rule VALUES(2,1,'cursor_bugbot_completed','{\"check_name\":\"first\"}',1,'2026-09-08T00:00:00Z'),(3,1,'cursor_bugbot_completed','{\"check_name\":\"later\"}',1,'2026-09-08T00:01:00Z'); INSERT INTO rule_observation VALUES(2,'abc','completed','first','https://example.test/first','done','2026-09-08T00:02:00Z'),(3,'abc','completed','later','https://example.test/later','done','2026-09-08T00:03:00Z'); INSERT INTO alert VALUES(2,'abc','first','first alert','body','unread','2026-09-08T00:02:00Z'),(3,'abc','later','later alert','body','read','2026-09-08T00:03:00Z');").unwrap();
        drop(source);
        let store = SqliteStore::memory().unwrap();
        let report = store.import_prototype(legacy.path(), false).unwrap();
        assert_eq!(
            (
                report.rules,
                report.observations,
                report.alerts,
                report.repaired
            ),
            (2, 2, 2, 1)
        );
        assert!(
            store
                .import_prototype(legacy.path(), false)
                .unwrap()
                .already_imported
        );
        let c = store.connection.lock().unwrap();
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM rule WHERE state='active'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            c.query_row(
                "SELECT count(*) FROM rule WHERE state='archived'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            c.query_row("SELECT count(*) FROM observation", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            c.query_row("SELECT count(*) FROM alert", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2
        );
        assert_eq!(
            c.query_row("SELECT count(*) FROM migration_repair", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
    #[tokio::test]
    async fn leases_contend_expire_and_never_release_another_owner() {
        let store = SqliteStore::memory().unwrap();
        let lease = store.acquire_refresh(Duration::ZERO).await.unwrap();
        lease.renew().await.unwrap();
        assert!(matches!(
            store.acquire_refresh(Duration::ZERO).await,
            Err(LeaseError::Busy { .. })
        ));
        let fake = SqliteLeaseGuard {
            connection: Arc::clone(&store.connection),
            name: "refresh",
            owner: "not-owner".into(),
        };
        fake.release().await.unwrap();
        assert!(matches!(
            store.acquire_refresh(Duration::ZERO).await,
            Err(LeaseError::Busy { .. })
        ));
        store
            .connection
            .lock()
            .unwrap()
            .execute("UPDATE lease SET expires_at=0 WHERE name='refresh'", [])
            .unwrap();
        let replacement = store.acquire_refresh(Duration::ZERO).await.unwrap();
        lease.release().await.unwrap();
        assert!(matches!(
            store.acquire_refresh(Duration::ZERO).await,
            Err(LeaseError::Busy { .. })
        ));
        replacement.release().await.unwrap();
    }
    #[test]
    fn dry_run_and_bad_legacy_input_do_not_persist() {
        let legacy = tempfile::NamedTempFile::new().unwrap();
        let source = Connection::open(legacy.path()).unwrap();
        source.execute_batch("CREATE TABLE watch (id INTEGER PRIMARY KEY,github_owner TEXT,github_repo TEXT,github_pr_number INTEGER,title TEXT,active INTEGER,created_at TEXT,head_sha TEXT); CREATE TABLE rule (id INTEGER PRIMARY KEY,watch_id INTEGER,kind TEXT,config_json TEXT,enabled INTEGER,created_at TEXT); CREATE TABLE rule_observation (rule_id INTEGER,head_sha TEXT,state TEXT,source_identity TEXT,source_url TEXT,detail TEXT,observed_at TEXT); CREATE TABLE alert (rule_id INTEGER,head_sha TEXT,source_identity TEXT,title TEXT,body TEXT,status TEXT,created_at TEXT); CREATE TABLE setting (key TEXT PRIMARY KEY,value TEXT); INSERT INTO watch VALUES (1,'o','r',0,'bad',1,'x',NULL);").unwrap();
        drop(source);
        let store = SqliteStore::memory().unwrap();
        let report = store.import_prototype(legacy.path(), true).unwrap();
        assert_eq!(report.skipped, 1);
        assert_eq!(
            store
                .connection
                .lock()
                .unwrap()
                .query_row("SELECT count(*) FROM subject", [], |r| r.get::<_, u64>(0))
                .unwrap(),
            0
        );
        assert!(
            !store
                .import_prototype(legacy.path(), false)
                .unwrap()
                .already_imported
        );
    }
    #[test]
    fn corrupt_database_returns_a_bounded_open_error() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"not a sqlite database").unwrap();
        let error = match SqliteStore::open(file.path()) {
            Err(error) => error,
            Ok(_) => panic!("corrupt database opened"),
        };
        assert!(!error.to_string().contains("not a sqlite database"));
    }
    #[test]
    fn failed_subject_transaction_rolls_back_all_rows() {
        let store = SqliteStore::memory().unwrap();
        let mut c = store.connection.lock().unwrap();
        let tx = c.transaction().unwrap();
        tx.execute("INSERT INTO subject(subject_key,kind,canonical_url,title,created_at,updated_at) VALUES ('s','github_pull_request','u','t','x','x')", []).unwrap();
        assert!(tx.execute("INSERT INTO observation(rule_id,rule_version,revision,state,observed_at) VALUES ('missing',1,'a','completed','x')", []).is_err());
        drop(tx);
        assert_eq!(
            c.query_row("SELECT count(*) FROM subject", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    fn legacy_database_with_alert(version: i64) -> tempfile::NamedTempFile {
        let file = tempfile::NamedTempFile::new().unwrap();
        let c = Connection::open(file.path()).unwrap();
        let source_url = if version == 1 { "" } else { ",source_url TEXT" };
        c.execute_batch(&format!("CREATE TABLE schema_migration(version INTEGER PRIMARY KEY, applied_at TEXT); INSERT INTO schema_migration VALUES({version},'x'); CREATE TABLE subject(id INTEGER PRIMARY KEY,subject_key TEXT,kind TEXT,canonical_url TEXT,title TEXT,revision TEXT,metadata_refreshed_at TEXT,created_at TEXT,updated_at TEXT); CREATE TABLE watch(id TEXT PRIMARY KEY,subject_id INTEGER,state TEXT,created_at TEXT,updated_at TEXT,archived_at TEXT); CREATE TABLE rule(id TEXT PRIMARY KEY,watch_id TEXT,kind TEXT,enabled INTEGER,current_version INTEGER,state TEXT,created_at TEXT,updated_at TEXT,archived_at TEXT); CREATE TABLE rule_definition(rule_id TEXT,version INTEGER,encoding_version INTEGER,definition TEXT,created_at TEXT,PRIMARY KEY(rule_id,version)); CREATE TABLE observation(id INTEGER PRIMARY KEY,rule_id TEXT,rule_version INTEGER,revision TEXT,state TEXT,source_identity TEXT,source_url TEXT,detail TEXT,observed_at TEXT); CREATE TABLE alert(id TEXT PRIMARY KEY,rule_id TEXT,rule_version INTEGER,revision TEXT,source_identity TEXT,title TEXT,body TEXT,status TEXT{source_url},created_at TEXT,acknowledged_at TEXT); CREATE TABLE poll_attempt(id TEXT); CREATE TABLE source_issue(id INTEGER); INSERT INTO subject VALUES(1,'s','github_pull_request','u','t',NULL,NULL,'x','x'); INSERT INTO watch VALUES('w',1,'active','x','x',NULL); INSERT INTO rule VALUES('r','w','github_check_completes',1,1,'active','x','x',NULL); INSERT INTO rule_definition VALUES('r',1,1,'{{}}','x'); INSERT INTO alert(id,rule_id,rule_version,revision,source_identity,title,body,status,created_at,acknowledged_at) VALUES('a','r',1,'v','source','t','b','pending','x',NULL);" )).unwrap();
        file
    }
    #[test]
    fn every_supported_legacy_schema_backfills_alert_display_fields() {
        for version in [1, 2, 3] {
            let file = legacy_database_with_alert(version);
            let store = SqliteStore::open(file.path()).unwrap();
            assert_eq!(store.schema_version().unwrap(), 5);
            let c = store.connection.lock().unwrap();
            assert_eq!(
                c.query_row("SELECT watch_id FROM alert", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "w"
            );
            assert_eq!(
                c.query_row("SELECT subject_key FROM alert", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                "s"
            );
            assert_eq!(
                c.query_row("SELECT rule_kind FROM alert", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "github_check_completes"
            );
            assert_eq!(
                c.query_row("SELECT event_kind FROM alert", [], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                "terminal"
            );
        }
    }
    #[test]
    fn v4_migration_preserves_lifecycle_data_and_repairs_duplicate_rules() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let c = Connection::open(file.path()).unwrap();
        c.execute_batch("CREATE TABLE schema_migration(version INTEGER PRIMARY KEY, applied_at TEXT); INSERT INTO schema_migration VALUES(4,'x'); CREATE TABLE subject(id INTEGER PRIMARY KEY,subject_key TEXT); CREATE TABLE watch(id TEXT PRIMARY KEY,subject_id INTEGER); CREATE TABLE rule(id TEXT PRIMARY KEY,watch_id TEXT,kind TEXT,enabled INTEGER,current_version INTEGER,state TEXT,created_at TEXT,updated_at TEXT,archived_at TEXT); CREATE TABLE rule_definition(rule_id TEXT,version INTEGER,encoding_version INTEGER,definition TEXT,created_at TEXT,PRIMARY KEY(rule_id,version)); CREATE TABLE observation(id INTEGER PRIMARY KEY,rule_id TEXT,rule_version INTEGER,revision TEXT,state TEXT,source_identity TEXT,source_url TEXT,detail TEXT,observed_at TEXT); CREATE TABLE alert(id TEXT PRIMARY KEY,rule_id TEXT,rule_version INTEGER,watch_id TEXT,subject_key TEXT,rule_kind TEXT,revision TEXT,source_identity TEXT,title TEXT,body TEXT,status TEXT,source_url TEXT,created_at TEXT,acknowledged_at TEXT); INSERT INTO subject VALUES(1,'s'); INSERT INTO watch VALUES('w',1); INSERT INTO rule VALUES('first','w','github_check_completes',1,1,'active','2026-01-01T00:00:00Z','x',NULL),('later','w','github_check_completes',1,1,'active','2026-01-02T00:00:00Z','x',NULL); INSERT INTO rule_definition VALUES('first',1,1,'{}','x'),('later',1,1,'{}','x'); INSERT INTO observation VALUES(1,'first',1,'v','completed','source',NULL,NULL,'2026-01-03T00:00:00Z'); INSERT INTO alert VALUES('a','first',1,'w','s','github_check_completes','v','source','t','b','pending',NULL,'x',NULL);").unwrap();
        drop(c);
        let store = SqliteStore::open(file.path()).unwrap();
        assert_eq!(store.schema_version().unwrap(), 5);
        let c = store.connection.lock().unwrap();
        assert_eq!(
            c.query_row("SELECT event_kind FROM alert", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "terminal"
        );
        assert_eq!(
            c.query_row("SELECT first_observed_at FROM observation", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "2026-01-03T00:00:00Z"
        );
        assert_eq!(
            c.query_row("SELECT state FROM rule WHERE id='later'", [], |r| r
                .get::<_, String>(0))
                .unwrap(),
            "archived"
        );
        assert_eq!(
            c.query_row("SELECT surviving_rule_id FROM migration_repair", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "first"
        );
    }
    #[test]
    fn alert_deduplication_survives_a_database_restart() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let store = SqliteStore::open(file.path()).unwrap();
        {
            let c = store.connection.lock().unwrap();
            c.execute_batch("INSERT INTO subject(subject_key,kind,canonical_url,title,created_at,updated_at) VALUES ('s','github_pull_request','u','t','x','x'); INSERT INTO watch(id,subject_id,state,created_at,updated_at) VALUES ('w',1,'active','x','x'); INSERT INTO rule(id,watch_id,kind,enabled,current_version,state,created_at,updated_at) VALUES ('r','w','github_check_completes',1,1,'active','x','x'); INSERT INTO rule_definition VALUES ('r',1,1,'{\"kind\":\"git_hub_check_completes\",\"check_name\":\"x\"}','x'); INSERT INTO alert(id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,event_kind,source_identity,title,body,status,created_at) VALUES ('a','r',1,'w','s','github_check_completes','v','terminal','source','t','b','pending','x');").unwrap();
        }
        drop(store);
        let store = SqliteStore::open(file.path()).unwrap();
        let c = store.connection.lock().unwrap();
        assert!(c.execute("INSERT INTO alert(id,rule_id,rule_version,watch_id,subject_key,rule_kind,revision,event_kind,source_identity,title,body,status,created_at) VALUES ('b','r',1,'w','s','github_check_completes','v','terminal','source','t','b','pending','x')",[]).is_err());
    }
}
