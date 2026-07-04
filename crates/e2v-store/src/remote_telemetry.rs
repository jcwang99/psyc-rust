use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteOperationKind {
    Read,
    ReadRange,
    Write,
    WriteIfAbsent,
    Stat,
    Exists,
    List,
    Delete,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteOperationStats {
    pub requests: u64,
    pub failed_requests: u64,
    pub duration_ms: u128,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub listed_entries: u64,
}

impl RemoteOperationStats {
    fn record(
        &mut self,
        elapsed: Duration,
        bytes_sent: u64,
        bytes_received: u64,
        listed_entries: u64,
        success: bool,
    ) {
        self.requests = self.requests.saturating_add(1);
        if !success {
            self.failed_requests = self.failed_requests.saturating_add(1);
        }
        self.duration_ms = self.duration_ms.saturating_add(elapsed.as_millis());
        self.bytes_sent = self.bytes_sent.saturating_add(bytes_sent);
        self.bytes_received = self.bytes_received.saturating_add(bytes_received);
        self.listed_entries = self.listed_entries.saturating_add(listed_entries);
    }

    fn subtract(&self, earlier: &Self) -> Self {
        Self {
            requests: self.requests.saturating_sub(earlier.requests),
            failed_requests: self.failed_requests.saturating_sub(earlier.failed_requests),
            duration_ms: self.duration_ms.saturating_sub(earlier.duration_ms),
            bytes_sent: self.bytes_sent.saturating_sub(earlier.bytes_sent),
            bytes_received: self.bytes_received.saturating_sub(earlier.bytes_received),
            listed_entries: self.listed_entries.saturating_sub(earlier.listed_entries),
        }
    }

    fn is_zero(&self) -> bool {
        self.requests == 0
            && self.failed_requests == 0
            && self.duration_ms == 0
            && self.bytes_sent == 0
            && self.bytes_received == 0
            && self.listed_entries == 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemotePathStats {
    pub requests: u64,
    pub failed_requests: u64,
    pub duration_ms: u128,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub listed_entries: u64,
    pub operations: BTreeMap<RemoteOperationKind, RemoteOperationStats>,
}

impl RemotePathStats {
    fn record(
        &mut self,
        kind: RemoteOperationKind,
        elapsed: Duration,
        bytes_sent: u64,
        bytes_received: u64,
        listed_entries: u64,
        success: bool,
    ) {
        self.requests = self.requests.saturating_add(1);
        if !success {
            self.failed_requests = self.failed_requests.saturating_add(1);
        }
        self.duration_ms = self.duration_ms.saturating_add(elapsed.as_millis());
        self.bytes_sent = self.bytes_sent.saturating_add(bytes_sent);
        self.bytes_received = self.bytes_received.saturating_add(bytes_received);
        self.listed_entries = self.listed_entries.saturating_add(listed_entries);
        self.operations
            .entry(kind)
            .or_default()
            .record(elapsed, bytes_sent, bytes_received, listed_entries, success);
    }

    fn subtract(&self, earlier: &Self) -> Self {
        let mut operations = BTreeMap::new();
        for kind in self.operations.keys().chain(earlier.operations.keys()) {
            if operations.contains_key(kind) {
                continue;
            }
            let current = self.operations.get(kind).cloned().unwrap_or_default();
            let previous = earlier.operations.get(kind).cloned().unwrap_or_default();
            let diff = current.subtract(&previous);
            if !diff.is_zero() {
                operations.insert(*kind, diff);
            }
        }

        Self {
            requests: self.requests.saturating_sub(earlier.requests),
            failed_requests: self.failed_requests.saturating_sub(earlier.failed_requests),
            duration_ms: self.duration_ms.saturating_sub(earlier.duration_ms),
            bytes_sent: self.bytes_sent.saturating_sub(earlier.bytes_sent),
            bytes_received: self.bytes_received.saturating_sub(earlier.bytes_received),
            listed_entries: self.listed_entries.saturating_sub(earlier.listed_entries),
            operations,
        }
    }

    fn is_zero(&self) -> bool {
        self.requests == 0
            && self.failed_requests == 0
            && self.duration_ms == 0
            && self.bytes_sent == 0
            && self.bytes_received == 0
            && self.listed_entries == 0
            && self.operations.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteTelemetrySnapshot {
    pub total_requests: u64,
    pub failed_requests: u64,
    pub duration_ms: u128,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    pub listed_entries: u64,
    pub operations: BTreeMap<RemoteOperationKind, RemoteOperationStats>,
    pub paths: BTreeMap<String, RemotePathStats>,
}

impl RemoteTelemetrySnapshot {
    pub fn unique_path_count(&self) -> usize {
        self.paths.len()
    }

    pub fn diff(&self, earlier: &Self) -> Self {
        let mut operations = BTreeMap::new();
        for kind in self.operations.keys().chain(earlier.operations.keys()) {
            if operations.contains_key(kind) {
                continue;
            }
            let current = self.operations.get(kind).cloned().unwrap_or_default();
            let previous = earlier.operations.get(kind).cloned().unwrap_or_default();
            let diff = current.subtract(&previous);
            if !diff.is_zero() {
                operations.insert(*kind, diff);
            }
        }

        let mut paths = BTreeMap::new();
        for path in self.paths.keys().chain(earlier.paths.keys()) {
            if paths.contains_key(path) {
                continue;
            }
            let current = self.paths.get(path).cloned().unwrap_or_default();
            let previous = earlier.paths.get(path).cloned().unwrap_or_default();
            let diff = current.subtract(&previous);
            if !diff.is_zero() {
                paths.insert(path.clone(), diff);
            }
        }

        Self {
            total_requests: self.total_requests.saturating_sub(earlier.total_requests),
            failed_requests: self.failed_requests.saturating_sub(earlier.failed_requests),
            duration_ms: self.duration_ms.saturating_sub(earlier.duration_ms),
            bytes_sent: self.bytes_sent.saturating_sub(earlier.bytes_sent),
            bytes_received: self.bytes_received.saturating_sub(earlier.bytes_received),
            listed_entries: self.listed_entries.saturating_sub(earlier.listed_entries),
            operations,
            paths,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RemoteTelemetryHandle {
    inner: Arc<Mutex<RemoteTelemetrySnapshot>>,
}

impl RemoteTelemetryHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> RemoteTelemetrySnapshot {
        self.inner.lock().unwrap().clone()
    }

    pub fn record(
        &self,
        kind: RemoteOperationKind,
        path: &str,
        elapsed: Duration,
        bytes_sent: u64,
        bytes_received: u64,
        listed_entries: u64,
        success: bool,
    ) {
        let mut snapshot = self.inner.lock().unwrap();
        snapshot.total_requests = snapshot.total_requests.saturating_add(1);
        if !success {
            snapshot.failed_requests = snapshot.failed_requests.saturating_add(1);
        }
        snapshot.duration_ms = snapshot.duration_ms.saturating_add(elapsed.as_millis());
        snapshot.bytes_sent = snapshot.bytes_sent.saturating_add(bytes_sent);
        snapshot.bytes_received = snapshot.bytes_received.saturating_add(bytes_received);
        snapshot.listed_entries = snapshot.listed_entries.saturating_add(listed_entries);
        snapshot
            .operations
            .entry(kind)
            .or_default()
            .record(elapsed, bytes_sent, bytes_received, listed_entries, success);
        snapshot
            .paths
            .entry(path.to_string())
            .or_default()
            .record(kind, elapsed, bytes_sent, bytes_received, listed_entries, success);
    }
}
