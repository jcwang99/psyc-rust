use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use unicode_normalization::UnicodeNormalization;

const WINDOWS_RESERVED_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];
const LOCAL_CHECKOUT_MAPPING_FILE: &str = ".e2v-checkout-mapping.json";
const LARGE_FILE_SNAPSHOT_PREFERENCE_BYTES: u64 = 8 * 1024 * 1024;
const VOLATILE_REREAD_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkingTreeEntry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
}

#[derive(Debug, Clone)]
pub struct WorkingTree {
    repo_root: PathBuf,
    stable_read_policy: StableReadPolicy,
    snapshot_reader: Option<Arc<dyn SnapshotReader>>,
}

pub trait SnapshotReader: Send + Sync + std::fmt::Debug {
    fn read(&self, path: &Path) -> Result<Vec<u8>>;
}

impl WorkingTree {
    pub fn new(repo_root: impl AsRef<Path>) -> Self {
        Self::new_with_policy(repo_root, StableReadPolicy::default())
    }

    pub fn new_with_policy(
        repo_root: impl AsRef<Path>,
        stable_read_policy: StableReadPolicy,
    ) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
            stable_read_policy,
            snapshot_reader: None,
        }
    }

    pub fn new_with_snapshot_reader(
        repo_root: impl AsRef<Path>,
        stable_read_policy: StableReadPolicy,
        snapshot_reader: Arc<dyn SnapshotReader>,
    ) -> Self {
        Self {
            repo_root: repo_root.as_ref().to_path_buf(),
            stable_read_policy,
            snapshot_reader: Some(snapshot_reader),
        }
    }

    pub fn stable_read_policy(&self) -> StableReadPolicy {
        self.stable_read_policy.clone()
    }

    pub fn scan_dir(&self, dir: &Path, skip_control_dir: bool) -> Result<Vec<WorkingTreeEntry>> {
        let mut entries = Vec::new();
        let mut case_folded_names = HashSet::new();

        for entry in fs::read_dir(dir)
            .with_context(|| format!("failed to read repo directory {}", dir.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            let raw_name = entry.file_name().to_string_lossy().to_string();

            if skip_control_dir
                && dir == self.repo_root
                && (raw_name == ".e2v" || raw_name == LOCAL_CHECKOUT_MAPPING_FILE)
            {
                continue;
            }

            let name = self.normalize_path_component(&raw_name);
            self.validate_path_component(&name)?;
            self.validate_case_fold_uniqueness(&mut case_folded_names, &name)?;

            entries.push(WorkingTreeEntry {
                name,
                path,
                is_dir: file_type.is_dir(),
            });
        }

        entries.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(entries)
    }

    pub fn open_stable_file(&self, path: &Path) -> Result<Vec<u8>> {
        let policy = self.stable_read_policy();
        let before = self.capture_metadata(path)?;
        if should_prefer_snapshot(&before, std::time::SystemTime::now())
            && let Some(snapshot_reader) = &self.snapshot_reader
            && let Ok(bytes) = snapshot_reader.read(path)
        {
            return Ok(bytes);
        }
        if is_volatile_source(&before, std::time::SystemTime::now()) {
            return read_volatile_with_retry(
                || {
                    let before = self.capture_metadata(path)?;
                    let first = fs::read(path).with_context(|| {
                        format!("failed to read working tree file {}", path.display())
                    })?;
                    let after_first = self.capture_metadata(path)?;
                    std::thread::sleep(VOLATILE_REREAD_DELAY);
                    let second = fs::read(path).with_context(|| {
                        format!("failed to read working tree file {}", path.display())
                    })?;
                    let after_second = self.capture_metadata(path)?;
                    Ok((before, first, after_first, second, after_second))
                },
                policy.volatile_retry_attempts,
                path,
            );
        }

        read_with_metadata_retry(
            || {
                let before = self.capture_metadata(path)?;
                let bytes = fs::read(path).with_context(|| {
                    format!("failed to read working tree file {}", path.display())
                })?;
                let after = self.capture_metadata(path)?;
                Ok((before, bytes, after))
            },
            policy.metadata_retry_attempts,
            path,
        )
    }

    pub fn plan_checkout(&self, target_dir: &Path, relative_path: &str) -> Result<PathBuf> {
        self.path_jail_validate(relative_path)?;
        let mut current = target_dir.to_path_buf();
        let normalized = relative_path.trim_matches('/');
        if normalized.is_empty() {
            return Ok(current);
        }

        for component in normalized.split('/') {
            self.validate_path_component(component)?;
            current.push(component);
        }

        Ok(current)
    }

    pub fn path_jail_validate(&self, relative_path: &str) -> Result<()> {
        let normalized = relative_path.trim_matches('/');
        if normalized.is_empty() {
            return Ok(());
        }

        for component in normalized.split('/') {
            ensure!(
                component != "." && component != "..",
                "path policy violation: checkout path escapes target root"
            );
            self.validate_path_component(component)?;
        }

        Ok(())
    }

    pub fn ensure_checkout_target_is_clear(&self, path: &Path) -> Result<()> {
        if let Ok(metadata) = fs::metadata(path) {
            ensure!(
                metadata.is_file(),
                "checkout conflict: target path is not a file: {}",
                path.display()
            );
        }

        Ok(())
    }

    pub fn ensure_checkout_parent_paths_are_creatable(
        &self,
        target_dir: &Path,
        final_path: &Path,
    ) -> Result<()> {
        let relative_parent = match final_path.parent() {
            Some(parent) => match parent.strip_prefix(target_dir) {
                Ok(path) => path,
                Err(_) => {
                    anyhow::bail!(
                        "checkout conflict: target path escapes checkout root: {}",
                        final_path.display()
                    )
                }
            },
            None => return Ok(()),
        };

        let mut current = target_dir.to_path_buf();
        for component in relative_parent.components() {
            current.push(component);
            if let Ok(metadata) = fs::metadata(&current) {
                ensure!(
                    metadata.is_dir(),
                    "checkout conflict: parent path is not a directory: {}",
                    current.display()
                );
            }
        }

        Ok(())
    }

    pub fn preflight_checkout_paths(
        &self,
        target_dir: &Path,
        relative_paths: &[String],
    ) -> Result<Vec<PathBuf>> {
        self.preflight_checkout_paths_for_bytes(target_dir, relative_paths, 0)
    }

    pub fn preflight_checkout_paths_for_bytes(
        &self,
        target_dir: &Path,
        relative_paths: &[String],
        required_bytes: u64,
    ) -> Result<Vec<PathBuf>> {
        self.preflight_checkout_paths_with(
            target_dir,
            relative_paths,
            required_bytes,
            available_space_for_path,
            probe_directory_writable,
        )
    }

    pub fn preflight_checkout_paths_with<FS, WP>(
        &self,
        target_dir: &Path,
        relative_paths: &[String],
        required_bytes: u64,
        mut available_space_for_target: FS,
        mut probe_writable_directory: WP,
    ) -> Result<Vec<PathBuf>>
    where
        FS: FnMut(&Path) -> Result<u64>,
        WP: FnMut(&Path) -> Result<()>,
    {
        let mut planned = Vec::with_capacity(relative_paths.len());
        let mut folded_paths = HashSet::new();
        let mut probed_directories = HashSet::new();
        for relative_path in relative_paths {
            let final_path = self.plan_checkout(target_dir, relative_path)?;
            let folded = final_path.to_string_lossy().to_lowercase();
            ensure!(
                folded_paths.insert(folded),
                "path policy violation: case-fold collision in checkout targets"
            );
            self.ensure_checkout_parent_paths_are_creatable(target_dir, &final_path)?;
            let writable_dir = self.nearest_existing_checkout_directory(target_dir, &final_path)?;
            if probed_directories.insert(writable_dir.clone()) {
                probe_writable_directory(&writable_dir)?;
            }
            self.ensure_checkout_target_is_clear(&final_path)?;
            planned.push(final_path);
        }
        if required_bytes > 0 {
            let available = available_space_for_target(target_dir)?;
            ensure!(
                available >= required_bytes,
                "checkout preflight failed: insufficient disk space (required {required_bytes} bytes, available {available} bytes)"
            );
        }
        Ok(planned)
    }

    pub fn write_checkout_temp(&self, final_path: &Path, bytes: &[u8]) -> Result<PathBuf> {
        let parent = final_path
            .parent()
            .with_context(|| format!("checkout target has no parent: {}", final_path.display()))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create checkout parent {}", parent.display()))?;
        self.ensure_checkout_target_is_clear(final_path)?;

        let temp_path = final_path.with_extension("e2v-tmp");
        fs::write(&temp_path, bytes).with_context(|| {
            format!("failed to write checkout temp file {}", temp_path.display())
        })?;
        Ok(temp_path)
    }

    pub fn publish_checkout_temp(&self, temp_path: &Path, final_path: &Path) -> Result<()> {
        if final_path.exists() {
            fs::remove_file(final_path).with_context(|| {
                format!(
                    "failed to replace existing checkout file {}",
                    final_path.display()
                )
            })?;
        }

        fs::rename(temp_path, final_path)
            .with_context(|| format!("failed to publish checkout file {}", final_path.display()))?;
        Ok(())
    }

    pub fn validate_checkout_read_back(
        &self,
        expected_name: &str,
        observed_name: &str,
    ) -> Result<()> {
        let expected = self.normalize_path_component(expected_name).to_lowercase();
        let observed = self.normalize_path_component(observed_name).to_lowercase();
        ensure!(
            expected == observed,
            "path policy violation: checkout read-back mismatch ({expected_name} != {observed_name})"
        );
        Ok(())
    }

    pub fn observed_checkout_name(&self, final_path: &Path) -> Result<String> {
        let parent = final_path
            .parent()
            .with_context(|| format!("checkout target has no parent: {}", final_path.display()))?;
        let expected_name = final_path
            .file_name()
            .and_then(|name| name.to_str())
            .with_context(|| {
                format!("checkout target has no file name: {}", final_path.display())
            })?;
        let mut observed_names = Vec::new();
        for entry in fs::read_dir(parent)
            .with_context(|| format!("failed to read checkout parent {}", parent.display()))?
        {
            let entry = entry?;
            observed_names.push(entry.file_name().to_string_lossy().to_string());
        }

        self.select_observed_checkout_name(expected_name, &observed_names)
            .with_context(|| format!("checkout read-back failed for {}", final_path.display()))
    }

    pub fn select_observed_checkout_name(
        &self,
        expected_name: &str,
        observed_names: &[String],
    ) -> Result<String> {
        let normalized_expected = self.normalize_path_component(expected_name);
        observed_names
            .iter()
            .find(|observed| self.normalize_path_component(observed) == normalized_expected)
            .cloned()
            .with_context(|| format!("file not found after publish: {expected_name}"))
    }

    pub(crate) fn write_platform_name_mappings<'a, I>(&self, mappings_to_write: I) -> Result<()>
    where
        I: IntoIterator<Item = (&'a str, &'a Path)>,
    {
        let mapping_path = self.repo_root.join(".e2v-checkout-mapping.json");
        let mut mappings = Vec::new();
        let mut seen_snapshot_paths = HashSet::new();

        for (snapshot_path, final_path) in mappings_to_write {
            if !seen_snapshot_paths.insert(snapshot_path.to_string()) {
                continue;
            }
            mappings.push(CheckoutPathMapping {
                snapshot_path: snapshot_path.to_string(),
                local_path: final_path.to_string_lossy().to_string(),
            });
        }

        if mappings.is_empty() {
            if mapping_path.is_file() {
                fs::remove_file(&mapping_path).with_context(|| {
                    format!(
                        "failed to remove checkout mapping {}",
                        mapping_path.display()
                    )
                })?;
            }
            return Ok(());
        }

        let bytes = serde_json::to_vec(&mappings).context("failed to encode checkout mapping")?;
        fs::write(&mapping_path, bytes).with_context(|| {
            format!(
                "failed to write checkout mapping {}",
                mapping_path.display()
            )
        })?;
        Ok(())
    }

    pub(crate) fn read_platform_name_mappings(&self) -> Result<Vec<(String, PathBuf)>> {
        let mapping_path = self.repo_root.join(LOCAL_CHECKOUT_MAPPING_FILE);
        match fs::symlink_metadata(&mapping_path) {
            Ok(metadata) => {
                ensure!(
                    !metadata.is_dir(),
                    "failed to read checkout mapping {}",
                    mapping_path.display()
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read checkout mapping {}", mapping_path.display())
                });
            }
        }
        let bytes = fs::read(&mapping_path).with_context(|| {
            format!("failed to read checkout mapping {}", mapping_path.display())
        })?;
        let mappings: Vec<CheckoutPathMapping> =
            serde_json::from_slice(&bytes).context("failed to decode checkout mapping")?;
        Ok(mappings
            .into_iter()
            .map(|mapping| (mapping.snapshot_path, PathBuf::from(mapping.local_path)))
            .collect())
    }

    pub(crate) fn remove_stale_platform_name_mappings<'a, I>(
        &self,
        previous_mappings: &[(String, PathBuf)],
        current_mappings: I,
    ) -> Result<()>
    where
        I: IntoIterator<Item = (&'a str, &'a Path)>,
    {
        let mut current_snapshot_paths = HashSet::new();
        let mut current_local_paths = HashSet::new();
        for (snapshot_path, local_path) in current_mappings {
            current_snapshot_paths.insert(snapshot_path.to_string());
            current_local_paths.insert(local_path.to_path_buf());
        }

        for (snapshot_path, local_path) in previous_mappings {
            if current_snapshot_paths.contains(snapshot_path)
                || current_local_paths.contains(local_path)
                || !local_path.starts_with(&self.repo_root)
            {
                continue;
            }
            let metadata = match fs::symlink_metadata(local_path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to inspect stale checkout path {}",
                            local_path.display()
                        )
                    });
                }
            };
            let file_type = metadata.file_type();
            if metadata.is_file() || file_type.is_symlink() {
                fs::remove_file(local_path).with_context(|| {
                    format!(
                        "failed to remove stale checkout path {}",
                        local_path.display()
                    )
                })?;
                self.remove_empty_checkout_ancestors(local_path.parent())?;
            }
        }

        Ok(())
    }

    fn remove_empty_checkout_ancestors(&self, start: Option<&Path>) -> Result<()> {
        let mut current = match start {
            Some(path) => path.to_path_buf(),
            None => return Ok(()),
        };

        while current.starts_with(&self.repo_root) && current != self.repo_root {
            match fs::remove_dir(&current) {
                Ok(()) => {
                    if !current.pop() {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if !current.pop() {
                        break;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => break,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to remove empty checkout directory {}",
                            current.display()
                        )
                    });
                }
            }
        }

        Ok(())
    }

    fn nearest_existing_checkout_directory(
        &self,
        target_dir: &Path,
        final_path: &Path,
    ) -> Result<PathBuf> {
        let mut current = final_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| target_dir.to_path_buf());

        while !current.exists() {
            ensure!(
                current.pop(),
                "checkout conflict: no existing parent directory for {}",
                final_path.display()
            );
        }

        ensure!(
            current.starts_with(target_dir),
            "checkout conflict: target path escapes checkout root: {}",
            final_path.display()
        );
        let metadata = fs::metadata(&current)
            .with_context(|| format!("failed to stat checkout directory {}", current.display()))?;
        ensure!(
            metadata.is_dir(),
            "checkout conflict: parent path is not a directory: {}",
            current.display()
        );
        Ok(current)
    }

    fn capture_metadata(&self, path: &Path) -> Result<FileMetadata> {
        let metadata = fs::metadata(path)
            .with_context(|| format!("failed to stat working tree file {}", path.display()))?;
        let modified = metadata
            .modified()
            .with_context(|| format!("failed to read modified time for {}", path.display()))?;
        let created = metadata.created().ok();

        Ok(FileMetadata {
            len: metadata.len(),
            modified,
            created,
        })
    }

    fn validate_path_component(&self, name: &str) -> Result<()> {
        ensure!(
            !name.is_empty(),
            "path policy violation: empty path component"
        );
        ensure!(
            !name.contains('/'),
            "path policy violation: '/' is not allowed"
        );
        ensure!(
            !name.contains('\0'),
            "path policy violation: '\\0' is not allowed"
        );
        ensure!(
            !name.contains('<')
                && !name.contains('>')
                && !name.contains(':')
                && !name.contains('"')
                && !name.contains('\\')
                && !name.contains('|')
                && !name.contains('?')
                && !name.contains('*'),
            "path policy violation: invalid portable-strict character in {name}"
        );
        ensure!(
            !name.ends_with(' ') && !name.ends_with('.'),
            "path policy violation: trailing space or dot is not allowed"
        );

        let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
        ensure!(
            !WINDOWS_RESERVED_NAMES.contains(&stem.as_str()),
            "path policy violation: reserved Windows name {name}"
        );

        Ok(())
    }

    fn normalize_path_component(&self, name: &str) -> String {
        name.nfc().collect()
    }

    fn validate_case_fold_uniqueness(
        &self,
        seen: &mut HashSet<String>,
        normalized_name: &str,
    ) -> Result<()> {
        let folded = normalized_name.to_lowercase();
        ensure!(
            seen.insert(folded),
            "path policy violation: case-fold collision for {normalized_name}"
        );
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileMetadata {
    len: u64,
    modified: std::time::SystemTime,
    created: Option<std::time::SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StableReadPolicy {
    pub metadata_retry_attempts: usize,
    pub volatile_retry_attempts: usize,
}

impl Default for StableReadPolicy {
    fn default() -> Self {
        Self {
            metadata_retry_attempts: 2,
            volatile_retry_attempts: 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct CheckoutPathMapping {
    snapshot_path: String,
    local_path: String,
}

fn validate_stable_metadata(
    before: &FileMetadata,
    after: &FileMetadata,
    path: &Path,
) -> Result<()> {
    ensure!(
        before == after,
        "stable read violation: file metadata changed during read for {}",
        path.display()
    );
    Ok(())
}

fn read_with_metadata_retry<F>(
    mut read_once: F,
    max_attempts: usize,
    path: &Path,
) -> Result<Vec<u8>>
where
    F: FnMut() -> Result<(FileMetadata, Vec<u8>, FileMetadata)>,
{
    let attempts = max_attempts.max(1);
    let mut last_error = None;

    for _ in 0..attempts {
        let (before, bytes, after) = read_once()?;
        match validate_stable_metadata(&before, &after, path) {
            Ok(()) => return Ok(bytes),
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("stable read violation")))
}

fn is_volatile_source(metadata: &FileMetadata, now: std::time::SystemTime) -> bool {
    match now.duration_since(metadata.modified) {
        Ok(age) => age.as_secs() < 2,
        Err(_) => true,
    }
}

fn should_prefer_snapshot(metadata: &FileMetadata, now: std::time::SystemTime) -> bool {
    metadata.len >= LARGE_FILE_SNAPSHOT_PREFERENCE_BYTES || is_volatile_source(metadata, now)
}

fn read_volatile_with_retry<F>(
    mut read_once: F,
    max_attempts: usize,
    path: &Path,
) -> Result<Vec<u8>>
where
    F: FnMut() -> Result<(FileMetadata, Vec<u8>, FileMetadata, Vec<u8>, FileMetadata)>,
{
    let attempts = max_attempts.max(1);
    let mut last_error = None;

    for _ in 0..attempts {
        let (before, first, after_first, second, after_second) = read_once()?;
        match (
            validate_stable_metadata(&before, &after_first, path),
            validate_stable_metadata(&after_first, &after_second, path),
        ) {
            (Ok(()), Ok(())) if first == second => return Ok(second),
            _ => {
                last_error = Some(anyhow::anyhow!(
                    "unstable input: volatile source retry budget exhausted for {}",
                    path.display()
                ));
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("stable read violation")))
}

fn probe_directory_writable(path: &Path) -> Result<()> {
    let probe_name = format!(
        ".e2v-write-probe-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let probe_path = path.join(probe_name);
    fs::write(&probe_path, []).with_context(|| {
        format!(
            "checkout preflight failed: target directory is not writable: {}",
            path.display()
        )
    })?;
    fs::remove_file(&probe_path).with_context(|| {
        format!(
            "checkout preflight failed: failed to remove write probe {}",
            probe_path.display()
        )
    })?;
    Ok(())
}

#[cfg(windows)]
fn available_space_for_path(path: &Path) -> Result<u64> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn GetDiskFreeSpaceExW(
            lpDirectoryName: *const u16,
            lpFreeBytesAvailableToCaller: *mut u64,
            lpTotalNumberOfBytes: *mut u64,
            lpTotalNumberOfFreeBytes: *mut u64,
        ) -> i32;
    }

    let mut wide_path = path.as_os_str().encode_wide().collect::<Vec<u16>>();
    wide_path.push(0);

    let mut available = 0u64;
    let result = unsafe {
        GetDiskFreeSpaceExW(
            wide_path.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "checkout preflight failed: could not query disk space for {}",
                path.display()
            )
        });
    }

    Ok(available)
}

#[cfg(unix)]
fn available_space_for_path(path: &Path) -> Result<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).with_context(|| {
        format!(
            "checkout preflight failed: invalid path for disk space query: {}",
            path.display()
        )
    })?;
    let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let result = unsafe { libc::statvfs(c_path.as_ptr(), stats.as_mut_ptr()) };
    if result != 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "checkout preflight failed: could not query disk space for {}",
                path.display()
            )
        });
    }
    let stats = unsafe { stats.assume_init() };
    Ok((stats.f_bavail as u64).saturating_mul(stats.f_frsize as u64))
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::WorkingTree;
    use tempfile::tempdir;

    #[derive(Debug)]
    struct RecordingSnapshotReader {
        result: Result<Vec<u8>, String>,
        calls: Arc<Mutex<Vec<std::path::PathBuf>>>,
    }

    impl super::SnapshotReader for RecordingSnapshotReader {
        fn read(&self, path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
            self.calls.lock().unwrap().push(path.to_path_buf());
            self.result.clone().map_err(anyhow::Error::msg)
        }
    }

    #[test]
    fn rejects_invalid_portable_strict_names() {
        let working_tree = WorkingTree::new("D:\\dummy");

        let invalid = [
            "bad:name.txt",
            "trailing. ",
            "NUL.txt",
            "bad?.txt",
            "bad\\name",
        ];
        for name in invalid {
            let error = working_tree.validate_path_component(name).unwrap_err();
            assert!(
                error.to_string().contains("path policy"),
                "unexpected error for {name}: {error:#}"
            );
        }
    }

    #[test]
    fn accepts_simple_portable_strict_names() {
        let working_tree = WorkingTree::new("D:\\dummy");

        for name in ["hello.txt", "nested", "README", "file-123.bin"] {
            working_tree.validate_path_component(name).unwrap();
        }
    }

    #[test]
    fn rejects_checkout_escape_components() {
        let working_tree = WorkingTree::new("D:\\dummy");

        let error = working_tree
            .path_jail_validate("../escape.txt")
            .unwrap_err();
        assert!(
            error.to_string().contains("path policy"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn detects_checkout_conflicts_before_write() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("conflict")).unwrap();
        let working_tree = WorkingTree::new(&root);

        let error = working_tree
            .ensure_checkout_target_is_clear(&root.join("conflict"))
            .unwrap_err();
        assert!(
            error.to_string().contains("checkout conflict"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn preflight_checkout_paths_rejects_conflicts() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(root.join("nested").join("hello.txt")).unwrap();
        let working_tree = WorkingTree::new(&root);

        let error = working_tree
            .preflight_checkout_paths(
                &root,
                &["a-root.txt".to_string(), "nested/hello.txt".to_string()],
            )
            .unwrap_err();

        assert!(
            error.to_string().contains("checkout conflict"),
            "unexpected error: {error:#}"
        );
        assert!(!root.join("a-root.txt").exists());
    }

    #[test]
    fn preflight_checkout_paths_rejects_file_parents_before_write() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("nested"), b"blocking-file").unwrap();
        let working_tree = WorkingTree::new(&root);

        let error = working_tree
            .preflight_checkout_paths(&root, &["nested/hello.txt".to_string()])
            .unwrap_err();

        assert!(
            error.to_string().contains("checkout conflict"),
            "unexpected error: {error:#}"
        );
        assert!(!root.join("nested").join("hello.txt").exists());
    }

    #[test]
    fn preflight_checkout_paths_rejects_insufficient_disk_space() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let working_tree = WorkingTree::new(&root);

        let error = working_tree
            .preflight_checkout_paths_with(
                &root,
                &["nested/hello.txt".to_string()],
                1024,
                |_path| Ok(512),
                |_path| Ok(()),
            )
            .unwrap_err();

        assert!(
            error.to_string().contains("disk space"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn preflight_checkout_paths_rejects_unwritable_parent_directory() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let working_tree = WorkingTree::new(&root);

        let error = working_tree
            .preflight_checkout_paths_with(
                &root,
                &["nested/hello.txt".to_string()],
                0,
                |_path| Ok(u64::MAX),
                |path| anyhow::bail!("target directory is not writable: {}", path.display()),
            )
            .unwrap_err();

        assert!(
            error.to_string().contains("not writable"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn preflight_checkout_paths_rejects_case_fold_collisions() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let working_tree = WorkingTree::new(&root);

        let error = working_tree
            .preflight_checkout_paths(&root, &["Readme".to_string(), "README".to_string()])
            .unwrap_err();

        assert!(
            error.to_string().contains("case-fold"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn checkout_read_back_accepts_unicode_equivalent_names() {
        let working_tree = WorkingTree::new("D:\\dummy");

        working_tree
            .validate_checkout_read_back("é.txt", "e\u{301}.txt")
            .unwrap();
    }

    #[test]
    fn checkout_read_back_accepts_case_fold_equivalent_names() {
        let working_tree = WorkingTree::new("D:\\dummy");

        working_tree
            .validate_checkout_read_back("Readme.txt", "README.TXT")
            .unwrap();
    }

    #[test]
    fn checkout_read_back_rejects_non_equivalent_names() {
        let working_tree = WorkingTree::new("D:\\dummy");

        let error = working_tree
            .validate_checkout_read_back("alpha.txt", "beta.txt")
            .unwrap_err();
        assert!(
            error.to_string().contains("read-back mismatch"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn observed_checkout_name_reads_back_from_filesystem() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let file_path = root.join("hello.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let working_tree = WorkingTree::new(&root);

        let observed = working_tree.observed_checkout_name(&file_path).unwrap();

        assert_eq!(observed, "hello.txt");
    }

    #[test]
    fn observed_checkout_name_accepts_unicode_equivalent_directory_entry() {
        let working_tree = WorkingTree::new("D:\\dummy");
        let observed = working_tree
            .select_observed_checkout_name("é.txt", &["e\u{301}.txt".to_string()])
            .unwrap();

        assert_eq!(observed, "e\u{301}.txt");
    }

    #[test]
    fn open_stable_file_reads_existing_file_bytes() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let file_path = root.join("hello.txt");
        std::fs::write(&file_path, b"hello").unwrap();
        let working_tree = WorkingTree::new(&root);

        let bytes = working_tree.open_stable_file(&file_path).unwrap();

        assert_eq!(bytes, b"hello");
    }

    #[test]
    fn stable_read_validation_rejects_changed_metadata() {
        let before = super::FileMetadata {
            len: 5,
            modified: std::time::UNIX_EPOCH,
            created: None,
        };
        let after = super::FileMetadata {
            len: 6,
            modified: std::time::UNIX_EPOCH,
            created: None,
        };
        let error =
            super::validate_stable_metadata(&before, &after, std::path::Path::new("demo.txt"))
                .unwrap_err();
        assert!(error.to_string().contains("stable read violation"));
    }

    #[test]
    fn stable_read_validation_rejects_changed_created_time_when_available() {
        let before = super::FileMetadata {
            len: 5,
            modified: std::time::UNIX_EPOCH,
            created: Some(std::time::UNIX_EPOCH),
        };
        let after = super::FileMetadata {
            len: 5,
            modified: std::time::UNIX_EPOCH,
            created: Some(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1)),
        };

        let error =
            super::validate_stable_metadata(&before, &after, std::path::Path::new("demo.txt"))
                .unwrap_err();

        assert!(error.to_string().contains("stable read violation"));
    }

    #[test]
    fn stable_read_retry_succeeds_after_one_changed_attempt() {
        let stable = super::FileMetadata {
            len: 5,
            modified: std::time::UNIX_EPOCH,
            created: None,
        };
        let changed = super::FileMetadata {
            len: 6,
            modified: std::time::UNIX_EPOCH,
            created: None,
        };
        let mut calls = 0usize;

        let bytes = super::read_with_metadata_retry(
            || {
                calls += 1;
                if calls == 1 {
                    Ok((stable.clone(), b"first".to_vec(), changed.clone()))
                } else {
                    Ok((stable.clone(), b"second".to_vec(), stable.clone()))
                }
            },
            2,
            std::path::Path::new("demo.txt"),
        )
        .unwrap();

        assert_eq!(bytes, b"second");
    }

    #[test]
    fn stable_read_retry_fails_after_budget_exhausted() {
        let before = super::FileMetadata {
            len: 5,
            modified: std::time::UNIX_EPOCH,
            created: None,
        };
        let after = super::FileMetadata {
            len: 6,
            modified: std::time::UNIX_EPOCH,
            created: None,
        };

        let error = super::read_with_metadata_retry(
            || Ok((before.clone(), b"payload".to_vec(), after.clone())),
            2,
            std::path::Path::new("demo.txt"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("stable read violation"));
    }

    #[test]
    fn volatile_source_detection_flags_recent_files() {
        let metadata = super::FileMetadata {
            len: 5,
            modified: std::time::SystemTime::now(),
            created: None,
        };

        assert!(super::is_volatile_source(
            &metadata,
            std::time::SystemTime::now()
        ));
    }

    #[test]
    fn volatile_source_detection_ignores_old_files() {
        let metadata = super::FileMetadata {
            len: 5,
            modified: std::time::UNIX_EPOCH,
            created: None,
        };

        assert!(!super::is_volatile_source(
            &metadata,
            std::time::SystemTime::now()
        ));
    }

    #[test]
    fn snapshot_preference_flags_large_files_even_when_old() {
        let metadata = super::FileMetadata {
            len: 9 * 1024 * 1024,
            modified: std::time::UNIX_EPOCH,
            created: None,
        };

        assert!(super::should_prefer_snapshot(
            &metadata,
            std::time::SystemTime::now()
        ));
    }

    #[test]
    fn stable_read_policy_exposes_default_retry_budget() {
        let working_tree = WorkingTree::new("D:\\dummy");

        let policy = working_tree.stable_read_policy();

        assert_eq!(
            policy,
            super::StableReadPolicy {
                metadata_retry_attempts: 2,
                volatile_retry_attempts: 2,
            }
        );
    }

    #[test]
    fn working_tree_can_override_stable_read_policy() {
        let working_tree = WorkingTree::new_with_policy(
            "D:\\dummy",
            super::StableReadPolicy {
                metadata_retry_attempts: 5,
                volatile_retry_attempts: 7,
            },
        );

        let policy = working_tree.stable_read_policy();

        assert_eq!(
            policy,
            super::StableReadPolicy {
                metadata_retry_attempts: 5,
                volatile_retry_attempts: 7,
            }
        );
    }

    #[test]
    fn open_stable_file_prefers_snapshot_reader_for_snapshot_preferred_input() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let file_path = root.join("large.bin");
        std::fs::write(&file_path, vec![7u8; 9 * 1024 * 1024]).unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let snapshot_reader = Arc::new(RecordingSnapshotReader {
            result: Ok(b"snapshot-bytes".to_vec()),
            calls: Arc::clone(&calls),
        });
        let working_tree = WorkingTree::new_with_snapshot_reader(
            &root,
            super::StableReadPolicy::default(),
            snapshot_reader,
        );

        let bytes = working_tree.open_stable_file(&file_path).unwrap();

        assert_eq!(bytes, b"snapshot-bytes");
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn open_stable_file_falls_back_when_snapshot_reader_fails() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let file_path = root.join("volatile.txt");
        std::fs::write(&file_path, b"disk-bytes").unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let snapshot_reader = Arc::new(RecordingSnapshotReader {
            result: Err("snapshot unavailable".to_string()),
            calls: Arc::clone(&calls),
        });
        let working_tree = WorkingTree::new_with_snapshot_reader(
            &root,
            super::StableReadPolicy::default(),
            snapshot_reader,
        );

        let bytes = working_tree.open_stable_file(&file_path).unwrap();

        assert_eq!(bytes, b"disk-bytes");
        assert_eq!(calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn volatile_retry_succeeds_after_later_stable_attempt() {
        let stable = super::FileMetadata {
            len: 5,
            modified: std::time::SystemTime::now(),
            created: None,
        };
        let changed = super::FileMetadata {
            len: 6,
            modified: std::time::SystemTime::now(),
            created: None,
        };
        let mut calls = 0usize;

        let bytes = super::read_volatile_with_retry(
            || {
                calls += 1;
                if calls == 1 {
                    Ok((
                        stable.clone(),
                        b"first".to_vec(),
                        changed.clone(),
                        b"second".to_vec(),
                        changed.clone(),
                    ))
                } else {
                    Ok((
                        stable.clone(),
                        b"stable".to_vec(),
                        stable.clone(),
                        b"stable".to_vec(),
                        stable.clone(),
                    ))
                }
            },
            2,
            std::path::Path::new("demo.txt"),
        )
        .unwrap();

        assert_eq!(bytes, b"stable");
    }

    #[test]
    fn volatile_retry_fails_after_budget_exhausted() {
        let stable = super::FileMetadata {
            len: 5,
            modified: std::time::SystemTime::now(),
            created: None,
        };
        let changed = super::FileMetadata {
            len: 6,
            modified: std::time::SystemTime::now(),
            created: None,
        };

        let error = super::read_volatile_with_retry(
            || {
                Ok((
                    stable.clone(),
                    b"first".to_vec(),
                    changed.clone(),
                    b"second".to_vec(),
                    changed.clone(),
                ))
            },
            2,
            std::path::Path::new("demo.txt"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("unstable input"));
    }

    #[test]
    fn volatile_retry_failure_is_classified_as_unstable_input() {
        let stable = super::FileMetadata {
            len: 5,
            modified: std::time::SystemTime::now(),
            created: None,
        };
        let changed = super::FileMetadata {
            len: 6,
            modified: std::time::SystemTime::now(),
            created: None,
        };

        let error = super::read_volatile_with_retry(
            || {
                Ok((
                    stable.clone(),
                    b"first".to_vec(),
                    changed.clone(),
                    b"second".to_vec(),
                    changed.clone(),
                ))
            },
            2,
            std::path::Path::new("demo.txt"),
        )
        .unwrap_err();

        assert!(
            error.to_string().contains("unstable input"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn volatile_retry_rejects_bytes_when_follow_up_observation_changes() {
        let stable = super::FileMetadata {
            len: 5,
            modified: std::time::SystemTime::now(),
            created: None,
        };
        let changed = super::FileMetadata {
            len: 6,
            modified: std::time::SystemTime::now(),
            created: None,
        };
        let mut calls = 0usize;

        let error = super::read_volatile_with_retry(
            || {
                calls += 1;
                let _ = calls == 1;
                Ok((
                    stable.clone(),
                    b"stable".to_vec(),
                    stable.clone(),
                    b"stable".to_vec(),
                    changed.clone(),
                ))
            },
            1,
            std::path::Path::new("demo.txt"),
        )
        .unwrap_err();

        assert!(error.to_string().contains("unstable input"));
    }

    #[test]
    fn custom_stable_read_policy_controls_volatile_retry_budget() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let file_path = root.join("volatile.txt");
        std::fs::write(&file_path, b"start").unwrap();
        let working_tree = WorkingTree::new_with_policy(
            &root,
            super::StableReadPolicy {
                metadata_retry_attempts: 2,
                volatile_retry_attempts: 1,
            },
        );

        let original_read = std::fs::read;
        let original_metadata = std::fs::metadata;
        let mut read_calls = 0usize;

        let error = super::read_volatile_with_retry(
            || {
                read_calls += 1;
                let before_meta = original_metadata(&file_path).unwrap();
                let before = super::FileMetadata {
                    len: before_meta.len(),
                    modified: std::time::SystemTime::now(),
                    created: None,
                };
                let first = original_read(&file_path).unwrap();
                std::fs::write(&file_path, format!("changed-{read_calls}")).unwrap();
                let changed_meta = original_metadata(&file_path).unwrap();
                let after = super::FileMetadata {
                    len: changed_meta.len(),
                    modified: std::time::SystemTime::now(),
                    created: None,
                };
                let second = original_read(&file_path).unwrap();
                Ok((before, first, after.clone(), second, after))
            },
            working_tree.stable_read_policy().volatile_retry_attempts,
            &file_path,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unstable input"));
        assert_eq!(read_calls, 1);
    }

    #[test]
    fn scan_normalizes_names_to_nfc() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let decomposed = "e\u{301}.txt".to_string();
        std::fs::write(root.join(&decomposed), b"hello").unwrap();
        let working_tree = WorkingTree::new(&root);

        let entries = working_tree.scan_dir(&root, false).unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "\u{e9}.txt");
    }

    #[test]
    fn scan_rejects_case_fold_collisions() {
        let working_tree = WorkingTree::new("D:\\dummy");
        let mut seen = std::collections::HashSet::new();
        working_tree
            .validate_case_fold_uniqueness(&mut seen, "Readme")
            .unwrap();
        let error = working_tree
            .validate_case_fold_uniqueness(&mut seen, "README")
            .unwrap_err();

        assert!(
            error.to_string().contains("case-fold"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn read_platform_name_mappings_rejects_directory_path_conflict() {
        let temp = tempdir().unwrap();
        let root = temp.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(root.join(super::LOCAL_CHECKOUT_MAPPING_FILE)).unwrap();
        let working_tree = WorkingTree::new(&root);

        let error = working_tree.read_platform_name_mappings().unwrap_err();

        assert!(
            error
                .to_string()
                .contains("failed to read checkout mapping"),
            "unexpected error: {error:#}"
        );
    }
}
