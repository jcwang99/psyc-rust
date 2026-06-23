use std::path::Path;

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
    let db_path = repo_root.join(INDEX_DB_FILE);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create index directory {}", parent.display()))?;
    }
    let connection = Connection::open(&db_path)
        .with_context(|| format!("failed to open index database {}", db_path.display()))?;
    bootstrap(&connection)?;
    rebuild_if_needed(&connection, repo_root, branch_token_hex, head_snapshot_id)?;

    let extension = query.extension.as_ref().map(|value| value.trim().to_lowercase());
    let path_prefix = query.path_prefix.as_ref().map(|value| {
        value.trim_matches('/')
            .replace('\\', "/")
            .to_string()
    });
    let min_size = query.min_size.map(i64::try_from).transpose()?;
    let max_size = query.max_size.map(i64::try_from).transpose()?;

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
}

pub fn search_filenames(
    repo_root: &Path,
    branch_token_hex: &str,
    head_snapshot_id: Option<&str>,
    query_text: &str,
) -> Result<Vec<FilenameSearchResult>> {
    let db_path = repo_root.join(INDEX_DB_FILE);
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create index directory {}", parent.display()))?;
    }
    let connection = Connection::open(&db_path)
        .with_context(|| format!("failed to open index database {}", db_path.display()))?;
    bootstrap(&connection)?;
    rebuild_if_needed(&connection, repo_root, branch_token_hex, head_snapshot_id)?;

    let query_text = query_text.trim();
    if query_text.is_empty() {
        return Ok(Vec::new());
    }
    let needle = query_text.to_lowercase();
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
    transaction.execute("DELETE FROM current_files", [])?;
    transaction.execute("DELETE FROM filename_fts", [])?;

    if let Some(head_snapshot_id) = head_snapshot_id {
        let store = ManifestStore::new(repo_root);
        let entries = store.walk_tree(head_snapshot_id)?;
        let read_service = crate::facade::RepositoryFacade::new().read_service(repo_root)?;
        let snapshot = read_service.open_snapshot(head_snapshot_id)?;
        let mut insert = transaction.prepare(
            "INSERT INTO current_files (
                 path, file_name, extension, size_bytes, modified_unix_ms, file_object_id
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )?;
        let mut insert_fts = transaction.prepare(
            "INSERT INTO filename_fts(path, file_name, file_object_id) VALUES (?1, ?2, ?3)",
        )?;
        for entry in entries {
            let path = entry.path;
            let (_, file_name) = path
                .rsplit_once('/')
                .map(|(parent, name)| (Some(parent), name))
                .unwrap_or((None, path.as_str()));
            let extension = file_name.rsplit_once('.').map(|(_, ext)| ext.to_lowercase());
            let file = read_service.open_file(&snapshot, &path)?;
            let manifest_file = store.get_file(&file.file_object_id)?;
            insert.execute(params![
                path,
                file_name.to_lowercase(),
                extension,
                i64::try_from(manifest_file.file_size)?,
                i64::try_from(manifest_file.modified_unix_ms)?,
                file.file_object_id
            ])?;
            insert_fts.execute(params![
                path,
                file_name.to_lowercase(),
                file.file_object_id
            ])?;
        }
    }

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
