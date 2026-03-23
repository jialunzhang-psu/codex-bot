use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::codex::RuntimeSettings;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionRecord {
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub pending_name: Option<String>,
    #[serde(default)]
    pub quiet: bool,
    #[serde(default)]
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedState {
    #[serde(default)]
    sessions: HashMap<String, SessionRecord>,
    #[serde(default)]
    names_by_thread_id: HashMap<String, String>,
    #[serde(default)]
    runtime: RuntimeSettings,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            names_by_thread_id: HashMap::new(),
            runtime: RuntimeSettings::default(),
        }
    }
}

pub struct StateStore {
    path: PathBuf,
    inner: Mutex<PersistedState>,
}

impl StateStore {
    pub fn load(path: PathBuf, default_runtime: RuntimeSettings) -> Result<Self> {
        let state = if path.exists() {
            let raw = fs::read_to_string(&path)
                .with_context(|| format!("failed to read state file {}", path.display()))?;
            let mut persisted: PersistedState = serde_json::from_str(&raw)
                .with_context(|| format!("invalid JSON in {}", path.display()))?;
            persisted.runtime = persisted.runtime.merged_with(&default_runtime);
            persisted
        } else {
            let mut persisted = PersistedState::default();
            persisted.runtime = default_runtime;
            persisted
        };

        Ok(Self {
            path,
            inner: Mutex::new(state),
        })
    }

    pub fn runtime_settings(&self) -> RuntimeSettings {
        self.inner.lock().runtime.clone()
    }

    pub fn set_runtime_settings(&self, runtime: RuntimeSettings) -> Result<()> {
        let mut state = self.inner.lock();
        state.runtime = runtime;
        self.save_locked(&state)
    }

    pub fn session(&self, session_key: &str) -> SessionRecord {
        self.inner
            .lock()
            .sessions
            .get(session_key)
            .cloned()
            .unwrap_or_default()
    }

    pub fn reset_session(
        &self,
        session_key: &str,
        pending_name: Option<String>,
    ) -> Result<SessionRecord> {
        let mut state = self.inner.lock();
        let record = state.sessions.entry(session_key.to_string()).or_default();
        record.generation = record.generation.saturating_add(1);
        record.thread_id = None;
        record.pending_name = pending_name.filter(|value| !value.trim().is_empty());
        let snapshot = record.clone();
        self.save_locked(&state)?;
        Ok(snapshot)
    }

    pub fn switch_session(&self, session_key: &str, thread_id: String) -> Result<SessionRecord> {
        let mut state = self.inner.lock();
        let record = state.sessions.entry(session_key.to_string()).or_default();
        record.generation = record.generation.saturating_add(1);
        record.thread_id = Some(thread_id);
        record.pending_name = None;
        let snapshot = record.clone();
        self.save_locked(&state)?;
        Ok(snapshot)
    }

    pub fn assign_thread_if_generation(
        &self,
        session_key: &str,
        expected_generation: u64,
        thread_id: &str,
    ) -> Result<bool> {
        let mut state = self.inner.lock();
        let Some(record) = state.sessions.get_mut(session_key) else {
            return Ok(false);
        };

        if record.generation != expected_generation {
            return Ok(false);
        }

        record.thread_id = Some(thread_id.to_string());
        if let Some(name) = record.pending_name.take() {
            state.names_by_thread_id.insert(thread_id.to_string(), name);
        }
        self.save_locked(&state)?;
        Ok(true)
    }

    pub fn set_quiet(&self, session_key: &str, quiet: bool) -> Result<SessionRecord> {
        let mut state = self.inner.lock();
        let record = state.sessions.entry(session_key.to_string()).or_default();
        record.quiet = quiet;
        let snapshot = record.clone();
        self.save_locked(&state)?;
        Ok(snapshot)
    }

    pub fn remove_thread_everywhere(&self, thread_id: &str) -> Result<()> {
        let mut state = self.inner.lock();
        state.names_by_thread_id.remove(thread_id);
        for record in state.sessions.values_mut() {
            if record.thread_id.as_deref() == Some(thread_id) {
                record.thread_id = None;
                record.pending_name = None;
                record.generation = record.generation.saturating_add(1);
            }
        }
        self.save_locked(&state)
    }

    pub fn all_thread_names(&self) -> HashMap<String, String> {
        self.inner.lock().names_by_thread_id.clone()
    }

    pub fn ensure_parent_dir(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        Ok(())
    }

    fn save_locked(&self, state: &PersistedState) -> Result<()> {
        self.ensure_parent_dir()?;

        let raw = serde_json::to_vec_pretty(state).context("failed to serialize state")?;
        let tmp_path = temp_path(&self.path);
        fs::write(&tmp_path, raw)
            .with_context(|| format!("failed to write {}", tmp_path.display()))?;
        fs::rename(&tmp_path, &self.path)
            .with_context(|| format!("failed to replace {}", self.path.display()))?;
        Ok(())
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let mut tmp = path.to_path_buf();
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| format!("{name}.tmp"))
        .unwrap_or_else(|| "state.tmp".to_string());
    tmp.set_file_name(file_name);
    tmp
}
