use crate::models::*;
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}
impl Store {
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }
    pub fn memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }
    fn migrate(&self) -> Result<()> {
        self.conn.lock().unwrap().execute_batch("PRAGMA foreign_keys=ON;
      CREATE TABLE IF NOT EXISTS watch (id INTEGER PRIMARY KEY, github_owner TEXT NOT NULL, github_repo TEXT NOT NULL, github_pr_number INTEGER NOT NULL, title TEXT NOT NULL, active INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL, head_sha TEXT, last_poll_at TEXT, last_error TEXT, UNIQUE(github_owner, github_repo, github_pr_number));
      CREATE TABLE IF NOT EXISTS rule (id INTEGER PRIMARY KEY, watch_id INTEGER NOT NULL REFERENCES watch(id) ON DELETE CASCADE, kind TEXT NOT NULL, config_json TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1, created_at TEXT NOT NULL);
      CREATE TABLE IF NOT EXISTS rule_observation (rule_id INTEGER NOT NULL REFERENCES rule(id) ON DELETE CASCADE, head_sha TEXT NOT NULL, state TEXT NOT NULL, source_identity TEXT, source_url TEXT, detail TEXT, observed_at TEXT NOT NULL, PRIMARY KEY(rule_id, head_sha));
      CREATE TABLE IF NOT EXISTS alert (id INTEGER PRIMARY KEY, rule_id INTEGER NOT NULL REFERENCES rule(id) ON DELETE CASCADE, head_sha TEXT NOT NULL, source_identity TEXT NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL, status TEXT NOT NULL DEFAULT 'unread', created_at TEXT NOT NULL, UNIQUE(rule_id, head_sha, source_identity));
      CREATE TABLE IF NOT EXISTS setting (key TEXT PRIMARY KEY, value TEXT NOT NULL);")?;
        Ok(())
    }
    pub fn add_watch(
        &self,
        owner: &str,
        repo: &str,
        number: i64,
        title: &str,
        head_sha: Option<&str>,
    ) -> Result<Watch> {
        let now = now();
        let c = self.conn.lock().unwrap();
        c.execute("INSERT INTO watch(github_owner,github_repo,github_pr_number,title,active,created_at,head_sha) VALUES(?,?,?,?,1,?,?)", params![owner,repo,number,title,now,head_sha])?;
        let id = c.last_insert_rowid();
        drop(c);
        self.watch(id)?.context("inserted watch missing")
    }
    pub fn watch(&self, id: i64) -> Result<Option<Watch>> {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT id,github_owner,github_repo,github_pr_number,title,active,created_at,head_sha,last_poll_at,last_error FROM watch WHERE id=?", [id], watch_row).optional().map_err(Into::into)
    }
    pub fn watches(&self) -> Result<Vec<Watch>> {
        let c = self.conn.lock().unwrap();
        let mut s=c.prepare("SELECT id,github_owner,github_repo,github_pr_number,title,active,created_at,head_sha,last_poll_at,last_error FROM watch ORDER BY id DESC")?;
        let rows = s
            .query_map([], watch_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn rules(&self) -> Result<Vec<Rule>> {
        let c = self.conn.lock().unwrap();
        let mut s = c.prepare(
            "SELECT id,watch_id,kind,config_json,enabled,created_at FROM rule ORDER BY id",
        )?;
        let rows = s
            .query_map([], rule_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn rules_for_watch(&self, watch_id: i64) -> Result<Vec<Rule>> {
        let c = self.conn.lock().unwrap();
        let mut s=c.prepare("SELECT id,watch_id,kind,config_json,enabled,created_at FROM rule WHERE watch_id=? ORDER BY id")?;
        let rows = s
            .query_map([watch_id], rule_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn add_rule(&self, watch_id: i64, input: &CreateRuleInput) -> Result<Rule> {
        let now = now();
        let config = serde_json::to_string(&input.config)?;
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO rule(watch_id,kind,config_json,enabled,created_at) VALUES(?,?,?,?,?)",
            params![watch_id, rule_kind(&input.kind), config, input.enabled, now],
        )?;
        let id = c.last_insert_rowid();
        drop(c);
        self.rules()?
            .into_iter()
            .find(|r| r.id == id)
            .context("inserted rule missing")
    }
    pub fn update_rule(&self, id: i64, input: &UpdateRuleInput) -> Result<()> {
        self.conn.lock().unwrap().execute(
            "UPDATE rule SET config_json=?, enabled=? WHERE id=?",
            params![serde_json::to_string(&input.config)?, input.enabled, id],
        )?;
        Ok(())
    }
    pub fn delete_rule(&self, id: i64) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM rule WHERE id=?", [id])?;
        Ok(())
    }
    pub fn observation(&self, rule_id: i64, sha: &str) -> Result<Option<RuleObservation>> {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT rule_id,head_sha,state,source_identity,source_url,detail,observed_at FROM rule_observation WHERE rule_id=? AND head_sha=?",params![rule_id,sha],observation_row).optional().map_err(Into::into)
    }
    pub fn has_any_observation(&self, rule_id: i64) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM rule_observation WHERE rule_id=?)",
            [rule_id],
            |r| r.get::<_, i64>(0),
        )? != 0)
    }
    /// Saves the observation and atomically deduplicates an alert. Returns true only for a new alert.
    pub fn record(
        &self,
        obs: &RuleObservation,
        alert: Option<&crate::rules::AlertCandidate>,
        allow_alert: bool,
    ) -> Result<bool> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("INSERT INTO rule_observation(rule_id,head_sha,state,source_identity,source_url,detail,observed_at) VALUES(?,?,?,?,?,?,?) ON CONFLICT(rule_id,head_sha) DO UPDATE SET state=excluded.state,source_identity=excluded.source_identity,source_url=excluded.source_url,detail=excluded.detail,observed_at=excluded.observed_at",params![obs.rule_id,obs.head_sha,rule_state(&obs.state),obs.source_identity,obs.source_url,obs.detail,obs.observed_at])?;
        let inserted = if allow_alert {
            if let Some(a) = alert {
                tx.execute("INSERT OR IGNORE INTO alert(rule_id,head_sha,source_identity,title,body,status,created_at) VALUES(?,?,?,?,?,'unread',?)",params![obs.rule_id,obs.head_sha,a.source_identity,a.title,a.body,now()])? == 1
            } else {
                false
            }
        } else {
            false
        };
        tx.commit()?;
        Ok(inserted)
    }
    pub fn observations(&self) -> Result<Vec<RuleObservation>> {
        let c = self.conn.lock().unwrap();
        let mut s=c.prepare("SELECT rule_id,head_sha,state,source_identity,source_url,detail,observed_at FROM rule_observation ORDER BY observed_at DESC")?;
        let rows = s
            .query_map([], observation_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn alerts(&self) -> Result<Vec<Alert>> {
        let c = self.conn.lock().unwrap();
        let mut s=c.prepare("SELECT id,rule_id,head_sha,source_identity,title,body,status,created_at FROM alert ORDER BY id DESC")?;
        let rows = s
            .query_map([], alert_row)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn mark_read(&self, id: i64) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE alert SET status='read' WHERE id=?", [id])?;
        Ok(())
    }
    pub fn mark_all_read(&self) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("UPDATE alert SET status='read' WHERE status='unread'", [])?;
        Ok(())
    }
    pub fn unread_count(&self) -> Result<usize> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT COUNT(*) FROM alert WHERE status='unread'",
            [],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }
    pub fn settings(&self) -> Result<Settings> {
        let c = self.conn.lock().unwrap();
        c.query_row("SELECT value FROM setting WHERE key='settings'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .map(|s| serde_json::from_str(&s).context("bad settings JSON"))
        .transpose()
        .map(|x| x.unwrap_or_default())
    }
    pub fn save_settings(&self, s: &Settings) -> Result<()> {
        if !(30..=300).contains(&s.poll_interval_seconds) {
            anyhow::bail!("poll interval must be between 30 and 300 seconds")
        }
        self.conn.lock().unwrap().execute("INSERT INTO setting(key,value) VALUES('settings',?) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[serde_json::to_string(s)?])?;
        Ok(())
    }
    pub fn finish_poll(
        &self,
        watch_id: i64,
        head_sha: Option<&str>,
        error: Option<&str>,
    ) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE watch SET head_sha=COALESCE(?,head_sha), last_poll_at=?, last_error=? WHERE id=?",params![head_sha,now(),error,watch_id])?;
        Ok(())
    }
}
fn now() -> String {
    Utc::now().to_rfc3339()
}
fn rule_kind(k: &RuleKind) -> &'static str {
    match k {
        RuleKind::CursorBugbotCompleted => "cursor_bugbot_completed",
        RuleKind::BuildkiteJobCompleted => "buildkite_job_completed",
    }
}
fn parse_kind(s: String) -> rusqlite::Result<RuleKind> {
    match s.as_str() {
        "cursor_bugbot_completed" => Ok(RuleKind::CursorBugbotCompleted),
        "buildkite_job_completed" => Ok(RuleKind::BuildkiteJobCompleted),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("unknown rule kind")),
        )),
    }
}
fn rule_state(x: &RuleState) -> &'static str {
    match x {
        RuleState::Waiting => "waiting",
        RuleState::InProgress => "in_progress",
        RuleState::Completed => "completed",
        RuleState::Failed => "failed",
        RuleState::Unavailable => "unavailable",
    }
}
fn parse_state(s: String) -> rusqlite::Result<RuleState> {
    match s.as_str() {
        "waiting" => Ok(RuleState::Waiting),
        "in_progress" => Ok(RuleState::InProgress),
        "completed" => Ok(RuleState::Completed),
        "failed" => Ok(RuleState::Failed),
        "unavailable" => Ok(RuleState::Unavailable),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other("unknown state")),
        )),
    }
}
fn watch_row(r: &rusqlite::Row) -> rusqlite::Result<Watch> {
    Ok(Watch {
        id: r.get(0)?,
        github_owner: r.get(1)?,
        github_repo: r.get(2)?,
        github_pr_number: r.get(3)?,
        title: r.get(4)?,
        active: r.get(5)?,
        created_at: r.get(6)?,
        head_sha: r.get(7)?,
        last_poll_at: r.get(8)?,
        last_error: r.get(9)?,
    })
}
fn rule_row(r: &rusqlite::Row) -> rusqlite::Result<Rule> {
    Ok(Rule {
        id: r.get(0)?,
        watch_id: r.get(1)?,
        kind: parse_kind(r.get(2)?)?,
        config_json: r.get(3)?,
        enabled: r.get(4)?,
        created_at: r.get(5)?,
    })
}
fn observation_row(r: &rusqlite::Row) -> rusqlite::Result<RuleObservation> {
    Ok(RuleObservation {
        rule_id: r.get(0)?,
        head_sha: r.get(1)?,
        state: parse_state(r.get(2)?)?,
        source_identity: r.get(3)?,
        source_url: r.get(4)?,
        detail: r.get(5)?,
        observed_at: r.get(6)?,
    })
}
fn alert_row(r: &rusqlite::Row) -> rusqlite::Result<Alert> {
    let status: String = r.get(6)?;
    Ok(Alert {
        id: r.get(0)?,
        rule_id: r.get(1)?,
        head_sha: r.get(2)?,
        source_identity: r.get(3)?,
        title: r.get(4)?,
        body: r.get(5)?,
        status: if status == "read" {
            AlertStatus::Read
        } else {
            AlertStatus::Unread
        },
        created_at: r.get(7)?,
    })
}
