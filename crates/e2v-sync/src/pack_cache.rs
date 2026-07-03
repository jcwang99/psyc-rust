use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use e2v_store::RemoteBackend;

use crate::pack::PackedObjectLocation;

pub(crate) fn remote_object_bytes_with_pack_cache<R: RemoteBackend>(
    remote: &R,
    loose_object_ids: &std::collections::BTreeSet<String>,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
    control_dir: Option<&Path>,
    object_id: &str,
) -> Result<Option<Vec<u8>>> {
    if loose_object_ids.contains(object_id) {
        return Ok(Some(
            remote.get_physical(&format!("objects/{object_id}.json"))?,
        ));
    }

    let Some(location) = pack_locations.get(object_id) else {
        return Ok(None);
    };
    let physical_ref = location.physical_ref()?;
    let offset = usize::try_from(physical_ref.offset.unwrap_or(0))
        .map_err(|_| anyhow::anyhow!("pack offset is too large to read on this platform"))?;
    let length = usize::try_from(physical_ref.length)
        .map_err(|_| anyhow::anyhow!("pack length is too large to read on this platform"))?;
    let end = offset.saturating_add(length);
    if !pack_cache.contains_key(&physical_ref.container_id) {
        let pack_bytes =
            load_pack_bytes(remote, control_dir, &physical_ref.container_id, end, true)?;
        pack_cache.insert(physical_ref.container_id.clone(), pack_bytes);
    }
    let cached_is_usable = pack_cache
        .get(&physical_ref.container_id)
        .map(|pack_bytes| end <= pack_bytes.len())
        .unwrap_or(false);
    if !cached_is_usable {
        if let Some(control_dir) = control_dir {
            delete_cached_pack_data_bytes(control_dir, &physical_ref.container_id)?;
        }
        let pack_bytes =
            load_pack_bytes(remote, control_dir, &physical_ref.container_id, end, false)?;
        pack_cache.insert(physical_ref.container_id.clone(), pack_bytes);
    }
    let pack_bytes = pack_cache.get(&physical_ref.container_id).unwrap();
    anyhow::ensure!(
        end <= pack_bytes.len(),
        "packed object range out of bounds for {object_id}"
    );
    Ok(Some(pack_bytes[offset..end].to_vec()))
}

fn load_pack_bytes<R: RemoteBackend>(
    remote: &R,
    control_dir: Option<&Path>,
    container_id: &str,
    minimum_len: usize,
    allow_cached_read: bool,
) -> Result<Vec<u8>> {
    if allow_cached_read
        && let Some(control_dir) = control_dir
        && let Some(cached) = read_cached_pack_data_bytes(control_dir, container_id)?
        && cached.len() >= minimum_len
    {
        return Ok(cached);
    }

    let pack_len: usize = remote
        .stat_physical(container_id)?
        .length
        .try_into()
        .map_err(|_| anyhow::anyhow!("pack is too large to read on this platform"))?;
    let pack_bytes = remote.get_physical_range(container_id, 0, pack_len)?;
    if let Some(control_dir) = control_dir {
        overwrite_cached_pack_data_bytes(control_dir, container_id, &pack_bytes)?;
    }
    Ok(pack_bytes)
}

pub(crate) fn cache_pack_data_bytes(
    control_dir: &Path,
    container_id: &str,
    pack_bytes: &[u8],
) -> Result<()> {
    validate_cached_pack_relative_name(container_id)?;
    let cache_path = pack_data_cache_path(control_dir, container_id);
    if cache_path.is_file() {
        return Ok(());
    }
    write_cached_pack_data_bytes(control_dir, container_id, pack_bytes)
}

pub(crate) fn preload_cached_pack_data(
    control_dir: &Path,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
    pack_cache: &mut BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for location in pack_locations.values() {
        let physical_ref = location.physical_ref()?;
        let container_id = &physical_ref.container_id;
        if pack_cache.contains_key(container_id) {
            continue;
        }
        if let Some(bytes) = read_cached_pack_data_bytes(control_dir, container_id)? {
            pack_cache.insert(container_id.clone(), bytes);
        }
    }
    Ok(())
}

pub(crate) fn prune_stale_cached_pack_data(
    control_dir: &Path,
    pack_locations: &BTreeMap<String, PackedObjectLocation>,
) -> Result<()> {
    let cache_root = control_dir.join("cache").join("pack-data");
    if !cache_root.is_dir() {
        return Ok(());
    }

    let live_container_ids = pack_locations
        .values()
        .map(PackedObjectLocation::physical_ref)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .map(|physical_ref| physical_ref.container_id)
        .collect::<std::collections::BTreeSet<_>>();

    let mut stack = vec![cache_root.clone()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let path = entry.path();
            let file_type = entry.file_type()?;
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }

            let relative = match path.strip_prefix(&cache_root) {
                Ok(relative) => relative,
                Err(_) => continue,
            };
            let container_id = match cached_pack_container_id_from_relative_path(relative) {
                Some(container_id) => container_id,
                None => continue,
            };
            if live_container_ids.contains(&container_id) {
                continue;
            }
            let _ = delete_cached_pack_data_bytes(control_dir, &container_id);
        }
    }

    let _ = remove_empty_cache_dirs(&cache_root, &cache_root);
    Ok(())
}

fn overwrite_cached_pack_data_bytes(
    control_dir: &Path,
    container_id: &str,
    pack_bytes: &[u8],
) -> Result<()> {
    validate_cached_pack_relative_name(container_id)?;
    write_cached_pack_data_bytes(control_dir, container_id, pack_bytes)
}

fn read_cached_pack_data_bytes(control_dir: &Path, container_id: &str) -> Result<Option<Vec<u8>>> {
    validate_cached_pack_relative_name(container_id)?;
    let cache_path = pack_data_cache_path(control_dir, container_id);
    match std::fs::read(cache_path) {
        Ok(bytes) => {
            if cached_pack_data_hash_matches(control_dir, container_id, &bytes)? {
                Ok(Some(bytes))
            } else {
                delete_cached_pack_data_bytes(control_dir, container_id)?;
                Ok(None)
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn delete_cached_pack_data_bytes(control_dir: &Path, container_id: &str) -> Result<()> {
    validate_cached_pack_relative_name(container_id)?;
    let cache_path = pack_data_cache_path(control_dir, container_id);
    let hash_path = pack_data_cache_hash_path(control_dir, container_id);
    if let Err(error) = std::fs::remove_file(cache_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    if let Err(error) = std::fs::remove_file(hash_path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    Ok(())
}

fn cached_pack_container_id_from_relative_path(relative: &Path) -> Option<String> {
    let extension = relative.extension()?.to_str()?;
    if extension == "blake3" {
        let mut base = relative.to_path_buf();
        let inner_extension = base.file_stem()?.to_str()?.to_string();
        base.set_extension(inner_extension);
        return normalize_cached_pack_relative_path(&base);
    }
    normalize_cached_pack_relative_path(relative)
}

fn normalize_cached_pack_relative_path(relative: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in relative.components() {
        let std::path::Component::Normal(part) = component else {
            return None;
        };
        parts.push(part.to_str()?);
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

fn pack_data_cache_path(control_dir: &Path, container_id: &str) -> std::path::PathBuf {
    control_dir
        .join("cache")
        .join("pack-data")
        .join(container_id)
}

fn pack_data_cache_hash_path(control_dir: &Path, container_id: &str) -> std::path::PathBuf {
    let cache_path = pack_data_cache_path(control_dir, container_id);
    cache_path.with_extension(format!(
        "{}.blake3",
        cache_path
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("cache")
    ))
}

fn validate_cached_pack_relative_name(value: &str) -> Result<()> {
    let path = Path::new(value);
    anyhow::ensure!(!value.is_empty(), "empty relative path");
    anyhow::ensure!(!path.is_absolute(), "path escapes target directory");
    anyhow::ensure!(
        path.components()
            .all(|component| matches!(component, std::path::Component::Normal(_))),
        "path traversal is not allowed"
    );
    Ok(())
}

fn cached_pack_data_hash_matches(
    control_dir: &Path,
    container_id: &str,
    bytes: &[u8],
) -> Result<bool> {
    let hash_path = pack_data_cache_hash_path(control_dir, container_id);
    let expected = match std::fs::read_to_string(hash_path) {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    Ok(expected.trim() == pack_data_hash(bytes))
}

fn write_cached_pack_data_bytes(
    control_dir: &Path,
    container_id: &str,
    pack_bytes: &[u8],
) -> Result<()> {
    let cache_path = pack_data_cache_path(control_dir, container_id);
    let hash_path = pack_data_cache_hash_path(control_dir, container_id);
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write_bytes(cache_path, pack_bytes)?;
    atomic_write_bytes(hash_path, pack_data_hash(pack_bytes).as_bytes())
}

fn pack_data_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn atomic_write_bytes(path: std::path::PathBuf, bytes: &[u8]) -> Result<()> {
    let temp_path = path.with_extension(format!(
        "{}.tmp",
        path.extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("tmp")
    ));
    std::fs::write(&temp_path, bytes)
        .map_err(anyhow::Error::from)
        .and_then(|_| {
            std::fs::rename(&temp_path, &path)
                .map_err(anyhow::Error::from)
                .map(|_| ())
        })
        .map_err(|error| anyhow::anyhow!(error))
}

fn remove_empty_cache_dirs(root: &Path, current: &Path) -> Result<bool> {
    let mut is_empty = true;
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if !remove_empty_cache_dirs(root, &path)? {
                is_empty = false;
            }
        } else {
            is_empty = false;
        }
    }

    if current != root && is_empty {
        std::fs::remove_dir(current)?;
        return Ok(true);
    }
    Ok(is_empty)
}
