//! Cross-process file lock - `packages/shared/src/node/atomicFileLock.ts` and
//! Rust equivalent implementation of `packages/shared/src/node/privateFilePersistence.ts:withFileLock`.
//!
//! Align TS semantics one by one (TS line number is written in the corresponding position):
//! - The lock is the directory `"{filePath}.lock"`, created exclusively with mkdir (atomicFileLock.ts:181,200);
//! - Write `owner-{token}.json` (create-exclusive, flag `wx`) in the directory, content
//!   `{pid, createdAt, token}` (atomicFileLock.ts:182-203);
//! - After obtaining, review the directory identity (dev/ino) and "the only owner and his own token",
//!   Otherwise, it is regarded as a competition failure (atomicFileLock.ts:204-217);
//! - Only reclaim locks where "the owner process has exited" or "no pid and grace has been exceeded"
//!   (atomicFileLock.ts:84-86,101-111), how long the waiter has waited cannot prove the lock stale
//!   (atomicFileLock.ts:235-248: timeout directly after reaching maxWait, no stale recycling will be done);
//! - Default retry `[25,50,100,200,400]ms`, grace 100ms, upper limit 8000ms
//!   (privateFilePersistence.ts:7-9), grace will also be clamped by `maxWait/2`
//!   (atomicFileLock.ts:190-193);
//! - The same process queues up first and then competes for the OS lock to avoid dozens of callers polling the directory lock to form a panic group
//!   (privateFilePersistence.ts:56-64).

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use nc_code_service_core::ServiceError;
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

/// `DEFAULT_LOCK_RETRY_DELAYS_MS` (privateFilePersistence.ts:7).
const DEFAULT_LOCK_RETRY_DELAYS_MS: &[u64] = &[25, 50, 100, 200, 400];
/// `DEFAULT_LOCK_OWNERLESS_GRACE_MS` (privateFilePersistence.ts:8).
const DEFAULT_LOCK_OWNERLESS_GRACE_MS: u64 = 100;
/// `DEFAULT_LOCK_MAX_WAIT_MS` (privateFilePersistence.ts:9).
const DEFAULT_LOCK_MAX_WAIT_MS: u64 = 8_000;
/// `MAX_LOCK_METADATA_CLOCK_SKEW_MS` (atomicFileLock.ts:7).
const MAX_LOCK_METADATA_CLOCK_SKEW_MS: u64 = 5 * 60_000;
/// `nc_code_FILE_LOCK_TIMEOUT_ERROR_CODE` (packages/shared/src/errors.ts:30).
pub const FILE_LOCK_TIMEOUT_ERROR_CODE: &str = "ZCODE_FILE_LOCK_TIMEOUT";

/// `SharedFileLockOptions` (privateFilePersistence.ts:13-17).
#[derive(Debug, Clone)]
pub struct LockOptions {
    pub retry_delays_ms: Vec<u64>,
    pub ownerless_grace_ms: u64,
    pub max_wait_ms: u64,
}

impl Default for LockOptions {
    fn default() -> Self {
        Self {
            retry_delays_ms: DEFAULT_LOCK_RETRY_DELAYS_MS.to_vec(),
            ownerless_grace_ms: DEFAULT_LOCK_OWNERLESS_GRACE_MS,
            max_wait_ms: DEFAULT_LOCK_MAX_WAIT_MS,
        }
    }
}

/// Isomorphic with TS `withFileLock`: In-process FIFO → Cross-process directory lock → Execute → Release.
pub async fn with_file_lock<T, F, Fut>(
    file_path: &Path,
    options: LockOptions,
    operation: F,
) -> Result<T, ServiceError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, ServiceError>>,
{
    // privateFilePersistence.ts:56-64 - One in-process queue for each path, and the leader of the queue competes for the OS lock.
    let _process_slot: OwnedMutexGuard<()> = acquire_process_slot(file_path).await;
    let guard = acquire_file_lock(file_path, &options).await?;
    let result = operation().await;
    guard.release().await;
    result
}

/// In-process queue: Equivalent of `processFileLockTails` (resident of a tokio mutex per path).
async fn acquire_process_slot(file_path: &Path) -> OwnedMutexGuard<()> {
    let mutex = process_slot(file_path);
    mutex.lock_owned().await
}

fn process_slot(file_path: &Path) -> Arc<AsyncMutex<()>> {
    static SLOTS: OnceLock<Mutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut slots = slots
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    slots
        .entry(file_path.to_path_buf())
        .or_insert_with(|| Arc::new(AsyncMutex::new(())))
        .clone()
}

struct FileLockGuard {
    lock_dir: PathBuf,
    owner_file: PathBuf,
}

impl FileLockGuard {
    /// atomicFileLock.ts:218-224 - Only delete your own owner file, and then try your best to rmdir.
    async fn release(self) {
        let _ = tokio::fs::remove_file(&self.owner_file).await;
        let _ = tokio::fs::remove_dir(&self.lock_dir).await;
    }
}

async fn acquire_file_lock(
    target: &Path,
    options: &LockOptions,
) -> Result<FileLockGuard, ServiceError> {
    let lock_dir = lock_dir_for(target);
    let token = new_lock_token();
    let owner_name = format!("owner-{token}.json");
    let owner_file = lock_dir.join(&owner_name);
    let payload = format!(
        "{}\n",
        json!({ "pid": std::process::id(), "createdAt": now_millis(), "token": token })
    );
    let started_at = now_millis();
    // atomicFileLock.ts:190-193 - grace is clamped by maxWait/2 (u64 is naturally not less than 0).
    let effective_grace_ms = options.ownerless_grace_ms.min(options.max_wait_ms / 2);
    let mut observer = LockInstanceObserver::default();
    let mut last_removal_error: Option<io::Error> = None;
    let mut attempt = 0usize;

    loop {
        let mut created_lock = false;
        let acquired: Result<FileLockGuard, io::Error> = async {
            // atomicFileLock.ts:200 —— mkdir without recursive, already exists, that is, EEXIST.
            tokio::fs::create_dir(&lock_dir).await?;
            created_lock = true;
            let created_identity = lock_identity(&lock_dir).await?;
            write_owner_exclusive(&owner_file, &payload).await?;
            let current_identity = lock_identity(&lock_dir).await?;
            let owners = list_owner_files(&lock_dir).await?;
            // atomicFileLock.ts:208-217 - The directory has been changed or the owner
            // is not unique, so it is not our lock.
            if current_identity != created_identity || owners.len() != 1 || owners[0] != owner_name
            {
                return Err(already_exists_error());
            }
            Ok(FileLockGuard {
                lock_dir: lock_dir.clone(),
                owner_file: owner_file.clone(),
            })
        }
        .await;

        let error = match acquired {
            Ok(guard) => return Ok(guard),
            Err(error) => error,
        };

        if created_lock {
            let _ = tokio::fs::remove_file(&owner_file).await;
            let _ = tokio::fs::remove_dir(&lock_dir).await;
        }
        // atomicFileLock.ts:230-233 - Only the disappearance of the directory after EEXIST / lock creation is regarded as competition.
        let lost_created_lock = created_lock && error.kind() == io::ErrorKind::NotFound;
        if error.kind() != io::ErrorKind::AlreadyExists && !lost_created_lock {
            return Err(ServiceError::new(error.to_string()));
        }

        let elapsed_ms = now_millis().saturating_sub(started_at);
        if elapsed_ms >= options.max_wait_ms {
            // atomicFileLock.ts:239-248 - will time out directly after reaching the upper limit and will not be recycled to avoid accidentally deleting subsequent writers.
            if let Some(error) = last_removal_error.filter(is_permission_error) {
                return Err(ServiceError::new(error.to_string()));
            }
            return Err(file_lock_timeout(target, &lock_dir, elapsed_ms));
        }

        let removal = remove_abandoned_lock(&lock_dir, effective_grace_ms, &mut observer).await;
        if removal.removed {
            attempt += 1;
            continue;
        }
        if removal.error.is_some() {
            last_removal_error = removal.error;
        }

        let remaining_ms = options.max_wait_ms.saturating_sub(elapsed_ms);
        if options.retry_delays_ms.is_empty() || remaining_ms == 0 {
            if let Some(error) = last_removal_error.filter(is_permission_error) {
                return Err(ServiceError::new(error.to_string()));
            }
            return Err(file_lock_timeout(target, &lock_dir, elapsed_ms));
        }
        // atomicFileLock.ts:270-272 - Use attempt to get the gear and clamp it to the remaining budget.
        let index = attempt.min(options.retry_delays_ms.len() - 1);
        let delay_ms = options.retry_delays_ms[index].min(remaining_ms);
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        attempt += 1;
    }
}

fn already_exists_error() -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, "EEXIST")
}

fn is_permission_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::PermissionDenied
}

fn file_lock_timeout(target: &Path, lock_dir: &Path, waited_ms: u64) -> ServiceError {
    // atomicFileLock.ts:159-173 - The same copy and error code, the client uses this to distinguish timeout and permission failure.
    ServiceError::new(format!(
        "Timed out after {waited_ms}ms waiting for the ZCode file lock: {}",
        lock_dir.display()
    ))
    .with_code(FILE_LOCK_TIMEOUT_ERROR_CODE)
    .with_data(json!({ "path": target.to_string_lossy() }))
}

pub fn lock_dir_for(file_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", file_path.to_string_lossy()))
}

async fn write_owner_exclusive(owner_file: &Path, payload: &str) -> Result<(), io::Error> {
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true) // flag "wx" (atomicFileLock.ts:203)
        .open(owner_file)
        .await?;
    file.write_all(payload.as_bytes()).await?;
    file.shutdown().await
}

async fn list_owner_files(lock_dir: &Path) -> Result<Vec<String>, io::Error> {
    let mut entries = tokio::fs::read_dir(lock_dir).await?;
    let mut owners = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with("owner-") && name.ends_with(".json") {
            owners.push(name);
        }
    }
    Ok(owners)
}

/// atomicFileLock.ts:22-25 - Only owner-*.json participates in owner determination, other entries are only deleted during cleanup.
async fn list_all_entries(lock_dir: &Path) -> Result<Vec<String>, io::Error> {
    let mut entries = tokio::fs::read_dir(lock_dir).await?;
    let mut names = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    Ok(names)
}

/// Lock instance identity: Unix uses dev/ino, windows uses volume number + file index (equivalent to TS's dev/ino).
#[derive(Clone, PartialEq, Eq, Debug)]
struct LockIdentity(String);

async fn lock_identity(path: &Path) -> Result<LockIdentity, io::Error> {
    let metadata = tokio::fs::metadata(path).await?;
    Ok(identity_of(&metadata))
}

fn identity_of(metadata: &std::fs::Metadata) -> LockIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        LockIdentity(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        // Stable identity: the lock directory's birth time. `modified` must not be part
        // of the identity: writing the owner file inside the directory changes mtime, so
        // created != modified would make every freshly-created lock look like someone
        // else's (the owner check then deletes it and retries until maxWait).
        let created = metadata
            .created()
            .map(duration_since_epoch_ms)
            .unwrap_or_default();
        LockIdentity(format!("{created}"))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        LockIdentity(String::new())
    }
}

fn instance_key(metadata: &std::fs::Metadata) -> String {
    // TS uses dev:ino:birthtimeMs:mtimeMs; here use identity + mtime (it can still be identified when birthtime is not available).
    let modified = metadata
        .modified()
        .map(duration_since_epoch_ms)
        .unwrap_or_default();
    let created = metadata
        .created()
        .map(duration_since_epoch_ms)
        .unwrap_or_default();
    format!("{}:{created}:{modified}", identity_of(metadata).0)
}

struct RemovalAttempt {
    removed: bool,
    error: Option<io::Error>,
}

impl RemovalAttempt {
    fn kept() -> Self {
        Self {
            removed: false,
            error: None,
        }
    }

    fn cleared() -> Self {
        Self {
            removed: true,
            error: None,
        }
    }

    /// atomicFileLock.ts:151-156 - ENOENT/ENOTEMPTY is regarded as "not grabbed", and other errors are left to the upper layer to determine the authority.
    fn from_error(error: io::Error) -> Self {
        if matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
        ) {
            return Self::kept();
        }
        Self {
            removed: false,
            error: Some(error),
        }
    }
}

async fn remove_abandoned_lock(
    lock_dir: &Path,
    ownerless_grace_ms: u64,
    observer: &mut LockInstanceObserver,
) -> RemovalAttempt {
    let metadata = match tokio::fs::metadata(lock_dir).await {
        Ok(metadata) => metadata,
        Err(error) => return RemovalAttempt::from_error(error),
    };
    if metadata.is_dir() {
        let entries = match list_all_entries(lock_dir).await {
            Ok(entries) => entries,
            Err(error) => return RemovalAttempt::from_error(error),
        };
        let owners: Vec<String> = entries
            .iter()
            .filter(|name| name.starts_with("owner-") && name.ends_with(".json"))
            .cloned()
            .collect();
        if owners.len() == 1 {
            // atomicFileLock.ts:101-111 - Delete the unique owner file equal to ownership verification, then rmdir.
            let owner_path = lock_dir.join(&owners[0]);
            match is_owner_file_reclaimable(&owner_path, ownerless_grace_ms, observer).await {
                Ok(true) => {}
                Ok(false) => return RemovalAttempt::kept(),
                Err(error) => return RemovalAttempt::from_error(error),
            }
            if let Err(error) = tokio::fs::remove_file(&owner_path).await {
                return RemovalAttempt::from_error(error);
            }
            return match tokio::fs::remove_dir(lock_dir).await {
                Ok(()) => RemovalAttempt::cleared(),
                Err(error) => RemovalAttempt::from_error(error),
            };
        }
        // atomicFileLock.ts:113-137 - 0 or more owners: calculated based on directory mtime, and can be recycled after verification one by one.
        let observed_at = now_millis();
        let directory_timestamp = parse_lock_timestamp(
            metadata
                .modified()
                .map(duration_since_epoch_ms)
                .unwrap_or_default(),
            observed_at,
        )
        .unwrap_or_else(|| observer.observe(lock_dir, &instance_key(&metadata), observed_at));
        if observed_at.saturating_sub(directory_timestamp) < ownerless_grace_ms {
            return RemovalAttempt::kept();
        }
        for owner in &owners {
            match is_owner_file_reclaimable(&lock_dir.join(owner), ownerless_grace_ms, observer)
                .await
            {
                Ok(true) => {}
                Ok(false) => return RemovalAttempt::kept(),
                Err(error) => return RemovalAttempt::from_error(error),
            }
        }
        for entry in &entries {
            if let Err(error) = tokio::fs::remove_file(lock_dir.join(entry)).await {
                return RemovalAttempt::from_error(error);
            }
        }
        return match tokio::fs::remove_dir(lock_dir).await {
            Ok(()) => RemovalAttempt::cleared(),
            Err(error) => RemovalAttempt::from_error(error),
        };
    }

    // atomicFileLock.ts:140-150 - Compatible with the single file lock before upgrade; the content reviewed before deletion has not been replaced.
    let raw = match tokio::fs::read_to_string(lock_dir).await {
        Ok(raw) => raw,
        Err(error) => return RemovalAttempt::from_error(error),
    };
    match is_owner_file_reclaimable(lock_dir, ownerless_grace_ms, observer).await {
        Ok(true) => {}
        Ok(false) => return RemovalAttempt::kept(),
        Err(error) => return RemovalAttempt::from_error(error),
    }
    let after = match tokio::fs::read_to_string(lock_dir).await {
        Ok(after) => after,
        Err(error) => return RemovalAttempt::from_error(error),
    };
    if after != raw {
        return RemovalAttempt::kept();
    }
    match tokio::fs::remove_file(lock_dir).await {
        Ok(()) => RemovalAttempt::cleared(),
        Err(error) => RemovalAttempt::from_error(error),
    }
}

/// atomicFileLock.ts:69-87 - Recycling is only allowed if the owner has exited, or has no pid and exceeds grace.
async fn is_owner_file_reclaimable(
    owner_file: &Path,
    ownerless_grace_ms: u64,
    observer: &mut LockInstanceObserver,
) -> Result<bool, io::Error> {
    let observed_at = now_millis();
    let raw = tokio::fs::read_to_string(owner_file).await?;
    let metadata = parse_lock_metadata(&raw, observed_at);
    let created_at = match metadata.created_at {
        Some(created_at) => created_at,
        None => {
            let file_metadata = tokio::fs::metadata(owner_file).await?;
            parse_lock_timestamp(
                file_metadata
                    .modified()
                    .map(duration_since_epoch_ms)
                    .unwrap_or_default(),
                observed_at,
            )
            .unwrap_or_else(|| {
                observer.observe(owner_file, &instance_key(&file_metadata), observed_at)
            })
        }
    };
    let owner_exited = metadata.pid.is_some_and(|pid| !pid_alive(pid));
    let ownerless_is_stale =
        metadata.pid.is_none() && observed_at.saturating_sub(created_at) >= ownerless_grace_ms;
    Ok(owner_exited || ownerless_is_stale)
}

struct LockMetadata {
    created_at: Option<u64>,
    pid: Option<u32>,
}

/// atomicFileLock.ts:27-53 - Illegal/negative/obviously future createdAt cannot permanently fail to reach stale;
/// 0, negative number, non-integer pid has no owner semantics and must fall back to file mtime judgment.
fn parse_lock_metadata(raw: &str, observed_at: u64) -> LockMetadata {
    let parsed: Value = match serde_json::from_str(raw) {
        Ok(parsed) => parsed,
        Err(_) => {
            return LockMetadata {
                created_at: None,
                pid: None,
            }
        }
    };
    LockMetadata {
        created_at: parsed
            .get("createdAt")
            .and_then(Value::as_u64)
            .and_then(|value| parse_lock_timestamp(value, observed_at)),
        pid: parsed.get("pid").and_then(Value::as_u64).and_then(|pid| {
            if pid > 0 && pid <= u64::from(u32::MAX) {
                u32::try_from(pid).ok()
            } else {
                None
            }
        }),
    }
}

fn parse_lock_timestamp(value: u64, observed_at: u64) -> Option<u64> {
    if value <= observed_at.saturating_add(MAX_LOCK_METADATA_CLOCK_SKEW_MS) {
        Some(value)
    } else {
        None
    }
}

/// `createLockInstanceObserver`(lockInstanceObserver.ts): Double illegal timestamp can only be obtained from
/// Grace starts when the lock instance is first seen; the lock must be reset after it is replaced.
#[derive(Default)]
struct LockInstanceObserver {
    observations: HashMap<String, (u64, String)>,
}

impl LockInstanceObserver {
    fn observe(&mut self, path: &Path, identity: &str, observed_at: u64) -> u64 {
        let key = path.to_string_lossy().into_owned();
        if let Some((first_observed_at, previous)) = self.observations.get(&key) {
            if previous == identity {
                return *first_observed_at;
            }
        }
        self.observations
            .insert(key, (observed_at, identity.to_string()));
        observed_at
    }
}

static TOKEN_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// atomicFileLock.ts:182 —— `{pid}-{Date.now()}-{random}`; the random amount only needs to ensure no collision within the same process.
fn new_lock_token() -> String {
    let sequence = TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u64(sequence);
    hasher.write_u128(nanos);
    hasher.write_u32(std::process::id());
    format!(
        "{}-{}-{:x}",
        std::process::id(),
        now_millis(),
        hasher.finish()
    )
}

pub(crate) fn now_millis() -> u64 {
    duration_since_epoch_ms(SystemTime::now())
}

fn duration_since_epoch_ms(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// Survival judgment image TS `isProcessAlive`: When in doubt, always press "alive" to handle conservatively.
/// It is better to wait until maxWait times out than delete the lock that is being held by others.
////// Reuse `nc_code_proc::pid_alive` (OpenProcess/GetExitCodeProcess on Windows,
/// kill(pid,0) on unix), do not introduce windows/libc dependency in this crate (ENTRY 084: original implementation
/// Links two undeclared platform crates).
fn pid_alive(pid: u32) -> bool {
    nc_code_proc::pid_alive(pid)
}

#[cfg(not(any(windows, unix)))]
fn pid_alive(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_path(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "zcode-filelock-{}-{tag}-{}",
            std::process::id(),
            TOKEN_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).expect("dir");
        root.join("state.json")
    }

    fn options(max_wait_ms: u64) -> LockOptions {
        LockOptions {
            retry_delays_ms: vec![5, 10],
            ownerless_grace_ms: 20,
            max_wait_ms,
        }
    }

    /// Take a PID that is sure to have exited: run a non-existent use case of the test binary and wait.
    fn dead_pid() -> u32 {
        let executable = std::env::current_exe().expect("current exe");
        let mut child = std::process::Command::new(executable)
            .arg("--exact")
            .arg("ZCODE_file_lock_no_such_case")
            .arg("--nocapture")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn child");
        let pid = child.id();
        let _ = child.wait();
        assert!(!pid_alive(pid), "child {pid} must be reaped before use");
        pid
    }

    async fn write_stale_lock(state: &Path, owner_payload: &str, age_ms: u64) -> PathBuf {
        let lock_dir = lock_dir_for(state);
        std::fs::create_dir_all(&lock_dir).expect("lock dir");
        let owner_file = lock_dir.join("owner-stale.json");
        std::fs::write(&owner_file, owner_payload).expect("owner");
        let old = SystemTime::now() - Duration::from_millis(age_ms);
        let file = std::fs::OpenOptions::new()
            .append(true)
            .open(&owner_file)
            .expect("open for times");
        file.set_times(std::fs::FileTimes::new().set_modified(old))
            .expect("set mtime");
        lock_dir
    }

    #[tokio::test]
    async fn lock_is_created_exclusive_and_removed_on_release() {
        let state = temp_path("exclusive");
        let lock_dir = lock_dir_for(&state);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let task = {
            let (entered, release, state, lock_dir) = (
                Arc::clone(&entered),
                Arc::clone(&release),
                state.clone(),
                lock_dir.clone(),
            );
            tokio::spawn(async move {
                with_file_lock(&state, LockOptions::default(), move || {
                    let (entered, release, lock_dir) =
                        (Arc::clone(&entered), Arc::clone(&release), lock_dir.clone());
                    async move {
                        //During the lock holding period: the lock directory exists and has only one owner-*.json.
                        let owners = std::fs::read_dir(&lock_dir)
                            .expect("lock dir while held")
                            .filter_map(|entry| entry.ok())
                            .map(|entry| entry.file_name().to_string_lossy().into_owned())
                            .filter(|name| name.starts_with("owner-") && name.ends_with(".json"))
                            .collect::<Vec<_>>();
                        assert_eq!(owners.len(), 1, "exactly one owner file while held");
                        entered.notify_one();
                        release.notified().await;
                        Ok(())
                    }
                })
                .await
            })
        };
        entered.notified().await;
        release.notify_one();
        task.await.expect("task").expect("lock");
        assert!(
            !lock_dir.exists(),
            "released lock directory must be removed"
        );
    }

    #[tokio::test]
    async fn two_tasks_in_one_process_serialize() {
        let state = temp_path("serialize");
        let overlap = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let inside = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..4 {
            let state = state.clone();
            let overlap = Arc::clone(&overlap);
            let inside = Arc::clone(&inside);
            tasks.push(tokio::spawn(async move {
                with_file_lock(&state, LockOptions::default(), move || {
                    let overlap = Arc::clone(&overlap);
                    let inside = Arc::clone(&inside);
                    async move {
                        if inside.fetch_add(1, Ordering::SeqCst) > 0 {
                            overlap.store(true, Ordering::SeqCst);
                        }
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        inside.fetch_sub(1, Ordering::SeqCst);
                        Ok(())
                    }
                })
                .await
            }));
        }
        for task in tasks {
            task.await.expect("task").expect("lock");
        }
        assert!(
            !overlap.load(Ordering::SeqCst),
            "holders must never overlap inside one process"
        );
        assert!(!lock_dir_for(&state).exists());
    }

    #[tokio::test]
    async fn abandoned_lock_with_exited_pid_is_reclaimed() {
        let state = temp_path("dead-owner");
        let pid = dead_pid();
        let lock_dir = write_stale_lock(
            &state,
            &format!(
                "{{\"pid\": {pid}, \"createdAt\": {}, \"token\": \"stale\"}}\n",
                now_millis()
            ),
            5_000,
        )
        .await;
        let value = with_file_lock(&state, options(1_000), || async { Ok(42u32) })
            .await
            .expect("stale lock must be reclaimed");
        assert_eq!(value, 42);
        assert!(!lock_dir.exists());
        std::fs::remove_dir_all(state.parent().expect("parent")).ok();
    }

    #[tokio::test]
    async fn ownerless_lock_is_reclaimed_only_after_grace() {
        // No pid and grace exceeded → recyclable.
        let state = temp_path("ownerless-stale");
        write_stale_lock(
            &state,
            &format!(
                "{{\"pid\": null, \"createdAt\": {}, \"token\": \"x\"}}\n",
                now_millis() - 5_000
            ),
            5_000,
        )
        .await;
        assert_eq!(
            with_file_lock(&state, options(1_000), || async { Ok(1u32) })
                .await
                .expect("reclaimed"),
            1
        );

        // owner pid is still alive (this process) → wait until maxWait times out and never grab the lock.
        let fresh = temp_path("owner-alive");
        write_stale_lock(
            &fresh,
            &format!(
                "{{\"pid\": {}, \"createdAt\": {}, \"token\": \"live\"}}\n",
                std::process::id(),
                now_millis()
            ),
            0,
        )
        .await;
        let error = with_file_lock(&fresh, options(120), || async { Ok(1u32) })
            .await
            .expect_err("a live owner must block the waiter");
        assert_eq!(
            error.code.as_deref(),
            Some(&json!(FILE_LOCK_TIMEOUT_ERROR_CODE))
        );
        assert!(error.message.contains("Timed out after"));
    }

    #[tokio::test]
    async fn malformed_metadata_falls_back_to_owner_mtime() {
        let state = temp_path("malformed");
        // createdAt is the future time and pid is illegal → determine stale based on the mtime of the owner file.
        write_stale_lock(
            &state,
            &format!(
                "{{\"pid\": \"{}\", \"createdAt\": {}, \"token\": 1}}\n",
                std::process::id(),
                now_millis() + 3_600_000
            ),
            5_000,
        )
        .await;
        assert_eq!(
            with_file_lock(&state, options(1_000), || async { Ok(7u32) })
                .await
                .expect("reclaimed via mtime"),
            7
        );
    }

    #[test]
    fn metadata_parsing_mirrors_typescript_guards() {
        let observed = 1_700_000_000_000;
        let good = parse_lock_metadata(
            &format!("{{\"pid\": 42, \"createdAt\": {}}}", observed - 10),
            observed,
        );
        assert_eq!(good.pid, Some(42));
        assert_eq!(good.created_at, Some(observed - 10));

        // In the future, createdAt and 0/negative/decimal pid will have no owner semantics.
        let bad = parse_lock_metadata(
            &format!("{{\"pid\": 0, \"createdAt\": {}}}", observed + 9_999_999),
            observed,
        );
        assert_eq!(bad.pid, None);
        assert_eq!(bad.created_at, None);

        let not_json = parse_lock_metadata("not json", observed);
        assert_eq!(not_json.pid, None);
        assert_eq!(not_json.created_at, None);
    }

    #[test]
    fn lock_directory_name_matches_typescript_suffix() {
        assert_eq!(
            lock_dir_for(Path::new("G:/tmp/provisioning.json")),
            PathBuf::from("G:/tmp/provisioning.json.lock")
        );
    }

    #[tokio::test]
    async fn observer_resets_when_the_lock_instance_changes() {
        // temp_path returns root/state.json (file); use owner-x.json in the same directory to observe instance changes.
        let path = temp_path("observer").with_file_name("owner-x.json");
        let dir = path.parent().expect("parent dir").to_path_buf();
        std::fs::write(&path, "x").expect("write");
        let first = now_millis() - 1_000;
        let mut observer = LockInstanceObserver::default();
        let observed = observer.observe(&path, "identity-a", first);
        assert_eq!(observed, first);
        let same = observer.observe(&path, "identity-a", first + 500);
        assert_eq!(same, first, "same instance keeps the first observation");
        let replaced = observer.observe(&path, "identity-b", first + 900);
        assert_eq!(replaced, first + 900, "a new instance must reset the grace");
        std::fs::remove_dir_all(dir).ok();
    }
}
