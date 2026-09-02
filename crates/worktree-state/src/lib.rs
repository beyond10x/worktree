//! SQLite ownership registry and XDG workspace policy configuration.

use b10x_worktree::RegistryPort;
use b10x_worktree_domain::{
    Lifecycle, OperationEvidence, RecoveryProof, Refusal, RelocationIntent, RemovalIntent,
    SURFACE_VERSION, WorkspacePolicy, WorktreeId, WorktreeRecord,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const REGISTRY_SCHEMA_VERSION: i64 = 3;

/// Persisted collection of activated profiles.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Configuration schema version.
    pub version: u32,
    /// Activated workspace profiles.
    pub profiles: Vec<WorkspacePolicy>,
}

/// Portable profile template committed by an organization.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileTemplate {
    /// Template schema version.
    pub version: u32,
    /// Profile name.
    pub name: String,
    /// Optional explicit absolute managed-tree root.
    #[serde(default)]
    pub worktree_root: Option<PathBuf>,
    /// Abandonment threshold.
    pub expire_after_seconds: i64,
    /// Whether agents should treat the primary collection as protected.
    #[serde(default = "default_true")]
    pub protect_workspace_root: bool,
}

fn default_true() -> bool {
    true
}

impl ProfileTemplate {
    /// Resolve a portable profile for one absolute workspace.
    pub fn resolve(
        self,
        workspace_root: &Path,
        state_home: &Path,
    ) -> Result<WorkspacePolicy, Refusal> {
        let workspace_root = std::fs::canonicalize(workspace_root).map_err(|error| {
            Refusal::new(
                "workspace-root-invalid",
                format!("{}: {error}", workspace_root.display()),
            )
        })?;
        let requested_worktree_root = self
            .worktree_root
            .unwrap_or_else(|| state_home.join("worktree").join("trees").join(&self.name));
        let worktree_root = canonicalize_future_path(&requested_worktree_root)?;
        let policy = WorkspacePolicy {
            version: self.version,
            name: self.name.clone(),
            workspace_root,
            worktree_root,
            expire_after_seconds: self.expire_after_seconds,
            protect_workspace_root: self.protect_workspace_root,
        };
        policy.validate()?;
        Ok(policy)
    }
}

/// Resolve the XDG configuration directory.
pub fn config_home() -> Result<PathBuf, Refusal> {
    if let Some(value) = std::env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(value));
    }
    home_dir().map(|path| path.join(".config"))
}

/// Resolve the XDG state directory.
pub fn state_home() -> Result<PathBuf, Refusal> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(value));
    }
    home_dir().map(|path| path.join(".local").join("state"))
}

fn home_dir() -> Result<PathBuf, Refusal> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| Refusal::new("home-unavailable", "HOME is not set"))
}

/// Default configuration file path.
pub fn config_path() -> Result<PathBuf, Refusal> {
    Ok(config_home()?.join("worktree").join("config.toml"))
}

/// Default SQLite registry path.
pub fn registry_path() -> Result<PathBuf, Refusal> {
    Ok(state_home()?.join("worktree").join("registry.sqlite3"))
}

/// Load configuration, returning an empty v1 configuration when absent.
pub fn load_config(path: &Path) -> Result<Config, Refusal> {
    if !path.exists() {
        return Ok(Config {
            version: SURFACE_VERSION,
            profiles: Vec::new(),
        });
    }
    let source = std::fs::read_to_string(path)
        .map_err(|error| Refusal::new("config-read-failed", error.to_string()))?;
    let config: Config = toml::from_str(&source)
        .map_err(|error| Refusal::new("config-invalid", error.to_string()))?;
    validate_config(&config)?;
    Ok(config)
}

fn validate_config(config: &Config) -> Result<(), Refusal> {
    if config.version != SURFACE_VERSION {
        return Err(Refusal::new(
            "unsupported-config-version",
            format!("configuration version {} is not supported", config.version),
        ));
    }
    for policy in &config.profiles {
        policy.validate()?;
        let workspace_root = std::fs::canonicalize(&policy.workspace_root).map_err(|error| {
            Refusal::new(
                "workspace-root-invalid",
                format!("{}: {error}", policy.workspace_root.display()),
            )
        })?;
        let worktree_root = canonicalize_future_path(&policy.worktree_root)?;
        if workspace_root != policy.workspace_root || worktree_root != policy.worktree_root {
            return Err(Refusal::new(
                "non-canonical-policy-path",
                format!("profile {} contains a non-canonical root", policy.name),
            ));
        }
    }
    Ok(())
}

/// Atomically persist configuration.
pub fn save_config(path: &Path, config: &Config) -> Result<(), Refusal> {
    validate_config(config)?;
    let parent = path
        .parent()
        .ok_or_else(|| Refusal::new("config-path-invalid", path.display().to_string()))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| Refusal::new("config-directory-failed", error.to_string()))?;
    let source = toml::to_string_pretty(config)
        .map_err(|error| Refusal::new("config-encode-failed", error.to_string()))?;
    let temporary = path.with_extension("toml.tmp");
    std::fs::write(&temporary, source)
        .map_err(|error| Refusal::new("config-write-failed", error.to_string()))?;
    std::fs::rename(&temporary, path)
        .map_err(|error| Refusal::new("config-replace-failed", error.to_string()))
}

/// Add or replace an activated profile by name.
pub fn upsert_profile(config: &mut Config, policy: WorkspacePolicy) {
    config.profiles.retain(|item| item.name != policy.name);
    config.profiles.push(policy);
    config
        .profiles
        .sort_by(|left, right| left.name.cmp(&right.name));
}

/// Select the most-specific workspace containing the repository.
pub fn resolve_policy<'a>(
    config: &'a Config,
    repository: &Path,
) -> Result<&'a WorkspacePolicy, Refusal> {
    let repository = std::fs::canonicalize(repository).map_err(|error| {
        Refusal::new(
            "repository-not-found",
            format!("{}: {error}", repository.display()),
        )
    })?;
    config
        .profiles
        .iter()
        .filter(|policy| repository.starts_with(&policy.workspace_root))
        .max_by_key(|policy| policy.workspace_root.components().count())
        .ok_or_else(|| {
            Refusal::new(
                "workspace-not-activated",
                format!("no active profile contains {}", repository.display()),
            )
        })
}

/// SQLite registry. A mutex serializes one process; SQLite serializes processes.
pub struct SqliteRegistry {
    connection: Mutex<Connection>,
}

impl SqliteRegistry {
    /// Open and migrate a registry.
    pub fn open(path: &Path) -> Result<Self, Refusal> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Refusal::new("state-directory-failed", error.to_string()))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| Refusal::new("registry-open-failed", error.to_string()))?;
        let existing_version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(database_error)?;
        if existing_version > REGISTRY_SCHEMA_VERSION {
            return Err(Refusal::new(
                "unsupported-registry-version",
                format!(
                    "registry version {existing_version} is newer than supported version {REGISTRY_SCHEMA_VERSION}"
                ),
            ));
        }
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA foreign_keys=ON;
                 CREATE TABLE IF NOT EXISTS worktrees (
                   id TEXT PRIMARY KEY,
                   repository_root TEXT NOT NULL,
                   path TEXT NOT NULL UNIQUE,
                   purpose TEXT NOT NULL,
                   owner TEXT NOT NULL,
                   lifecycle TEXT NOT NULL,
                   created_at INTEGER NOT NULL,
                   last_seen_at INTEGER NOT NULL,
                   finished_at INTEGER,
                   head TEXT
                 );
                 CREATE TABLE IF NOT EXISTS leases (
                   worktree_id TEXT NOT NULL REFERENCES worktrees(id) ON DELETE CASCADE,
                   session TEXT NOT NULL,
                   last_seen_at INTEGER NOT NULL,
                   PRIMARY KEY(worktree_id, session)
                 );
                 CREATE TABLE IF NOT EXISTS events (
                   sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                   worktree_id TEXT NOT NULL,
                   operation TEXT NOT NULL,
                   recorded_at INTEGER NOT NULL,
                   payload TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS relocations (
                   worktree_id TEXT PRIMARY KEY REFERENCES worktrees(id) ON DELETE CASCADE,
                   from_path TEXT NOT NULL,
                   to_path TEXT NOT NULL UNIQUE,
                   head TEXT NOT NULL,
                   planned_at INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS removal_intents (
                   worktree_id TEXT PRIMARY KEY REFERENCES worktrees(id) ON DELETE CASCADE,
                   path TEXT NOT NULL,
                   head TEXT NOT NULL,
                   recovery_json TEXT NOT NULL,
                   operation TEXT NOT NULL,
                   planned_at INTEGER NOT NULL
                 );
                 PRAGMA user_version=3;",
            )
            .map_err(|error| Refusal::new("registry-migration-failed", error.to_string()))?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn connection(&self) -> Result<std::sync::MutexGuard<'_, Connection>, Refusal> {
        self.connection
            .lock()
            .map_err(|_| Refusal::new("registry-lock-poisoned", "registry lock was poisoned"))
    }
}

impl RegistryPort for SqliteRegistry {
    fn reserve(&self, record: &WorktreeRecord) -> Result<(), Refusal> {
        let connection = self.connection()?;
        insert_record(&connection, record)
    }

    fn activate(&self, id: &str, head: &str, now: i64) -> Result<(), Refusal> {
        changed(
            self.connection()?.execute(
                "UPDATE worktrees SET lifecycle='active', head=?2, last_seen_at=?3
                 WHERE id=?1 AND lifecycle IN ('provisioning','failed')",
                params![id, head, now],
            ),
            "activate",
        )
    }

    fn fail(&self, id: &str, now: i64) -> Result<(), Refusal> {
        changed(
            self.connection()?.execute(
                "UPDATE worktrees SET lifecycle='failed', last_seen_at=?2 WHERE id=?1 AND lifecycle='provisioning'",
                params![id, now],
            ),
            "fail",
        )
    }

    fn find_by_path(&self, path: &Path) -> Result<Option<WorktreeRecord>, Refusal> {
        let path = path_text(path)?;
        self.connection()?
            .query_row(
                "SELECT id,repository_root,path,purpose,owner,lifecycle,created_at,last_seen_at,finished_at,head FROM worktrees WHERE path=?1",
                [path],
                row_record,
            )
            .optional()
            .map_err(database_error)
    }

    fn list(&self) -> Result<Vec<WorktreeRecord>, Refusal> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT id,repository_root,path,purpose,owner,lifecycle,created_at,last_seen_at,finished_at,head FROM worktrees ORDER BY created_at,id")
            .map_err(database_error)?;
        let rows = statement
            .query_map([], row_record)
            .map_err(database_error)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(database_error)
    }

    fn mark_seen(&self, id: &str, head: Option<&str>, now: i64) -> Result<(), Refusal> {
        changed(
            self.connection()?.execute(
                "UPDATE worktrees SET last_seen_at=?2, head=COALESCE(?3,head) WHERE id=?1 AND lifecycle='active'",
                params![id, now, head],
            ),
            "mark-seen",
        )
    }

    fn mark_finished(
        &self,
        id: &str,
        head: &str,
        now: i64,
        lease_timeout: i64,
    ) -> Result<(), Refusal> {
        let lease_cutoff = now.saturating_sub(lease_timeout);
        changed(
            self.connection()?.execute(
                "UPDATE worktrees
                 SET lifecycle='finished', head=?2, finished_at=?3, last_seen_at=?3
                 WHERE id=?1 AND lifecycle='active'
                   AND NOT EXISTS (
                     SELECT 1 FROM leases
                     WHERE worktree_id=?1 AND last_seen_at>=?4
                   )",
                params![id, head, now, lease_cutoff],
            ),
            "finish",
        )
    }

    fn claim_expired(
        &self,
        id: &str,
        head: &str,
        now: i64,
        expire_before: i64,
        lease_timeout: i64,
    ) -> Result<(), Refusal> {
        let lease_cutoff = now.saturating_sub(lease_timeout);
        changed(
            self.connection()?.execute(
                "UPDATE worktrees
                 SET lifecycle='finished', head=?2, finished_at=?3, last_seen_at=?3
                 WHERE id=?1 AND lifecycle='active' AND last_seen_at<=?4
                   AND NOT EXISTS (
                     SELECT 1 FROM leases
                     WHERE worktree_id=?1 AND last_seen_at>=?5
                   )",
                params![id, head, now, expire_before, lease_cutoff],
            ),
            "claim-expired",
        )
    }

    fn claim_relocation(&self, id: &str, now: i64, lease_timeout: i64) -> Result<(), Refusal> {
        let lease_cutoff = now.saturating_sub(lease_timeout);
        changed(
            self.connection()?.execute(
                "UPDATE worktrees SET lifecycle='relocating',last_seen_at=?2
                 WHERE id=?1 AND lifecycle='active'
                   AND NOT EXISTS (
                     SELECT 1 FROM leases
                     WHERE worktree_id=?1 AND last_seen_at>=?3
                   )",
                params![id, now, lease_cutoff],
            ),
            "claim-relocation",
        )
    }

    fn mark_removed(&self, evidence: &OperationEvidence) -> Result<(), Refusal> {
        let connection = self.connection()?;
        let payload = format!(
            "head={} refs={}",
            evidence.head.as_deref().unwrap_or(""),
            evidence
                .recovery
                .as_ref()
                .map_or_else(String::new, |proof| proof.refs.join(","))
        );
        let transaction = connection.unchecked_transaction().map_err(database_error)?;
        changed(
            transaction.execute(
                "UPDATE worktrees SET lifecycle='removed', last_seen_at=?2 WHERE id=?1",
                params![evidence.id.as_str(), evidence.recorded_at],
            ),
            "remove",
        )?;
        transaction
            .execute(
                "INSERT INTO events(worktree_id,operation,recorded_at,payload) VALUES(?1,?2,?3,?4)",
                params![
                    evidence.id.as_str(),
                    evidence.operation,
                    evidence.recorded_at,
                    payload
                ],
            )
            .map_err(database_error)?;
        transaction.commit().map_err(database_error)
    }

    fn live_lease_count(&self, id: &str, now: i64, timeout: i64) -> Result<u64, Refusal> {
        let cutoff = now.saturating_sub(timeout);
        let count: i64 = self
            .connection()?
            .query_row(
                "SELECT COUNT(*) FROM leases WHERE worktree_id=?1 AND last_seen_at>=?2",
                params![id, cutoff],
                |row| row.get(0),
            )
            .map_err(database_error)?;
        u64::try_from(count)
            .map_err(|error| Refusal::new("registry-count-invalid", error.to_string()))
    }

    fn acquire_lease(&self, id: &str, session: &str, now: i64) -> Result<(), Refusal> {
        changed(
            self.connection()?.execute(
                "INSERT INTO leases(worktree_id,session,last_seen_at)
                 SELECT ?1,?2,?3 FROM worktrees
                 WHERE id=?1 AND lifecycle='active'
                 ON CONFLICT(worktree_id,session)
                 DO UPDATE SET last_seen_at=excluded.last_seen_at",
                params![id, session, now],
            ),
            "acquire-lease",
        )
    }

    fn release_lease(&self, id: &str, session: &str) -> Result<(), Refusal> {
        self.connection()?
            .execute(
                "DELETE FROM leases WHERE worktree_id=?1 AND session=?2",
                params![id, session],
            )
            .map(|_| ())
            .map_err(database_error)
    }

    fn adopt(&self, record: &WorktreeRecord) -> Result<(), Refusal> {
        let connection = self.connection()?;
        insert_record(&connection, record)
    }

    fn relocation(&self, id: &str) -> Result<Option<RelocationIntent>, Refusal> {
        self.connection()?
            .query_row(
                "SELECT worktree_id,from_path,to_path,head,planned_at FROM relocations WHERE worktree_id=?1",
                [id],
                |row| {
                    let id: String = row.get(0)?;
                    Ok(RelocationIntent {
                        id: WorktreeId::new(id).map_err(sql_conversion_error)?,
                        from: PathBuf::from(row.get::<_, String>(1)?),
                        to: PathBuf::from(row.get::<_, String>(2)?),
                        head: row.get(3)?,
                        planned_at: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(database_error)
    }

    fn begin_relocation(&self, intent: &RelocationIntent) -> Result<(), Refusal> {
        if let Some(existing) = self.relocation(intent.id.as_str())? {
            return if existing == *intent {
                Ok(())
            } else {
                Err(Refusal::new(
                    "relocation-already-planned",
                    "a different relocation intent already exists",
                ))
            };
        }
        self.connection()?
            .execute(
                "INSERT INTO relocations(worktree_id,from_path,to_path,head,planned_at) VALUES(?1,?2,?3,?4,?5)",
                params![
                    intent.id.as_str(),
                    path_text(&intent.from)?,
                    path_text(&intent.to)?,
                    intent.head,
                    intent.planned_at,
                ],
            )
            .map(|_| ())
            .map_err(database_error)
    }

    fn complete_relocation(
        &self,
        intent: &RelocationIntent,
        evidence: &OperationEvidence,
    ) -> Result<(), Refusal> {
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction().map_err(database_error)?;
        changed(
            transaction.execute(
                "UPDATE worktrees
                 SET path=?2,head=?3,last_seen_at=?4,lifecycle='active'
                 WHERE id=?1 AND path=?5 AND lifecycle='relocating'",
                params![
                    intent.id.as_str(),
                    path_text(&intent.to)?,
                    intent.head,
                    evidence.recorded_at,
                    path_text(&intent.from)?,
                ],
            ),
            "relocate",
        )?;
        transaction
            .execute(
                "INSERT INTO events(worktree_id,operation,recorded_at,payload) VALUES(?1,?2,?3,?4)",
                params![
                    intent.id.as_str(),
                    evidence.operation,
                    evidence.recorded_at,
                    format!(
                        "from={} to={} head={}",
                        intent.from.display(),
                        intent.to.display(),
                        intent.head
                    )
                ],
            )
            .map_err(database_error)?;
        changed(
            transaction.execute(
                "DELETE FROM relocations WHERE worktree_id=?1 AND from_path=?2 AND to_path=?3 AND head=?4",
                params![
                    intent.id.as_str(),
                    path_text(&intent.from)?,
                    path_text(&intent.to)?,
                    intent.head,
                ],
            ),
            "complete-relocation",
        )?;
        transaction.commit().map_err(database_error)
    }

    fn removal(&self, id: &str) -> Result<Option<RemovalIntent>, Refusal> {
        self.connection()?
            .query_row(
                "SELECT worktree_id,path,head,recovery_json,operation,planned_at
                 FROM removal_intents WHERE worktree_id=?1",
                [id],
                |row| {
                    let id: String = row.get(0)?;
                    let recovery_json: String = row.get(3)?;
                    let recovery =
                        serde_json::from_str::<RecoveryProof>(&recovery_json).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?;
                    Ok(RemovalIntent {
                        id: WorktreeId::new(id).map_err(sql_conversion_error)?,
                        path: PathBuf::from(row.get::<_, String>(1)?),
                        head: row.get(2)?,
                        recovery,
                        operation: row.get(4)?,
                        planned_at: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(database_error)
    }

    fn begin_removal(&self, intent: &RemovalIntent) -> Result<(), Refusal> {
        let recovery_json = serde_json::to_string(&intent.recovery)
            .map_err(|error| Refusal::new("recovery-proof-encode-failed", error.to_string()))?;
        let path = path_text(&intent.path)?;
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction().map_err(database_error)?;
        let existing = transaction
            .query_row(
                "SELECT path,operation FROM removal_intents WHERE worktree_id=?1",
                [intent.id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(database_error)?;
        if let Some((existing_path, existing_operation)) = existing {
            if existing_path != path || existing_operation != intent.operation {
                return Err(Refusal::new(
                    "removal-already-planned",
                    "a different removal intent already exists",
                ));
            }
            changed(
                transaction.execute(
                    "UPDATE removal_intents SET head=?2,recovery_json=?3,planned_at=?4
                     WHERE worktree_id=?1 AND path=?5 AND operation=?6",
                    params![
                        intent.id.as_str(),
                        intent.head,
                        recovery_json,
                        intent.planned_at,
                        path,
                        intent.operation,
                    ],
                ),
                "refresh-removal-intent",
            )?;
        } else {
            transaction
                .execute(
                    "INSERT INTO removal_intents(worktree_id,path,head,recovery_json,operation,planned_at)
                     VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        intent.id.as_str(),
                        path,
                        intent.head,
                        recovery_json,
                        intent.operation,
                        intent.planned_at,
                    ],
                )
                .map_err(database_error)?;
        }
        let relocation = transaction
            .query_row(
                "SELECT from_path,head FROM relocations WHERE worktree_id=?1",
                [intent.id.as_str()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(database_error)?;
        if let Some((relocation_source, relocation_head)) = relocation {
            if intent.operation != "retire-external"
                || relocation_source != path
                || relocation_head != intent.head
            {
                return Err(Refusal::new(
                    "removal-relocation-mismatch",
                    "pending relocation can only be superseded by retirement of its exact source and HEAD",
                ));
            }
        }
        transaction.commit().map_err(database_error)
    }

    fn complete_removal(
        &self,
        intent: &RemovalIntent,
        evidence: &OperationEvidence,
    ) -> Result<(), Refusal> {
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction().map_err(database_error)?;
        let relocation = transaction
            .query_row(
                "SELECT from_path,to_path,head FROM relocations WHERE worktree_id=?1",
                [intent.id.as_str()],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(database_error)?;
        if let Some((relocation_source, _, relocation_head)) = relocation.as_ref() {
            if intent.operation != "retire-external"
                || relocation_source != path_text(&intent.path)?
                || relocation_head != &intent.head
            {
                return Err(Refusal::new(
                    "removal-relocation-mismatch",
                    "pending relocation can only be completed by retirement of its exact source and HEAD",
                ));
            }
        }
        changed(
            transaction.execute(
                "UPDATE worktrees SET lifecycle='removed',head=?2,last_seen_at=?3
                 WHERE id=?1 AND path=?4",
                params![
                    intent.id.as_str(),
                    intent.head,
                    evidence.recorded_at,
                    path_text(&intent.path)?,
                ],
            ),
            "complete-removal",
        )?;
        transaction
            .execute(
                "INSERT INTO events(worktree_id,operation,recorded_at,payload) VALUES(?1,?2,?3,?4)",
                params![
                    intent.id.as_str(),
                    evidence.operation,
                    evidence.recorded_at,
                    format!(
                        "head={} refs={}",
                        intent.head,
                        intent.recovery.refs.join(",")
                    )
                ],
            )
            .map_err(database_error)?;
        changed(
            transaction.execute(
                "DELETE FROM removal_intents
                 WHERE worktree_id=?1 AND path=?2 AND head=?3 AND operation=?4",
                params![
                    intent.id.as_str(),
                    path_text(&intent.path)?,
                    intent.head,
                    intent.operation,
                ],
            ),
            "clear-removal-intent",
        )?;
        if let Some((relocation_source, relocation_destination, relocation_head)) = relocation {
            changed(
                transaction.execute(
                    "DELETE FROM relocations
                     WHERE worktree_id=?1 AND from_path=?2 AND to_path=?3 AND head=?4",
                    params![
                        intent.id.as_str(),
                        relocation_source,
                        relocation_destination,
                        relocation_head,
                    ],
                ),
                "complete-retirement-relocation",
            )?;
        }
        transaction.commit().map_err(database_error)
    }
}

fn insert_record(connection: &Connection, record: &WorktreeRecord) -> Result<(), Refusal> {
    connection
        .execute(
            "INSERT INTO worktrees(id,repository_root,path,purpose,owner,lifecycle,created_at,last_seen_at,finished_at,head)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                record.id.as_str(),
                path_text(&record.repository_root)?,
                path_text(&record.path)?,
                record.purpose,
                record.owner,
                record.lifecycle.as_str(),
                record.created_at,
                record.last_seen_at,
                record.finished_at,
                record.head,
            ],
        )
        .map(|_| ())
        .map_err(|error| Refusal::new("registry-reservation-failed", error.to_string()))
}

fn row_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<WorktreeRecord> {
    let id: String = row.get(0)?;
    let lifecycle: String = row.get(5)?;
    Ok(WorktreeRecord {
        id: WorktreeId::new(id).map_err(sql_conversion_error)?,
        repository_root: PathBuf::from(row.get::<_, String>(1)?),
        path: PathBuf::from(row.get::<_, String>(2)?),
        purpose: row.get(3)?,
        owner: row.get(4)?,
        lifecycle: Lifecycle::parse(&lifecycle).map_err(sql_conversion_error)?,
        created_at: row.get(6)?,
        last_seen_at: row.get(7)?,
        finished_at: row.get(8)?,
        head: row.get(9)?,
    })
}

fn sql_conversion_error(error: Refusal) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
}

fn changed(result: rusqlite::Result<usize>, operation: &str) -> Result<(), Refusal> {
    match result.map_err(database_error)? {
        1 => Ok(()),
        _ => Err(Refusal::new(
            "invalid-lifecycle-transition",
            format!("{operation} did not match one record in the expected state"),
        )),
    }
}

fn path_text(path: &Path) -> Result<&str, Refusal> {
    path.to_str().ok_or_else(|| {
        Refusal::new(
            "path-not-utf8",
            format!("{} cannot be stored portably", path.display()),
        )
    })
}

fn canonicalize_future_path(path: &Path) -> Result<PathBuf, Refusal> {
    if !path.is_absolute() {
        return Err(Refusal::new(
            "relative-policy-path",
            format!("{} is not absolute", path.display()),
        ));
    }
    let mut missing = Vec::new();
    let mut existing = path;
    loop {
        match std::fs::metadata(existing) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(Refusal::new(
                        "worktree-root-ancestor-not-directory",
                        format!("{} is not a directory", existing.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(existing).is_ok() {
                    return Err(Refusal::new(
                        "policy-path-dangling-symlink",
                        format!("{} is a dangling symlink", existing.display()),
                    ));
                }
                let name = existing.file_name().ok_or_else(|| {
                    Refusal::new(
                        "policy-path-invalid",
                        format!("{} has no existing ancestor", path.display()),
                    )
                })?;
                missing.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    Refusal::new(
                        "policy-path-invalid",
                        format!("{} has no existing ancestor", path.display()),
                    )
                })?;
            }
            Err(error) => {
                return Err(Refusal::new(
                    "policy-path-invalid",
                    format!("{}: {error}", existing.display()),
                ));
            }
        }
    }
    let mut canonical = std::fs::canonicalize(existing).map_err(|error| {
        Refusal::new(
            "policy-path-invalid",
            format!("{}: {error}", existing.display()),
        )
    })?;
    for component in missing.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
}

#[allow(clippy::needless_pass_by_value)]
fn database_error(error: rusqlite::Error) -> Refusal {
    Refusal::new("registry-database-error", error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn profile_resolution_keeps_worktrees_outside_workspace() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let state = temporary.path().join("state");
        std::fs::create_dir(&workspace).unwrap();
        let profile = ProfileTemplate {
            version: 1,
            name: "test".into(),
            worktree_root: None,
            expire_after_seconds: 60,
            protect_workspace_root: true,
        };
        let resolved = profile.resolve(&workspace, &state).unwrap();
        assert!(resolved.worktree_root.starts_with(state));
    }

    #[test]
    fn profile_name_cannot_replace_the_managed_root() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let refusal = ProfileTemplate {
            version: 1,
            name: "/".into(),
            worktree_root: None,
            expire_after_seconds: 60,
            protect_workspace_root: true,
        }
        .resolve(&workspace, &temporary.path().join("state"))
        .unwrap_err();
        assert_eq!(refusal.code, "invalid-policy-name");
    }

    #[cfg(unix)]
    #[test]
    fn profile_resolution_canonicalizes_symlinked_roots() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let real_workspace = temporary.path().join("real-workspace");
        let linked_workspace = temporary.path().join("linked-workspace");
        let real_state = temporary.path().join("real-state");
        let linked_state = temporary.path().join("linked-state");
        std::fs::create_dir(&real_workspace).unwrap();
        std::fs::create_dir(&real_state).unwrap();
        symlink(&real_workspace, &linked_workspace).unwrap();
        symlink(&real_state, &linked_state).unwrap();

        let resolved = ProfileTemplate {
            version: 1,
            name: "test".into(),
            worktree_root: None,
            expire_after_seconds: 60,
            protect_workspace_root: true,
        }
        .resolve(&linked_workspace, &linked_state)
        .unwrap();

        assert_eq!(resolved.workspace_root, real_workspace);
        assert_eq!(
            resolved.worktree_root,
            real_state.join("worktree/trees/test")
        );
    }

    #[test]
    fn registry_round_trips_records() {
        let temporary = tempdir().unwrap();
        let registry = SqliteRegistry::open(&temporary.path().join("registry.sqlite3")).unwrap();
        let record = WorktreeRecord {
            id: WorktreeId::new("one").unwrap(),
            repository_root: "/repo".into(),
            path: "/trees/one".into(),
            purpose: "test".into(),
            owner: "test".into(),
            lifecycle: Lifecycle::Active,
            created_at: 1,
            last_seen_at: 1,
            finished_at: None,
            head: Some("abc".into()),
        };
        registry.adopt(&record).unwrap();
        assert_eq!(registry.list().unwrap(), vec![record]);
    }

    #[test]
    fn relocation_intent_is_durable_until_path_update_commits() {
        let temporary = tempdir().unwrap();
        let registry = SqliteRegistry::open(&temporary.path().join("registry.sqlite3")).unwrap();
        let record = WorktreeRecord {
            id: WorktreeId::new("legacy-one").unwrap(),
            repository_root: "/repo".into(),
            path: "/legacy/one".into(),
            purpose: "migration test".into(),
            owner: "test".into(),
            lifecycle: Lifecycle::Active,
            created_at: 1,
            last_seen_at: 1,
            finished_at: None,
            head: Some("abc".into()),
        };
        registry.adopt(&record).unwrap();
        registry.claim_relocation("legacy-one", 2, 60).unwrap();
        let intent = RelocationIntent {
            id: record.id.clone(),
            from: record.path.clone(),
            to: "/managed/repo/legacy-one".into(),
            head: "abc".into(),
            planned_at: 3,
        };
        registry.begin_relocation(&intent).unwrap();
        assert_eq!(
            registry.relocation("legacy-one").unwrap(),
            Some(intent.clone())
        );

        let evidence = OperationEvidence {
            operation: "migrate".into(),
            id: record.id,
            path: intent.to.clone(),
            head: Some("abc".into()),
            recovery: None,
            recorded_at: 4,
        };
        registry.complete_relocation(&intent, &evidence).unwrap();
        assert!(registry.relocation("legacy-one").unwrap().is_none());
        let migrated = registry.list().unwrap().pop().unwrap();
        assert_eq!(migrated.path, intent.to);
        assert_eq!(migrated.head.as_deref(), Some("abc"));
    }

    #[test]
    fn external_retirement_retains_stale_relocation_until_atomic_completion() {
        let temporary = tempdir().unwrap();
        let registry = SqliteRegistry::open(&temporary.path().join("registry.sqlite3")).unwrap();
        let record = WorktreeRecord {
            id: WorktreeId::new("legacy-one").unwrap(),
            repository_root: "/repo".into(),
            path: "/legacy/one".into(),
            purpose: "migration test".into(),
            owner: "test".into(),
            lifecycle: Lifecycle::Finished,
            created_at: 1,
            last_seen_at: 2,
            finished_at: Some(2),
            head: Some("abc".into()),
        };
        registry.adopt(&record).unwrap();
        let relocation = RelocationIntent {
            id: record.id.clone(),
            from: record.path.clone(),
            to: "/managed/repo/legacy-one".into(),
            head: "abc".into(),
            planned_at: 1,
        };
        registry.begin_relocation(&relocation).unwrap();
        let recovery = RecoveryProof {
            head: "abc".into(),
            refs: vec!["refs/heads/main".into()],
            observed_at: 3,
        };
        let mut removal = RemovalIntent {
            id: record.id,
            path: record.path,
            head: "abc".into(),
            recovery,
            operation: "gc".into(),
            planned_at: 3,
        };

        assert_eq!(
            registry.begin_removal(&removal).unwrap_err().code,
            "removal-relocation-mismatch"
        );
        assert_eq!(
            registry.relocation("legacy-one").unwrap(),
            Some(relocation.clone())
        );
        assert!(registry.removal("legacy-one").unwrap().is_none());

        removal.operation = "retire-external".into();
        removal.head = "different".into();
        removal.recovery.head = "different".into();
        assert_eq!(
            registry.begin_removal(&removal).unwrap_err().code,
            "removal-relocation-mismatch"
        );
        assert!(registry.relocation("legacy-one").unwrap().is_some());
        assert!(registry.removal("legacy-one").unwrap().is_none());

        removal.head = "abc".into();
        removal.recovery.head = "abc".into();
        registry.begin_removal(&removal).unwrap();
        assert_eq!(registry.relocation("legacy-one").unwrap(), Some(relocation));
        assert_eq!(
            registry.removal("legacy-one").unwrap(),
            Some(removal.clone())
        );

        let evidence = OperationEvidence {
            operation: "retire-external".into(),
            id: removal.id.clone(),
            path: removal.path.clone(),
            head: Some(removal.head.clone()),
            recovery: Some(removal.recovery.clone()),
            recorded_at: 4,
        };
        registry.complete_removal(&removal, &evidence).unwrap();
        assert!(registry.relocation("legacy-one").unwrap().is_none());
        assert!(registry.removal("legacy-one").unwrap().is_none());
        assert_eq!(
            registry.list().unwrap().pop().unwrap().lifecycle,
            Lifecycle::Removed
        );
    }

    #[test]
    fn failed_retirement_completion_rolls_back_lifecycle_event_and_both_intents() {
        let temporary = tempdir().unwrap();
        let registry = SqliteRegistry::open(&temporary.path().join("registry.sqlite3")).unwrap();
        let record = WorktreeRecord {
            id: WorktreeId::new("legacy-one").unwrap(),
            repository_root: "/repo".into(),
            path: "/legacy/one".into(),
            purpose: "migration test".into(),
            owner: "test".into(),
            lifecycle: Lifecycle::Finished,
            created_at: 1,
            last_seen_at: 2,
            finished_at: Some(2),
            head: Some("abc".into()),
        };
        registry.adopt(&record).unwrap();
        let relocation = RelocationIntent {
            id: record.id.clone(),
            from: record.path.clone(),
            to: "/managed/repo/legacy-one".into(),
            head: "abc".into(),
            planned_at: 2,
        };
        registry.begin_relocation(&relocation).unwrap();
        let removal = RemovalIntent {
            id: record.id.clone(),
            path: record.path.clone(),
            head: "abc".into(),
            recovery: RecoveryProof {
                head: "abc".into(),
                refs: vec!["refs/heads/main".into()],
                observed_at: 3,
            },
            operation: "retire-external".into(),
            planned_at: 3,
        };
        registry.begin_removal(&removal).unwrap();
        registry
            .connection()
            .unwrap()
            .execute(
                "UPDATE removal_intents SET operation='corrupt' WHERE worktree_id=?1",
                [record.id.as_str()],
            )
            .unwrap();
        let stored_removal = registry.removal(record.id.as_str()).unwrap().unwrap();
        let evidence = OperationEvidence {
            operation: "retire-external".into(),
            id: record.id.clone(),
            path: record.path.clone(),
            head: Some("abc".into()),
            recovery: Some(removal.recovery.clone()),
            recorded_at: 4,
        };

        assert_eq!(
            registry
                .complete_removal(&removal, &evidence)
                .unwrap_err()
                .code,
            "invalid-lifecycle-transition"
        );
        assert_eq!(
            registry.list().unwrap().pop().unwrap().lifecycle,
            Lifecycle::Finished
        );
        assert_eq!(
            registry.relocation(record.id.as_str()).unwrap(),
            Some(relocation)
        );
        assert_eq!(
            registry.removal(record.id.as_str()).unwrap(),
            Some(stored_removal)
        );
        let event_count: i64 = registry
            .connection()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
            .unwrap();
        assert_eq!(event_count, 0);
    }

    #[test]
    fn future_registry_versions_are_refused_without_downgrade() {
        let temporary = tempdir().unwrap();
        let path = temporary.path().join("registry.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch("PRAGMA user_version=99;").unwrap();
        drop(connection);

        let refusal = SqliteRegistry::open(&path).err().unwrap();
        assert_eq!(refusal.code, "unsupported-registry-version");
        let connection = Connection::open(path).unwrap();
        let version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 99);
    }

    #[test]
    fn leases_are_acquired_only_for_active_records() {
        let temporary = tempdir().unwrap();
        let registry = SqliteRegistry::open(&temporary.path().join("registry.sqlite3")).unwrap();
        let mut record = WorktreeRecord {
            id: WorktreeId::new("one").unwrap(),
            repository_root: "/repo".into(),
            path: "/trees/one".into(),
            purpose: "test".into(),
            owner: "test".into(),
            lifecycle: Lifecycle::Finished,
            created_at: 1,
            last_seen_at: 1,
            finished_at: Some(2),
            head: Some("abc".into()),
        };
        registry.adopt(&record).unwrap();
        assert_eq!(
            registry
                .acquire_lease("one", "session", 3)
                .unwrap_err()
                .code,
            "invalid-lifecycle-transition"
        );
        assert_eq!(registry.live_lease_count("one", 3, 60).unwrap(), 0);

        record.id = WorktreeId::new("two").unwrap();
        record.path = "/trees/two".into();
        record.lifecycle = Lifecycle::Active;
        record.finished_at = None;
        registry.adopt(&record).unwrap();
        registry.acquire_lease("two", "session", 3).unwrap();
        assert_eq!(registry.live_lease_count("two", 3, 60).unwrap(), 1);
        assert!(registry.claim_relocation("two", 3, 60).is_err());
        registry.release_lease("two", "session").unwrap();
        registry.claim_relocation("two", 4, 60).unwrap();
        assert_eq!(
            registry.acquire_lease("two", "later", 4).unwrap_err().code,
            "invalid-lifecycle-transition"
        );
        assert_eq!(registry.list().unwrap()[1].lifecycle, Lifecycle::Relocating);
    }

    #[test]
    fn removal_intent_survives_until_atomic_completion() {
        let temporary = tempdir().unwrap();
        let registry = SqliteRegistry::open(&temporary.path().join("registry.sqlite3")).unwrap();
        let record = WorktreeRecord {
            id: WorktreeId::new("one").unwrap(),
            repository_root: "/repo".into(),
            path: "/trees/one".into(),
            purpose: "test".into(),
            owner: "test".into(),
            lifecycle: Lifecycle::Finished,
            created_at: 1,
            last_seen_at: 1,
            finished_at: Some(2),
            head: Some("abc".into()),
        };
        registry.adopt(&record).unwrap();
        let proof = RecoveryProof {
            head: "abc".into(),
            refs: vec!["origin:refs/heads/main".into()],
            observed_at: 3,
        };
        let intent = RemovalIntent {
            id: record.id.clone(),
            path: record.path.clone(),
            head: "abc".into(),
            recovery: proof.clone(),
            operation: "remove".into(),
            planned_at: 3,
        };
        registry.begin_removal(&intent).unwrap();
        assert_eq!(registry.removal("one").unwrap(), Some(intent.clone()));
        let mut refreshed = intent.clone();
        refreshed.head = "def".into();
        refreshed.recovery.head = "def".into();
        refreshed.recovery.observed_at = 4;
        refreshed.planned_at = 4;
        registry.begin_removal(&refreshed).unwrap();
        assert_eq!(registry.removal("one").unwrap(), Some(refreshed.clone()));
        let evidence = OperationEvidence {
            operation: "remove".into(),
            id: record.id,
            path: record.path,
            head: Some("def".into()),
            recovery: Some(refreshed.recovery.clone()),
            recorded_at: 5,
        };
        registry.complete_removal(&refreshed, &evidence).unwrap();
        assert!(registry.removal("one").unwrap().is_none());
        let stored = registry.list().unwrap().pop().unwrap();
        assert_eq!(stored.lifecycle, Lifecycle::Removed);
        assert_eq!(stored.head.as_deref(), Some("def"));
    }

    #[test]
    fn finish_persists_head_and_is_atomic_with_live_leases() {
        let temporary = tempdir().unwrap();
        let registry = SqliteRegistry::open(&temporary.path().join("registry.sqlite3")).unwrap();
        let record = WorktreeRecord {
            id: WorktreeId::new("one").unwrap(),
            repository_root: "/repo".into(),
            path: "/trees/one".into(),
            purpose: "test".into(),
            owner: "test".into(),
            lifecycle: Lifecycle::Active,
            created_at: 1,
            last_seen_at: 1,
            finished_at: None,
            head: Some("old".into()),
        };
        registry.adopt(&record).unwrap();
        registry.acquire_lease("one", "session", 10).unwrap();
        assert!(registry.mark_finished("one", "new", 10, 60).is_err());
        let active = registry.list().unwrap().pop().unwrap();
        assert_eq!(active.lifecycle, Lifecycle::Active);
        assert_eq!(active.head.as_deref(), Some("old"));

        registry.release_lease("one", "session").unwrap();
        registry.mark_finished("one", "new", 11, 60).unwrap();
        let finished = registry.list().unwrap().pop().unwrap();
        assert_eq!(finished.lifecycle, Lifecycle::Finished);
        assert_eq!(finished.head.as_deref(), Some("new"));
        assert_eq!(finished.finished_at, Some(11));
    }
}
