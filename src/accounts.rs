use std::env;
use std::fs::{self, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

const POOL_DIR_NAME: &str = ".codex_pool";
const POOL_MANIFEST_NAME: &str = "pool.json";
const POOL_ACCOUNTS_DIR: &str = "accounts";
const CODEX_AUTH_FILE_NAME: &str = "auth.json";
const POOL_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct StoredAccount {
    pub id: String,
    pub label: Option<String>,
    pub created_ms: u128,
}

#[derive(Clone, Debug)]
pub struct PoolAccountView {
    pub id: String,
    pub label: Option<String>,
    pub created_ms: u128,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub struct PoolList {
    pub pool_dir: PathBuf,
    pub codex_auth_path: PathBuf,
    pub active_in_pool: bool,
    pub accounts: Vec<PoolAccountView>,
}

#[derive(Clone, Debug)]
pub struct PoolCatalog {
    pub pool_dir: PathBuf,
    pub accounts: Vec<StoredAccount>,
}

#[derive(Clone, Debug)]
pub struct AddAccountResult {
    pub account: StoredAccount,
    pub created_new: bool,
    pub total_accounts: usize,
}

#[derive(Clone, Debug)]
pub struct SwitchAccountResult {
    pub from: Option<StoredAccount>,
    pub to: StoredAccount,
    pub switched: bool,
}

#[derive(Clone, Debug)]
pub struct RemoveAccountResult {
    pub account: StoredAccount,
    pub removed_was_active: bool,
    pub remaining_accounts: usize,
}

#[derive(Clone, Debug)]
pub struct RemoveAllAccountsResult {
    pub removed: Vec<RemoveAccountResult>,
    pub remaining_accounts: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedPoolAccount {
    pub account: StoredAccount,
    pub auth_path: PathBuf,
    pub active: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedPool {
    pub pool_dir: PathBuf,
    pub codex_auth_path: PathBuf,
    pub active_in_pool: bool,
    pub accounts: Vec<ResolvedPoolAccount>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PoolManifest {
    version: u32,
    accounts: Vec<StoredAccountRecord>,
}

impl Default for PoolManifest {
    fn default() -> Self {
        Self {
            version: POOL_VERSION,
            accounts: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredAccountRecord {
    id: String,
    label: Option<String>,
    created_ms: u128,
    auth_file: String,
}

pub fn list_accounts(codex_home: &Path) -> Result<PoolList> {
    let pool_dir = pool_dir()?;
    let codex_auth_path = codex_auth_path(codex_home);
    list_accounts_with_paths(&pool_dir, &codex_auth_path)
}

pub fn list_pool_accounts() -> Result<PoolCatalog> {
    let pool_dir = pool_dir()?;
    let manifest = load_manifest_or_default(&manifest_path(&pool_dir))?;
    Ok(PoolCatalog {
        pool_dir,
        accounts: manifest
            .accounts
            .iter()
            .map(stored_account_from_record)
            .collect(),
    })
}

pub fn add_current_account(codex_home: &Path, label: Option<String>) -> Result<AddAccountResult> {
    let pool_dir = pool_dir()?;
    let codex_auth_path = codex_auth_path(codex_home);
    add_current_account_with_paths(&pool_dir, &codex_auth_path, label)
}

pub fn switch_account(codex_home: &Path, account_id: &str) -> Result<SwitchAccountResult> {
    let pool_dir = pool_dir()?;
    let codex_auth_path = codex_auth_path(codex_home);
    switch_account_with_paths(&pool_dir, &codex_auth_path, account_id)
}

pub fn remove_account(codex_home: &Path, account_id: &str) -> Result<RemoveAccountResult> {
    let pool_dir = pool_dir()?;
    let codex_auth_path = codex_auth_path(codex_home);
    remove_account_with_paths(&pool_dir, &codex_auth_path, account_id)
}

pub fn remove_all_accounts(codex_home: &Path) -> Result<RemoveAllAccountsResult> {
    let pool_dir = pool_dir()?;
    let codex_auth_path = codex_auth_path(codex_home);
    remove_all_accounts_with_paths(&pool_dir, &codex_auth_path)
}

pub(crate) fn resolve_accounts(codex_home: &Path) -> Result<ResolvedPool> {
    let pool_dir = pool_dir()?;
    let codex_auth_path = codex_auth_path(codex_home);
    resolve_accounts_with_paths(&pool_dir, &codex_auth_path)
}

fn list_accounts_with_paths(pool_dir: &Path, codex_auth_path: &Path) -> Result<PoolList> {
    let manifest = load_manifest_or_default(&manifest_path(pool_dir))?;
    let current_auth = fs::read(codex_auth_path).ok();
    let mut active_in_pool = false;
    let mut accounts = Vec::new();
    for entry in manifest.accounts {
        let auth_path = pool_dir.join(&entry.auth_file);
        let active = if let Some(bytes) = current_auth.as_deref() {
            fs::read(&auth_path).ok().as_deref() == Some(bytes)
        } else {
            false
        };
        active_in_pool |= active;
        accounts.push(PoolAccountView {
            id: entry.id,
            label: entry.label,
            created_ms: entry.created_ms,
            active,
        });
    }

    Ok(PoolList {
        pool_dir: pool_dir.to_path_buf(),
        codex_auth_path: codex_auth_path.to_path_buf(),
        active_in_pool,
        accounts,
    })
}

fn add_current_account_with_paths(
    pool_dir: &Path,
    codex_auth_path: &Path,
    label: Option<String>,
) -> Result<AddAccountResult> {
    let lock_path = auth_lock_path_for(codex_auth_path);
    let _process_lock = ProcessFileLock::acquire(&lock_path)?;
    let auth_bytes = fs::read(codex_auth_path).with_context(|| {
        format!(
            "failed to read current Codex identity file: {}",
            codex_auth_path.display()
        )
    })?;

    let manifest_path = manifest_path(pool_dir);
    let mut manifest = load_manifest_or_default(&manifest_path)?;
    fs::create_dir_all(pool_accounts_dir(pool_dir))
        .with_context(|| format!("failed to create {}", pool_accounts_dir(pool_dir).display()))?;

    if let Some(existing) = find_duplicate_account(pool_dir, &manifest, &auth_bytes)? {
        return Ok(AddAccountResult {
            account: existing,
            created_new: false,
            total_accounts: manifest.accounts.len(),
        });
    }

    let label = normalize_label(label);
    let created_ms = now_ms();
    let id = allocate_account_id(&manifest, label.as_deref(), created_ms);
    let relative_auth = PathBuf::from(POOL_ACCOUNTS_DIR)
        .join(&id)
        .join(CODEX_AUTH_FILE_NAME);
    let absolute_auth = pool_dir.join(&relative_auth);
    write_secure_file(&absolute_auth, &auth_bytes).with_context(|| {
        format!(
            "failed to persist identity file to account pool: {}",
            absolute_auth.display()
        )
    })?;

    manifest.accounts.push(StoredAccountRecord {
        id: id.clone(),
        label: label.clone(),
        created_ms,
        auth_file: normalize_relative_path(&relative_auth),
    });
    save_manifest(&manifest_path, &manifest)?;

    Ok(AddAccountResult {
        account: StoredAccount {
            id,
            label,
            created_ms,
        },
        created_new: true,
        total_accounts: manifest.accounts.len(),
    })
}

fn switch_account_with_paths(
    pool_dir: &Path,
    codex_auth_path: &Path,
    account_id: &str,
) -> Result<SwitchAccountResult> {
    if account_id.trim().is_empty() {
        return Err(anyhow!("account id cannot be empty"));
    }

    let lock_path = auth_lock_path_for(codex_auth_path);
    let _process_lock = ProcessFileLock::acquire(&lock_path)?;
    let manifest = load_manifest_or_default(&manifest_path(pool_dir))?;
    if manifest.accounts.is_empty() {
        return Err(anyhow!("account pool is empty"));
    }

    let Some(index) = manifest
        .accounts
        .iter()
        .position(|entry| entry.id == account_id)
    else {
        return Err(anyhow!("account not found in pool: {account_id}"));
    };
    let Some(target) = manifest.accounts.get(index) else {
        return Err(anyhow!(
            "internal error: target account index out of bounds"
        ));
    };

    let target_auth_path = pool_dir.join(&target.auth_file);
    if !target_auth_path.exists() {
        return Err(anyhow!(
            "target account auth file missing: {}",
            target_auth_path.display()
        ));
    }

    let current_auth = fs::read(codex_auth_path).ok();
    let current_active_id =
        find_manifest_account_id_by_auth_bytes(pool_dir, &manifest, current_auth.as_deref());
    let from = current_active_id.as_deref().and_then(|active_id| {
        manifest
            .accounts
            .iter()
            .find(|entry| entry.id == active_id)
            .map(stored_account_from_record)
    });
    let to = stored_account_from_record(target);
    let switched = current_active_id.as_deref() != Some(target.id.as_str());

    if switched {
        install_identity_file(&target_auth_path, codex_auth_path).with_context(|| {
            format!(
                "failed to switch identity file {} -> {}",
                target_auth_path.display(),
                codex_auth_path.display()
            )
        })?;
    }

    Ok(SwitchAccountResult { from, to, switched })
}

fn remove_account_with_paths(
    pool_dir: &Path,
    codex_auth_path: &Path,
    account_id: &str,
) -> Result<RemoveAccountResult> {
    if account_id.trim().is_empty() {
        return Err(anyhow!("account id cannot be empty"));
    }

    let lock_path = auth_lock_path_for(codex_auth_path);
    let _process_lock = ProcessFileLock::acquire(&lock_path)?;
    let manifest_path = manifest_path(pool_dir);
    let mut manifest = load_manifest_or_default(&manifest_path)?;
    if manifest.accounts.is_empty() {
        return Err(anyhow!("account pool is empty"));
    }

    let Some(index) = manifest
        .accounts
        .iter()
        .position(|entry| entry.id == account_id)
    else {
        return Err(anyhow!("account not found in pool: {account_id}"));
    };

    remove_account_at_index(
        pool_dir,
        codex_auth_path,
        &manifest_path,
        &mut manifest,
        index,
    )
}

fn remove_all_accounts_with_paths(
    pool_dir: &Path,
    codex_auth_path: &Path,
) -> Result<RemoveAllAccountsResult> {
    let lock_path = auth_lock_path_for(codex_auth_path);
    let _process_lock = ProcessFileLock::acquire(&lock_path)?;
    let manifest_path = manifest_path(pool_dir);
    let mut manifest = load_manifest_or_default(&manifest_path)?;
    let mut removed = Vec::new();
    while !manifest.accounts.is_empty() {
        removed.push(remove_account_at_index(
            pool_dir,
            codex_auth_path,
            &manifest_path,
            &mut manifest,
            0,
        )?);
    }

    Ok(RemoveAllAccountsResult {
        removed,
        remaining_accounts: manifest.accounts.len(),
    })
}

fn resolve_accounts_with_paths(pool_dir: &Path, codex_auth_path: &Path) -> Result<ResolvedPool> {
    let manifest = load_manifest_or_default(&manifest_path(pool_dir))?;
    let current_auth = fs::read(codex_auth_path).ok();
    let mut active_in_pool = false;
    let mut accounts = Vec::new();
    for entry in manifest.accounts {
        let auth_path = pool_dir.join(&entry.auth_file);
        let active = if let Some(bytes) = current_auth.as_deref() {
            fs::read(&auth_path).ok().as_deref() == Some(bytes)
        } else {
            false
        };
        active_in_pool |= active;
        accounts.push(ResolvedPoolAccount {
            account: stored_account_from_record(&entry),
            auth_path,
            active,
        });
    }

    Ok(ResolvedPool {
        pool_dir: pool_dir.to_path_buf(),
        codex_auth_path: codex_auth_path.to_path_buf(),
        active_in_pool,
        accounts,
    })
}

fn remove_account_at_index(
    pool_dir: &Path,
    codex_auth_path: &Path,
    manifest_path: &Path,
    manifest: &mut PoolManifest,
    index: usize,
) -> Result<RemoveAccountResult> {
    let removed = manifest.accounts.remove(index);
    let removed_auth_path = pool_dir.join(&removed.auth_file);
    let removed_auth = fs::read(&removed_auth_path).ok();
    let current_auth = fs::read(codex_auth_path).ok();
    let removed_was_active = match (removed_auth.as_deref(), current_auth.as_deref()) {
        (Some(removed_auth), Some(current_auth)) => removed_auth == current_auth,
        _ => false,
    };

    if removed_auth_path.exists() {
        fs::remove_file(&removed_auth_path).with_context(|| {
            format!(
                "failed to remove pooled auth file: {}",
                removed_auth_path.display()
            )
        })?;
    }
    prune_empty_parent_dir(removed_auth_path.parent());

    restore_active_auth_after_removal(pool_dir, codex_auth_path, manifest, removed_was_active)?;
    save_manifest(manifest_path, manifest)?;

    Ok(RemoveAccountResult {
        account: StoredAccount {
            id: removed.id,
            label: removed.label,
            created_ms: removed.created_ms,
        },
        removed_was_active,
        remaining_accounts: manifest.accounts.len(),
    })
}

fn restore_active_auth_after_removal(
    pool_dir: &Path,
    codex_auth_path: &Path,
    manifest: &PoolManifest,
    removed_was_active: bool,
) -> Result<()> {
    if !removed_was_active {
        return Ok(());
    }

    if let Some(next_active) = manifest.accounts.first() {
        let next_auth_path = pool_dir.join(&next_active.auth_file);
        if !next_auth_path.exists() {
            return Err(anyhow!(
                "remaining account auth file missing: {}",
                next_auth_path.display()
            ));
        }
        install_identity_file(&next_auth_path, codex_auth_path).with_context(|| {
            format!(
                "failed to promote remaining account {} as active identity",
                next_active.id
            )
        })?;
    } else if let Err(error) = fs::remove_file(codex_auth_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error).with_context(|| {
            format!(
                "failed to remove active auth file after clearing pool: {}",
                codex_auth_path.display()
            )
        });
    }

    Ok(())
}

fn stored_account_from_record(record: &StoredAccountRecord) -> StoredAccount {
    StoredAccount {
        id: record.id.clone(),
        label: record.label.clone(),
        created_ms: record.created_ms,
    }
}

fn pool_dir() -> Result<PathBuf> {
    let home = env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(POOL_DIR_NAME))
}

fn codex_auth_path(codex_home: &Path) -> PathBuf {
    codex_home.join(CODEX_AUTH_FILE_NAME)
}

fn auth_lock_path_for(codex_auth_path: &Path) -> PathBuf {
    let lock_name = codex_auth_path
        .file_name()
        .map(|name| format!("{}.lock", name.to_string_lossy()))
        .unwrap_or_else(|| "auth.json.lock".to_string());
    codex_auth_path.with_file_name(lock_name)
}

fn manifest_path(pool_dir: &Path) -> PathBuf {
    pool_dir.join(POOL_MANIFEST_NAME)
}

fn pool_accounts_dir(pool_dir: &Path) -> PathBuf {
    pool_dir.join(POOL_ACCOUNTS_DIR)
}

fn load_manifest_or_default(path: &Path) -> Result<PoolManifest> {
    match fs::read_to_string(path) {
        Ok(content) => {
            let mut manifest: PoolManifest = serde_json::from_str(&content).with_context(|| {
                format!("failed to parse account pool manifest: {}", path.display())
            })?;
            if manifest.version == 0 {
                manifest.version = POOL_VERSION;
            }
            Ok(manifest)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(PoolManifest::default()),
        Err(error) => Err(error)
            .with_context(|| format!("failed to read account pool manifest: {}", path.display())),
    }
}

fn save_manifest(path: &Path, manifest: &PoolManifest) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let content = serde_json::to_string_pretty(manifest)?;
    let tmp_path = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_ms()));
    fs::write(&tmp_path, content)?;
    fs::rename(&tmp_path, path)?;
    Ok(())
}

fn find_duplicate_account(
    pool_dir: &Path,
    manifest: &PoolManifest,
    auth_bytes: &[u8],
) -> Result<Option<StoredAccount>> {
    for entry in &manifest.accounts {
        let path = pool_dir.join(&entry.auth_file);
        let existing = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed reading pooled account identity: {}", path.display())
                });
            }
        };
        if existing == auth_bytes {
            return Ok(Some(stored_account_from_record(entry)));
        }
    }

    Ok(None)
}

fn find_manifest_account_id_by_auth_bytes(
    pool_dir: &Path,
    manifest: &PoolManifest,
    auth_bytes: Option<&[u8]>,
) -> Option<String> {
    let auth_bytes = auth_bytes?;
    for account in &manifest.accounts {
        let path = pool_dir.join(&account.auth_file);
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        if bytes == auth_bytes {
            return Some(account.id.clone());
        }
    }
    None
}

fn install_identity_file(source_auth_path: &Path, target_auth_path: &Path) -> Result<()> {
    let bytes = fs::read(source_auth_path).with_context(|| {
        format!(
            "failed to read pooled identity file: {}",
            source_auth_path.display()
        )
    })?;
    write_secure_file(target_auth_path, &bytes)
}

fn write_secure_file(path: &Path, content: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_ms()));
    fs::write(&tmp_path, content)?;
    set_secure_permissions(&tmp_path)?;
    fs::rename(&tmp_path, path)?;
    set_secure_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_secure_permissions(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_secure_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn normalize_label(label: Option<String>) -> Option<String> {
    label.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

fn allocate_account_id(manifest: &PoolManifest, label: Option<&str>, created_ms: u128) -> String {
    let base = match label {
        Some(label) => {
            let slug = slugify(label);
            if slug.is_empty() {
                created_ms.to_string()
            } else {
                format!("{created_ms}-{slug}")
            }
        }
        None => created_ms.to_string(),
    };

    if !manifest.accounts.iter().any(|item| item.id == base) {
        return base;
    }

    for suffix in 1u32..1000 {
        let candidate = format!("{base}-{suffix}");
        if !manifest.accounts.iter().any(|item| item.id == candidate) {
            return candidate;
        }
    }

    format!("{base}-{}", now_ms())
}

fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in input.chars() {
        let lowered = ch.to_ascii_lowercase();
        if lowered.is_ascii_alphanumeric() {
            out.push(lowered);
            last_dash = false;
        } else if (ch.is_whitespace() || ch == '-' || ch == '_') && !last_dash {
            out.push('-');
            last_dash = true;
        }
    }

    out.trim_matches('-').to_string()
}

fn normalize_relative_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn prune_empty_parent_dir(path: Option<&Path>) {
    let Some(path) = path else {
        return;
    };
    let Ok(mut entries) = fs::read_dir(path) else {
        return;
    };
    if entries.next().is_some() {
        return;
    }
    let _ = fs::remove_dir(path);
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[derive(Debug)]
struct ProcessFileLock {
    #[cfg(unix)]
    file: fs::File,
}

impl ProcessFileLock {
    fn acquire(path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::io::AsRawFd;

            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }

            let file = OpenOptions::new()
                .create(true)
                .read(true)
                .write(true)
                .truncate(false)
                .open(path)
                .with_context(|| format!("failed to open process lock file: {}", path.display()))?;

            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
            if rc != 0 {
                return Err(std::io::Error::last_os_error()).with_context(|| {
                    format!("failed to lock process lock file: {}", path.display())
                });
            }

            Ok(Self { file })
        }

        #[cfg(not(unix))]
        {
            let _ = path;
            Ok(Self {})
        }
    }
}

#[cfg(unix)]
impl Drop for ProcessFileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;

        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn add_current_account_deduplicates_existing_auth() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool_dir = tmp.path().join(".codex_pool");
        let codex_auth_path = tmp.path().join(".codex/auth.json");
        write_secure_file(&codex_auth_path, br#"{"id":"acct-a"}"#)?;

        let first =
            add_current_account_with_paths(&pool_dir, &codex_auth_path, Some("Work".to_string()))?;
        assert!(first.created_new);
        assert_eq!(first.total_accounts, 1);

        let second =
            add_current_account_with_paths(&pool_dir, &codex_auth_path, Some("Work".to_string()))?;
        assert!(!second.created_new);
        assert_eq!(second.total_accounts, 1);
        assert_eq!(first.account.id, second.account.id);
        Ok(())
    }

    #[test]
    fn list_accounts_marks_active_account() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool_dir = tmp.path().join(".codex_pool");
        let codex_auth_path = tmp.path().join(".codex/auth.json");
        write_secure_file(&codex_auth_path, b"auth-a")?;
        add_current_account_with_paths(&pool_dir, &codex_auth_path, Some("Work".to_string()))?;
        write_secure_file(&codex_auth_path, b"auth-b")?;
        add_current_account_with_paths(&pool_dir, &codex_auth_path, Some("Personal".to_string()))?;

        let list = list_accounts_with_paths(&pool_dir, &codex_auth_path)?;
        assert_eq!(list.accounts.len(), 2);
        assert!(list.active_in_pool);
        assert_eq!(
            list.accounts
                .iter()
                .filter(|account| account.active)
                .count(),
            1
        );
        Ok(())
    }

    #[test]
    fn switch_account_updates_active_auth_file() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool_dir = tmp.path().join(".codex_pool");
        let codex_auth_path = tmp.path().join(".codex/auth.json");
        write_secure_file(&codex_auth_path, b"auth-a")?;
        let first =
            add_current_account_with_paths(&pool_dir, &codex_auth_path, Some("Work".to_string()))?;
        write_secure_file(&codex_auth_path, b"auth-b")?;
        let second = add_current_account_with_paths(
            &pool_dir,
            &codex_auth_path,
            Some("Personal".to_string()),
        )?;

        let switched = switch_account_with_paths(&pool_dir, &codex_auth_path, &first.account.id)?;
        assert!(switched.switched);
        assert_eq!(switched.to.id, first.account.id);
        assert_eq!(
            switched.from.as_ref().map(|account| account.id.as_str()),
            Some(second.account.id.as_str())
        );
        assert_eq!(fs::read(&codex_auth_path)?, b"auth-a");
        Ok(())
    }

    #[test]
    fn remove_active_account_promotes_first_remaining_account() -> Result<()> {
        let tmp = TempDir::new()?;
        let pool_dir = tmp.path().join(".codex_pool");
        let codex_auth_path = tmp.path().join(".codex/auth.json");
        write_secure_file(&codex_auth_path, b"auth-a")?;
        let first =
            add_current_account_with_paths(&pool_dir, &codex_auth_path, Some("Work".to_string()))?;
        write_secure_file(&codex_auth_path, b"auth-b")?;
        let second = add_current_account_with_paths(
            &pool_dir,
            &codex_auth_path,
            Some("Personal".to_string()),
        )?;

        let removed = remove_account_with_paths(&pool_dir, &codex_auth_path, &second.account.id)?;
        assert!(removed.removed_was_active);
        assert_eq!(removed.remaining_accounts, 1);
        assert_eq!(fs::read(&codex_auth_path)?, b"auth-a");

        let list = list_accounts_with_paths(&pool_dir, &codex_auth_path)?;
        assert_eq!(list.accounts.len(), 1);
        assert_eq!(list.accounts[0].id, first.account.id);
        assert!(list.accounts[0].active);
        Ok(())
    }
}
