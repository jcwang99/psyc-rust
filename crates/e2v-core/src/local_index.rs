use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

use crate::manifest_store::{ManifestStore, ManifestStoreApi};

const INDEX_DB_FILE: &str = ".e2v/index.sqlite3";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataSearchQuery {
    pub extension: Option<String>,
    pub path_prefix: Option<String>,
    pub min_size: Option<u64>,
    pub max_size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MetadataSearchResult {
    pub path: String,
    pub extension: Option<String>,
    pub size_bytes: u64,
    pub modified_unix_ms: u64,
    pub file_object_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilenameSearchResult {
    pub path: String,
    pub file_name: String,
    pub file_object_id: String,
}

pub fn search_metadata(
    repo_root: &Path,
    branch_token_hex: &str,
    head_snapshot_id: Option<&str>,
    query: &MetadataSearchQuery,
) -> Result<Vec<MetadataSearchResult>> {
    let extension = query
        .extension
        .as_ref()
        .map(|value| value.trim().to_lowercase());
    let path_prefix = query
        .path_prefix
        .as_ref()
        .map(|value| value.trim_matches('/').replace('\\', "/").to_string());
    let min_size = query.min_size.map(i64::try_from).transpose()?;
    let max_size = query.max_size.map(i64::try_from).transpose()?;

    with_local_index_connection(
        repo_root,
        branch_token_hex,
        head_snapshot_id,
        |connection| {
            let mut statement = connection.prepare(
                "SELECT path, extension, size_bytes, modified_unix_ms, file_object_id
             FROM current_files
             WHERE (?1 IS NULL OR extension = ?1)
               AND (?2 IS NULL OR path = ?2 OR path LIKE (?2 || '/%'))
               AND (?3 IS NULL OR size_bytes >= ?3)
               AND (?4 IS NULL OR size_bytes <= ?4)
             ORDER BY path ASC",
            )?;
            let rows = statement.query_map(
                params![extension, path_prefix, min_size, max_size],
                |row| {
                    Ok(MetadataSearchResult {
                        path: row.get(0)?,
                        extension: row.get(1)?,
                        size_bytes: row.get::<_, u64>(2)?,
                        modified_unix_ms: row.get::<_, u64>(3)?,
                        file_object_id: row.get(4)?,
                    })
                },
            )?;

            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(anyhow::Error::from)
        },
    )
}

pub fn search_filenames(
    repo_root: &Path,
    branch_token_hex: &str,
    head_snapshot_id: Option<&str>,
    query_text: &str,
) -> Result<Vec<FilenameSearchResult>> {
    let query_text = query_text.trim();
    if query_text.is_empty() {
        return Ok(Vec::new());
    }
    let needle = query_text.to_lowercase();
    with_local_index_connection(
        repo_root,
        branch_token_hex,
        head_snapshot_id,
        |connection| {
            let mut statement = connection.prepare(
                "SELECT path, file_name, file_object_id
             FROM filename_fts
             WHERE filename_fts MATCH ?1
             ORDER BY path ASC",
            )?;
            let rows = statement.query_map([format!("{needle}*")], |row| {
                Ok(FilenameSearchResult {
                    path: row.get(0)?,
                    file_name: row.get(1)?,
                    file_object_id: row.get(2)?,
                })
            })?;

            rows.collect::<rusqlite::Result<Vec<_>>>()
                .map_err(anyhow::Error::from)
        },
    )
}

fn with_local_index_connection<T>(
    repo_root: &Path,
    branch_token_hex: &str,
    head_snapshot_id: Option<&str>,
    operation: impl Fn(&Connection) -> Result<T>,
) -> Result<T> {
    let db_path = repo_root.join(INDEX_DB_FILE);
    let mut reset_attempted = false;
    loop {
        let outcome = open_index_connection(&db_path).and_then(|connection| {
            bootstrap(&connection)?;
            rebuild_if_needed(&connection, repo_root, branch_token_hex, head_snapshot_id)?;
            operation(&connection)
        });
        match outcome {
            Ok(value) => return Ok(value),
            Err(error) if !reset_attempted && is_recoverable_local_index_error(&error) => {
                reset_attempted = true;
                reset_index_database(&db_path)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn open_index_connection(db_path: &Path) -> Result<Connection> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create index directory {}", parent.display()))?;
    }
    Connection::open(db_path)
        .with_context(|| format!("failed to open index database {}", db_path.display()))
}

fn is_recoverable_local_index_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<rusqlite::Error>(),
            Some(rusqlite::Error::SqliteFailure(sql_error, _))
                if matches!(
                    sql_error.code,
                    rusqlite::ffi::ErrorCode::CannotOpen
                        | rusqlite::ffi::ErrorCode::DatabaseCorrupt
                        | rusqlite::ffi::ErrorCode::NotADatabase
                )
        )
    })
}

fn reset_index_database(db_path: &Path) -> Result<()> {
    remove_path_if_exists(db_path)?;
    remove_path_if_exists(&sqlite_sidecar_path(db_path, "-wal"))?;
    remove_path_if_exists(&sqlite_sidecar_path(db_path, "-shm"))?;
    Ok(())
}

fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    let mut os_string = db_path.as_os_str().to_os_string();
    os_string.push(suffix);
    PathBuf::from(os_string)
}

fn remove_path_if_exists(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path)?,
        Ok(_) => std::fs::remove_file(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

fn bootstrap(connection: &Connection) -> Result<()> {
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "secure_delete", "ON")?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS index_meta (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS current_files (
             path TEXT PRIMARY KEY,
             file_name TEXT NOT NULL,
             extension TEXT,
             size_bytes INTEGER NOT NULL,
             modified_unix_ms INTEGER NOT NULL,
             file_object_id TEXT NOT NULL
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS filename_fts
         USING fts5(path, file_name, file_object_id, tokenize = 'unicode61');",
    )?;
    Ok(())
}

fn rebuild_if_needed(
    connection: &Connection,
    repo_root: &Path,
    branch_token_hex: &str,
    head_snapshot_id: Option<&str>,
) -> Result<()> {
    let indexed_branch: Option<String> = connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'branch_token_hex'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let indexed_head: Option<String> = connection
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'head_snapshot_id'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let requested_head = head_snapshot_id.unwrap_or("");

    if indexed_branch.as_deref() == Some(branch_token_hex)
        && indexed_head.as_deref() == Some(requested_head)
    {
        return Ok(());
    }

    let transaction = connection.unchecked_transaction()?;
    let next_entries = load_snapshot_file_rows(repo_root, head_snapshot_id)?;
    let current_entries = load_indexed_file_rows(&transaction)?;
    sync_index_rows(&transaction, &current_entries, &next_entries)?;

    transaction.execute(
        "INSERT INTO index_meta(key, value) VALUES('branch_token_hex', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![branch_token_hex],
    )?;
    transaction.execute(
        "INSERT INTO index_meta(key, value) VALUES('head_snapshot_id', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![requested_head],
    )?;
    transaction.commit()?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexedFileRow {
    path: String,
    file_name: String,
    extension: Option<String>,
    size_bytes: u64,
    modified_unix_ms: u64,
    file_object_id: String,
}

fn load_snapshot_file_rows(
    repo_root: &Path,
    head_snapshot_id: Option<&str>,
) -> Result<BTreeMap<String, IndexedFileRow>> {
    let Some(head_snapshot_id) = head_snapshot_id else {
        return Ok(BTreeMap::new());
    };
    let store = ManifestStore::new(repo_root);
    let entries = store.walk_tree(head_snapshot_id)?;
    let read_service = crate::facade::RepositoryFacade::new().read_service(repo_root)?;
    let snapshot = read_service.open_snapshot(head_snapshot_id)?;
    let mut rows = BTreeMap::new();
    for entry in entries {
        let path = entry.path;
        let (_, file_name) = path
            .rsplit_once('/')
            .map(|(parent, name)| (Some(parent), name))
            .unwrap_or((None, path.as_str()));
        let file_name = file_name.to_lowercase();
        let extension = file_name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_lowercase());
        let file = read_service.open_file(&snapshot, &path)?;
        let manifest_file = store.get_file_metadata(&file.file_object_id)?;
        rows.insert(
            path.clone(),
            IndexedFileRow {
                path,
                file_name,
                extension,
                size_bytes: manifest_file.file_size,
                modified_unix_ms: manifest_file.modified_unix_ms,
                file_object_id: file.file_object_id,
            },
        );
    }
    Ok(rows)
}

fn load_indexed_file_rows(connection: &Connection) -> Result<BTreeMap<String, IndexedFileRow>> {
    let mut statement = connection.prepare(
        "SELECT path, file_name, extension, size_bytes, modified_unix_ms, file_object_id
         FROM current_files
         ORDER BY path ASC",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(IndexedFileRow {
            path: row.get(0)?,
            file_name: row.get(1)?,
            extension: row.get(2)?,
            size_bytes: row.get::<_, u64>(3)?,
            modified_unix_ms: row.get::<_, u64>(4)?,
            file_object_id: row.get(5)?,
        })
    })?;
    let rows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|row| (row.path.clone(), row))
        .collect::<BTreeMap<_, _>>())
}

fn sync_index_rows(
    connection: &Connection,
    current_entries: &BTreeMap<String, IndexedFileRow>,
    next_entries: &BTreeMap<String, IndexedFileRow>,
) -> Result<()> {
    {
        let mut delete_current = connection.prepare("DELETE FROM current_files WHERE path = ?1")?;
        let mut delete_fts = connection.prepare("DELETE FROM filename_fts WHERE path = ?1")?;
        for path in current_entries.keys() {
            if !next_entries.contains_key(path) {
                delete_current.execute([path])?;
                delete_fts.execute([path])?;
            }
        }
    }

    {
        let mut upsert_current = connection.prepare(
            "INSERT INTO current_files (
                 path, file_name, extension, size_bytes, modified_unix_ms, file_object_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(path) DO UPDATE SET
                 file_name = excluded.file_name,
                 extension = excluded.extension,
                 size_bytes = excluded.size_bytes,
                 modified_unix_ms = excluded.modified_unix_ms,
                 file_object_id = excluded.file_object_id",
        )?;
        let mut delete_fts = connection.prepare("DELETE FROM filename_fts WHERE path = ?1")?;
        let mut insert_fts = connection.prepare(
            "INSERT INTO filename_fts(path, file_name, file_object_id) VALUES (?1, ?2, ?3)",
        )?;
        for (path, next) in next_entries {
            if current_entries.get(path) == Some(next) {
                continue;
            }
            upsert_current.execute(params![
                next.path,
                next.file_name,
                next.extension,
                i64::try_from(next.size_bytes)?,
                i64::try_from(next.modified_unix_ms)?,
                next.file_object_id
            ])?;
            delete_fts.execute([path])?;
            insert_fts.execute(params![next.path, next.file_name, next.file_object_id])?;
        }
    }

    Ok(())
}
