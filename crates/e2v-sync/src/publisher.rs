use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use e2v_store::{BackendCapability, CasResult, EncryptedRef, LayoutRootVersion, RemoteBackend};

use crate::journal::{OperationJournal, validate_sync_identifier};
use crate::remote_markers::{
    INTENT_EXPIRY_HOURS, RemoteWriteIntentMarker, RemoteWriterLeaseMarker,
    build_write_intent_marker, build_writer_lease_marker, marker_is_fresh_at,
    observe_remote_now_with_probe, remote_observed_at_unix_ms, renew_write_intent_marker,
    renew_writer_lease_marker, system_time_to_unix_ms, writer_id_for_operation,
};
use crate::transaction::{PublishPlan, PublishSession, PublishedObject};

const HEARTBEAT_RENEWAL_INTERVAL: Duration = Duration::from_secs(10 * 60);

thread_local! {
    static HEARTBEAT_NOW_OVERRIDE: RefCell<Option<Arc<Mutex<SystemTime>>>> =
        const { RefCell::new(None) };
}

fn heartbeat_now() -> SystemTime {
    HEARTBEAT_NOW_OVERRIDE.with(|override_clock| {
        override_clock
            .borrow()
            .as_ref()
            .map(|clock| *clock.lock().unwrap())
            .unwrap_or_else(SystemTime::now)
    })
}

pub(crate) fn override_heartbeat_time_for_test(
    current_time: Arc<Mutex<SystemTime>>,
) -> HeartbeatTimeOverrideGuard {
    let previous =
        HEARTBEAT_NOW_OVERRIDE.with(|override_clock| override_clock.replace(Some(current_time)));
    HeartbeatTimeOverrideGuard { previous }
}

pub struct HeartbeatTimeOverrideGuard {
    previous: Option<Arc<Mutex<SystemTime>>>,
}

impl Drop for HeartbeatTimeOverrideGuard {
    fn drop(&mut self) {
        HEARTBEAT_NOW_OVERRIDE.with(|override_clock| {
            override_clock.replace(self.previous.take());
        });
    }
}

pub trait TransactionPublisher {
    fn begin(&self, plan: PublishPlan) -> Result<PublishSession>;
    fn record_uploaded(&self, session: &PublishSession, object: PublishedObject) -> Result<()>;
    fn heartbeat(&self, session: &PublishSession) -> Result<()>;
    fn publish_layout_if_needed(&self, session: &PublishSession) -> Result<LayoutRootVersion>;
    fn pre_commit_verify(&self, session: &PublishSession) -> Result<()>;
    fn publish_ref(&self, session: &PublishSession, next: EncryptedRef) -> Result<CasResult>;
    fn complete(&self, session: PublishSession) -> Result<()>;
}

#[derive(Clone)]
pub struct SimpleTransactionPublisher<R: RemoteBackend> {
    capability: BackendCapability,
    journal: OperationJournal,
    remote_backend: R,
    heartbeat_observed_at_local: Arc<Mutex<BTreeMap<String, SystemTime>>>,
}

impl<R: RemoteBackend> SimpleTransactionPublisher<R> {
    pub fn new(
        capability: BackendCapability,
        journal: OperationJournal,
        remote_backend: R,
    ) -> Self {
        Self {
            capability,
            journal,
            remote_backend,
            heartbeat_observed_at_local: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn begin_allowing_risky_single_writer(
        &self,
        plan: PublishPlan,
        allow_risky_single_writer: bool,
    ) -> Result<PublishSession> {
        validate_sync_identifier("branch token", &plan.target_branch_token)?;
        let advertised_writer_mode = self.capability.writer_mode();
        let effective_writer_mode = if allow_risky_single_writer
            && advertised_writer_mode == e2v_store::WriterMode::SingleWriter
            && self.capability.push_write_mode() == e2v_store::WriterMode::ReadOnly
        {
            e2v_store::WriterMode::SingleWriter
        } else {
            self.capability.push_write_mode()
        };
        if advertised_writer_mode == e2v_store::WriterMode::SingleWriter
            && effective_writer_mode == e2v_store::WriterMode::ReadOnly
        {
            anyhow::bail!("risky single-writer backend capabilities are disabled by default");
        }
        anyhow::ensure!(
            effective_writer_mode != e2v_store::WriterMode::ReadOnly,
            "read-only backend capabilities cannot publish"
        );
        let writer_mode = effective_writer_mode;
        let lease_path = if writer_mode == e2v_store::WriterMode::SingleWriter
            && self.capability.supports_atomic_create_if_absent
        {
            let lease_path = format!("leases/{}.lock", plan.target_branch_token);
            let initial_lease_bytes =
                serde_json::to_vec(&build_writer_lease_marker(&plan, 0, 1, 1))?;
            if !self
                .remote_backend
                .put_physical_if_absent(&lease_path, initial_lease_bytes.as_slice())?
            {
                let observed_now = observe_remote_now_with_probe(
                    &self.remote_backend,
                    ".e2v/publish-remote-time.probe",
                )?;
                let lease_is_fresh = marker_is_fresh_at(
                    &self.remote_backend,
                    &lease_path,
                    observed_now,
                    INTENT_EXPIRY_HOURS,
                )?;
                if lease_is_fresh {
                    anyhow::bail!(
                        "writer lease acquisition failed for {}",
                        plan.target_branch_token
                    );
                }
                let lease_bytes = self.remote_backend.get_physical(&lease_path)?;
                if let Ok(existing) =
                    serde_json::from_slice::<RemoteWriterLeaseMarker>(&lease_bytes)
                    && existing.target_branch_token != plan.target_branch_token
                {
                    anyhow::bail!(
                        "writer lease acquisition failed for {}",
                        plan.target_branch_token
                    );
                }
                self.remote_backend.delete_physical(&lease_path)?;
                let reacquired = self
                    .remote_backend
                    .put_physical_if_absent(&lease_path, initial_lease_bytes.as_slice())?;
                anyhow::ensure!(
                    reacquired,
                    "writer lease acquisition failed for {}",
                    plan.target_branch_token
                );
            }
            let observed_at = remote_observed_at_unix_ms(&self.remote_backend, &lease_path)?;
            self.remote_backend.put_physical(
                &lease_path,
                serde_json::to_vec(&build_writer_lease_marker(&plan, observed_at, 1, 1))?
                    .as_slice(),
            )?;
            Some(lease_path)
        } else {
            None
        };
        let active_intent_path = format!("transactions/active/{}.intent", plan.operation_id.value);
        self.remote_backend.put_physical(
            &active_intent_path,
            serde_json::to_vec(&build_write_intent_marker(&plan, 0, 1))?.as_slice(),
        )?;
        let observed_at = remote_observed_at_unix_ms(&self.remote_backend, &active_intent_path)?;
        if self.capability.supports_reliable_remote_time
            || self.capability.supports_object_generation_or_etag
        {
            self.remote_backend.put_physical(
                &active_intent_path,
                serde_json::to_vec(&build_write_intent_marker(&plan, observed_at, 1))?.as_slice(),
            )?;
        }
        let session = PublishSession {
            operation_id: plan.operation_id,
            target_branch_token: plan.target_branch_token,
            expected_ref_version: plan.expected_ref_version,
            planned_snapshot_id: plan.planned_snapshot_id,
            writer_mode,
            started_at_remote_unix_ms: observed_at,
            observed_layout_root_generation: None,
            next_layout_root: None,
            next_layout_root_bytes: None,
            active_intent_path,
            lease_path,
        };
        self.record_heartbeat_observed_now(&session.operation_id, heartbeat_now());
        Ok(session)
    }

    fn renew_intent_if_needed(
        &self,
        path: &str,
        intent: &RemoteWriteIntentMarker,
    ) -> Result<RemoteWriteIntentMarker> {
        let observed_now =
            observe_remote_now_with_probe(&self.remote_backend, ".e2v/publish-remote-time.probe")?;
        if marker_is_fresh_at(
            &self.remote_backend,
            path,
            observed_now,
            INTENT_EXPIRY_HOURS,
        )? {
            return Ok(intent.clone());
        }
        let observed_at = system_time_to_unix_ms(observed_now)?;
        let renewed = renew_write_intent_marker(intent, observed_at);
        self.remote_backend
            .put_physical(path, serde_json::to_vec(&renewed)?.as_slice())?;
        Ok(renewed)
    }

    fn load_or_recover_intent_marker(
        &self,
        path: &str,
        session: &PublishSession,
        phase: &str,
    ) -> Result<RemoteWriteIntentMarker> {
        let intent_bytes = self.remote_backend.get_physical(path)?;
        anyhow::ensure!(
            !intent_bytes.is_empty(),
            "{phase}: active intent missing or empty"
        );
        match serde_json::from_slice::<RemoteWriteIntentMarker>(&intent_bytes) {
            Ok(intent_marker) => {
                validate_intent_marker_for_session(&intent_marker, session, phase)?;
                Ok(intent_marker)
            }
            Err(error) => {
                let observed_now = observe_remote_now_with_probe(
                    &self.remote_backend,
                    ".e2v/publish-remote-time.probe",
                )?;
                if marker_is_fresh_at(
                    &self.remote_backend,
                    path,
                    observed_now,
                    INTENT_EXPIRY_HOURS,
                )? {
                    return Err(error).context(format!("{phase}: invalid active intent marker"));
                }
                let observed_at = system_time_to_unix_ms(observed_now)?;
                let plan = PublishPlan {
                    operation_id: session.operation_id.clone(),
                    target_branch_token: session.target_branch_token.clone(),
                    expected_ref_version: session.expected_ref_version,
                    planned_snapshot_id: session.planned_snapshot_id.clone(),
                    writer_mode: session.writer_mode.clone(),
                };
                let mut recovered = build_write_intent_marker(&plan, observed_at, 1);
                recovered.started_at_remote_unix_ms = session.started_at_remote_unix_ms;
                self.remote_backend
                    .put_physical(path, serde_json::to_vec(&recovered)?.as_slice())?;
                Ok(recovered)
            }
        }
    }

    fn record_heartbeat_observed_now(
        &self,
        operation_id: &crate::journal::OperationId,
        now: SystemTime,
    ) {
        self.heartbeat_observed_at_local
            .lock()
            .unwrap()
            .insert(operation_id.value.clone(), now);
    }

    fn should_renew_heartbeat(
        &self,
        operation_id: &crate::journal::OperationId,
        now: SystemTime,
    ) -> bool {
        let observed = self.heartbeat_observed_at_local.lock().unwrap();
        let Some(last_observed) = observed.get(&operation_id.value) else {
            return true;
        };
        now.duration_since(*last_observed)
            .map(|elapsed| elapsed >= HEARTBEAT_RENEWAL_INTERVAL)
            .unwrap_or(false)
    }

    fn clear_heartbeat_state(&self, operation_id: &crate::journal::OperationId) {
        self.heartbeat_observed_at_local
            .lock()
            .unwrap()
            .remove(&operation_id.value);
    }

    pub(crate) fn heartbeat_if_needed(&self, session: &PublishSession) -> Result<()> {
        let local_now = heartbeat_now();
        if !self.should_renew_heartbeat(&session.operation_id, local_now) {
            return Ok(());
        }
        TransactionPublisher::heartbeat(self, session)
    }

    fn renew_lease_if_needed(
        &self,
        path: &str,
        lease: &RemoteWriterLeaseMarker,
    ) -> Result<RemoteWriterLeaseMarker> {
        let observed_now =
            observe_remote_now_with_probe(&self.remote_backend, ".e2v/publish-remote-time.probe")?;
        if marker_is_fresh_at(
            &self.remote_backend,
            path,
            observed_now,
            INTENT_EXPIRY_HOURS,
        )? {
            return Ok(lease.clone());
        }
        let observed_at = system_time_to_unix_ms(observed_now)?;
        let renewed = renew_writer_lease_marker(lease, observed_at);
        self.remote_backend
            .put_physical(path, serde_json::to_vec(&renewed)?.as_slice())?;
        Ok(renewed)
    }

    fn load_or_recover_lease_marker(
        &self,
        path: &str,
        session: &PublishSession,
        phase: &str,
    ) -> Result<RemoteWriterLeaseMarker> {
        let lease_bytes = self.remote_backend.get_physical(path)?;
        anyhow::ensure!(
            !lease_bytes.is_empty(),
            "{phase}: writer lease missing or empty"
        );
        match serde_json::from_slice::<RemoteWriterLeaseMarker>(&lease_bytes) {
            Ok(lease_marker) => {
                validate_lease_marker_for_session(&lease_marker, session, phase)?;
                Ok(lease_marker)
            }
            Err(error) => {
                let observed_now = observe_remote_now_with_probe(
                    &self.remote_backend,
                    ".e2v/publish-remote-time.probe",
                )?;
                if marker_is_fresh_at(
                    &self.remote_backend,
                    path,
                    observed_now,
                    INTENT_EXPIRY_HOURS,
                )? {
                    return Err(error).context(format!("{phase}: invalid writer lease marker"));
                }
                let observed_at = system_time_to_unix_ms(observed_now)?;
                let plan = PublishPlan {
                    operation_id: session.operation_id.clone(),
                    target_branch_token: session.target_branch_token.clone(),
                    expected_ref_version: session.expected_ref_version,
                    planned_snapshot_id: None,
                    writer_mode: session.writer_mode.clone(),
                };
                let recovered = build_writer_lease_marker(&plan, observed_at, 1, 1);
                self.remote_backend
                    .put_physical(path, serde_json::to_vec(&recovered)?.as_slice())?;
                Ok(recovered)
            }
        }
    }
}

fn validate_intent_marker_for_session(
    intent_marker: &RemoteWriteIntentMarker,
    session: &PublishSession,
    phase: &str,
) -> Result<()> {
    anyhow::ensure!(
        intent_marker.operation_id == session.operation_id.value,
        "{phase}: active intent belongs to another operation"
    );
    anyhow::ensure!(
        intent_marker.writer_id == writer_id_for_operation(&session.operation_id),
        "{phase}: active intent belongs to another writer"
    );
    anyhow::ensure!(
        intent_marker.target_branch_token == session.target_branch_token,
        "{phase}: active intent belongs to another branch"
    );
    anyhow::ensure!(
        intent_marker.expected_ref_version == session.expected_ref_version,
        "{phase}: active intent expected ref version does not match session"
    );
    anyhow::ensure!(
        intent_marker.planned_snapshot_id == session.planned_snapshot_id,
        "{phase}: active intent planned snapshot does not match session"
    );
    Ok(())
}

fn validate_lease_marker_for_session(
    lease_marker: &RemoteWriterLeaseMarker,
    session: &PublishSession,
    phase: &str,
) -> Result<()> {
    anyhow::ensure!(
        lease_marker.operation_id == session.operation_id.value,
        "{phase}: writer lease belongs to another operation"
    );
    anyhow::ensure!(
        lease_marker.writer_id == writer_id_for_operation(&session.operation_id),
        "{phase}: writer lease belongs to another writer"
    );
    anyhow::ensure!(
        lease_marker.target_branch_token == session.target_branch_token,
        "{phase}: writer lease belongs to another branch"
    );
    Ok(())
}

impl<R: RemoteBackend> TransactionPublisher for SimpleTransactionPublisher<R> {
    fn begin(&self, plan: PublishPlan) -> Result<PublishSession> {
        self.begin_allowing_risky_single_writer(plan, false)
    }

    fn record_uploaded(&self, session: &PublishSession, object: PublishedObject) -> Result<()> {
        self.journal.record_uploaded(
            &session.operation_id,
            &object.object_id,
            &object.object_type,
        )
    }

    fn heartbeat(&self, session: &PublishSession) -> Result<()> {
        let intent = self.load_or_recover_intent_marker(
            &session.active_intent_path,
            session,
            "heartbeat failed",
        )?;
        let lease = if let Some(lease_path) = &session.lease_path {
            Some((
                lease_path.as_str(),
                self.load_or_recover_lease_marker(lease_path, session, "heartbeat failed")?,
            ))
        } else {
            None
        };
        let local_now = heartbeat_now();
        let observed_now =
            observe_remote_now_with_probe(&self.remote_backend, ".e2v/publish-remote-time.probe")?;
        let observed_at = system_time_to_unix_ms(observed_now)?;
        let renewed_intent = renew_write_intent_marker(&intent, observed_at);
        self.remote_backend.put_physical(
            &session.active_intent_path,
            serde_json::to_vec(&renewed_intent)?.as_slice(),
        )?;

        if let Some((lease_path, lease)) = lease {
            let renewed_lease = renew_writer_lease_marker(&lease, observed_at);
            self.remote_backend
                .put_physical(lease_path, serde_json::to_vec(&renewed_lease)?.as_slice())?;
        }
        self.record_heartbeat_observed_now(&session.operation_id, local_now);
        Ok(())
    }

    fn publish_layout_if_needed(&self, session: &PublishSession) -> Result<LayoutRootVersion> {
        if let Some(next_layout_root) = &session.next_layout_root {
            let expected_generation = match session.observed_layout_root_generation {
                Some(generation) => generation,
                None => self.remote_backend.read_layout_root()?.generation,
            };
            let result = self
                .remote_backend
                .compare_and_swap_layout_root(expected_generation, next_layout_root.clone())?;
            anyhow::ensure!(
                result.applied,
                "layout root publish failed: generation changed before publish"
            );
            Ok(next_layout_root.generation)
        } else {
            match session.observed_layout_root_generation {
                Some(generation) => Ok(generation),
                None => Ok(self.remote_backend.read_layout_root()?.generation),
            }
        }
    }

    fn pre_commit_verify(&self, session: &PublishSession) -> Result<()> {
        let intent = self.load_or_recover_intent_marker(
            &session.active_intent_path,
            session,
            "pre-commit verify failed",
        )?;
        let local_now = heartbeat_now();
        let should_renew = self.should_renew_heartbeat(&session.operation_id, local_now);
        if should_renew {
            let _intent = self.renew_intent_if_needed(&session.active_intent_path, &intent)?;
        }
        if let Some(expected_ref_version) = session.expected_ref_version {
            validate_sync_identifier("branch token", &session.target_branch_token)?;
            let current = self.remote_backend.read_ref(&e2v_store::RefToken::new(
                session.target_branch_token.clone(),
            ))?;
            let current_version = current
                .as_ref()
                .map(|stored_ref| stored_ref.version.value)
                .context("pre-commit verify failed: remote ref missing")?;
            anyhow::ensure!(
                current_version == expected_ref_version,
                "pre-commit verify failed: remote ref version changed"
            );
        }
        if let Some(lease_path) = &session.lease_path {
            let lease =
                self.load_or_recover_lease_marker(lease_path, session, "pre-commit verify failed")?;
            if should_renew {
                let _lease = self.renew_lease_if_needed(lease_path, &lease)?;
            }
        }
        if should_renew {
            self.record_heartbeat_observed_now(&session.operation_id, local_now);
        }
        Ok(())
    }

    fn publish_ref(&self, session: &PublishSession, next: EncryptedRef) -> Result<CasResult> {
        validate_sync_identifier("branch token", &session.target_branch_token)?;
        self.remote_backend.compare_and_swap_ref(
            &e2v_store::RefToken::new(session.target_branch_token.clone()),
            session
                .expected_ref_version
                .map(|value| e2v_store::RefVersion { value }),
            next,
        )
    }

    fn complete(&self, session: PublishSession) -> Result<()> {
        self.remote_backend
            .delete_physical(&session.active_intent_path)?;
        if let Some(lease_path) = session.lease_path {
            self.remote_backend.delete_physical(&lease_path)?;
        }
        self.clear_heartbeat_state(&session.operation_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tempfile::tempdir;

    use crate::journal::OperationId;

    use e2v_store::{
        BlobStore, CasResult, ConsistencyClass, EncryptedRef, LayoutRoot, LayoutRootStore,
        ListedRef, RefStore, RefToken, RefVersion, StoredRef, WriterMode,
    };

    use super::*;

    #[derive(Debug, Clone)]
    struct LayoutRootWriteCountingBackend {
        inner: e2v_store::MemoryBackend,
        layout_root_puts: Arc<Mutex<usize>>,
        layout_root_reads: Arc<Mutex<usize>>,
    }

    impl LayoutRootWriteCountingBackend {
        fn new() -> Self {
            Self {
                inner: e2v_store::MemoryBackend::new(),
                layout_root_puts: Arc::new(Mutex::new(0)),
                layout_root_reads: Arc::new(Mutex::new(0)),
            }
        }

        fn layout_root_put_count(&self) -> usize {
            *self.layout_root_puts.lock().unwrap()
        }

        fn layout_root_read_count(&self) -> usize {
            *self.layout_root_reads.lock().unwrap()
        }
    }

    impl BlobStore for LayoutRootWriteCountingBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
            if relative_path == "layout_root.json" {
                *self.layout_root_puts.lock().unwrap() += 1;
            }
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> Result<()> {
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> Result<e2v_store::ObjectStat> {
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl RefStore for LayoutRootWriteCountingBackend {
        fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
            self.inner.read_ref(token)
        }

        fn list_refs(&self) -> Result<Vec<ListedRef>> {
            self.inner.list_refs()
        }

        fn compare_and_swap_ref(
            &self,
            token: &RefToken,
            expected: Option<RefVersion>,
            next: EncryptedRef,
        ) -> Result<CasResult> {
            self.inner.compare_and_swap_ref(token, expected, next)
        }
    }

    impl LayoutRootStore for LayoutRootWriteCountingBackend {
        fn read_layout_root(&self) -> Result<LayoutRoot> {
            *self.layout_root_reads.lock().unwrap() += 1;
            self.inner.read_layout_root()
        }

        fn compare_and_swap_layout_root(
            &self,
            expected: u64,
            next: LayoutRoot,
        ) -> Result<CasResult> {
            let current = self.inner.read_layout_root()?;
            if current.generation != expected {
                return Ok(CasResult {
                    applied: false,
                    current: None,
                });
            }
            let bytes = serde_json::to_vec(&next)?;
            self.put_physical("layout_root.json", &bytes)?;
            self.put_physical(
                &format!("control/layout-roots/{:020}.json", next.generation),
                &bytes,
            )?;
            Ok(CasResult {
                applied: true,
                current: None,
            })
        }

        fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
            self.inner.list_retained_layout_roots()
        }
    }

    impl e2v_store::RemoteBackend for LayoutRootWriteCountingBackend {
        fn capability(&self) -> &BackendCapability {
            self.inner.capability()
        }
    }

    #[derive(Debug, Clone)]
    struct FixedRemoteTimeBackend {
        inner: e2v_store::MemoryBackend,
        fixed_time: std::sync::Arc<std::sync::Mutex<SystemTime>>,
        counts: Arc<Mutex<MarkerIoCounts>>,
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
    struct MarkerIoCounts {
        intent_gets: usize,
        lease_gets: usize,
        probe_puts: usize,
        probe_deletes: usize,
        probe_stats: usize,
    }

    impl FixedRemoteTimeBackend {
        fn new(fixed_time: SystemTime) -> Self {
            Self {
                inner: e2v_store::MemoryBackend::new(),
                fixed_time: std::sync::Arc::new(std::sync::Mutex::new(fixed_time)),
                counts: Arc::new(Mutex::new(MarkerIoCounts::default())),
            }
        }

        fn set_fixed_time_for_test(&self, fixed_time: SystemTime) {
            *self.fixed_time.lock().unwrap() = fixed_time;
        }

        fn marker_io_counts(&self) -> MarkerIoCounts {
            *self.counts.lock().unwrap()
        }
    }

    impl BlobStore for FixedRemoteTimeBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
            if relative_path.ends_with(".probe") {
                self.counts.lock().unwrap().probe_puts += 1;
            }
            self.inner.put_physical(relative_path, bytes)?;
            if relative_path.starts_with("transactions/active/")
                || relative_path.starts_with("leases/")
                || relative_path.ends_with(".probe")
            {
                e2v_store::testing::override_memory_backend_modified_time(
                    &self.inner,
                    relative_path,
                    *self.fixed_time.lock().unwrap(),
                )?;
            }
            Ok(())
        }

        fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
            let created = self.inner.put_physical_if_absent(relative_path, bytes)?;
            if created
                && (relative_path.starts_with("transactions/active/")
                    || relative_path.starts_with("leases/")
                    || relative_path.ends_with(".probe"))
            {
                e2v_store::testing::override_memory_backend_modified_time(
                    &self.inner,
                    relative_path,
                    *self.fixed_time.lock().unwrap(),
                )?;
            }
            Ok(created)
        }

        fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
            if relative_path.starts_with("transactions/active/") {
                self.counts.lock().unwrap().intent_gets += 1;
            } else if relative_path.starts_with("leases/") {
                self.counts.lock().unwrap().lease_gets += 1;
            }
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> Result<()> {
            if relative_path.ends_with(".probe") {
                self.counts.lock().unwrap().probe_deletes += 1;
            }
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> Result<e2v_store::ObjectStat> {
            if relative_path.ends_with(".probe") {
                self.counts.lock().unwrap().probe_stats += 1;
            }
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl RefStore for FixedRemoteTimeBackend {
        fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
            self.inner.read_ref(token)
        }

        fn list_refs(&self) -> Result<Vec<ListedRef>> {
            self.inner.list_refs()
        }

        fn compare_and_swap_ref(
            &self,
            token: &RefToken,
            expected: Option<RefVersion>,
            next: EncryptedRef,
        ) -> Result<CasResult> {
            self.inner.compare_and_swap_ref(token, expected, next)
        }
    }

    impl LayoutRootStore for FixedRemoteTimeBackend {
        fn read_layout_root(&self) -> Result<LayoutRoot> {
            self.inner.read_layout_root()
        }

        fn compare_and_swap_layout_root(
            &self,
            expected: u64,
            next: LayoutRoot,
        ) -> Result<CasResult> {
            self.inner.compare_and_swap_layout_root(expected, next)
        }

        fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
            self.inner.list_retained_layout_roots()
        }
    }

    impl e2v_store::RemoteBackend for FixedRemoteTimeBackend {
        fn capability(&self) -> &BackendCapability {
            self.inner.capability()
        }
    }

    #[derive(Debug, Clone)]
    struct RaceInjectingLeaseBackend {
        inner: e2v_store::MemoryBackend,
        injected: Arc<Mutex<bool>>,
    }

    impl RaceInjectingLeaseBackend {
        fn new() -> Self {
            Self {
                inner: e2v_store::MemoryBackend::new(),
                injected: Arc::new(Mutex::new(false)),
            }
        }
    }

    impl BlobStore for RaceInjectingLeaseBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(&self, relative_path: &str, bytes: &[u8]) -> Result<bool> {
            if relative_path == "leases/branch-token.lock" {
                let mut injected = self.injected.lock().unwrap();
                if !*injected && !self.inner.exists_physical(relative_path) {
                    self.inner
                        .put_physical(
                            relative_path,
                            serde_json::to_vec(&serde_json::json!({
                                "writer_id": "writer:foreign-operation",
                                "operation_id": "foreign-operation",
                                "target_branch_token": "branch-token",
                                "remote_observed_at_unix_ms": 1,
                                "lease_generation": 1,
                                "heartbeat": {
                                    "remote_observed_at_unix_ms": 1,
                                    "sequence": 1
                                }
                            }))
                            .unwrap()
                            .as_slice(),
                        )
                        .unwrap();
                    *injected = true;
                    return Ok(false);
                }
            }
            self.inner.put_physical_if_absent(relative_path, bytes)
        }

        fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> Result<()> {
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> Result<e2v_store::ObjectStat> {
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl RefStore for RaceInjectingLeaseBackend {
        fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
            self.inner.read_ref(token)
        }

        fn list_refs(&self) -> Result<Vec<ListedRef>> {
            self.inner.list_refs()
        }

        fn compare_and_swap_ref(
            &self,
            token: &RefToken,
            expected: Option<RefVersion>,
            next: EncryptedRef,
        ) -> Result<CasResult> {
            self.inner.compare_and_swap_ref(token, expected, next)
        }
    }

    impl LayoutRootStore for RaceInjectingLeaseBackend {
        fn read_layout_root(&self) -> Result<LayoutRoot> {
            self.inner.read_layout_root()
        }

        fn compare_and_swap_layout_root(
            &self,
            expected: u64,
            next: LayoutRoot,
        ) -> Result<CasResult> {
            self.inner.compare_and_swap_layout_root(expected, next)
        }

        fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
            self.inner.list_retained_layout_roots()
        }
    }

    impl e2v_store::RemoteBackend for RaceInjectingLeaseBackend {
        fn capability(&self) -> &BackendCapability {
            self.inner.capability()
        }
    }

    #[derive(Debug, Clone)]
    struct UnsupportedIfAbsentLeaseBackend {
        inner: e2v_store::MemoryBackend,
        intent_puts: Arc<Mutex<usize>>,
        intent_stats: Arc<Mutex<usize>>,
    }

    impl UnsupportedIfAbsentLeaseBackend {
        fn new() -> Self {
            Self {
                inner: e2v_store::MemoryBackend::new(),
                intent_puts: Arc::new(Mutex::new(0)),
                intent_stats: Arc::new(Mutex::new(0)),
            }
        }

        fn intent_put_count(&self) -> usize {
            *self.intent_puts.lock().unwrap()
        }

        fn intent_stat_count(&self) -> usize {
            *self.intent_stats.lock().unwrap()
        }
    }

    impl BlobStore for UnsupportedIfAbsentLeaseBackend {
        fn put_physical(&self, relative_path: &str, bytes: &[u8]) -> Result<()> {
            if relative_path.starts_with("transactions/active/") {
                *self.intent_puts.lock().unwrap() += 1;
            }
            self.inner.put_physical(relative_path, bytes)
        }

        fn put_physical_if_absent(&self, _relative_path: &str, _bytes: &[u8]) -> Result<bool> {
            anyhow::bail!(
                "Unsupported (permanent) at write => The service webdav does not support the operation write with the arguments if_not_exists."
            );
        }

        fn get_physical(&self, relative_path: &str) -> Result<Vec<u8>> {
            self.inner.get_physical(relative_path)
        }

        fn get_physical_range(
            &self,
            relative_path: &str,
            offset: usize,
            length: usize,
        ) -> Result<Vec<u8>> {
            self.inner.get_physical_range(relative_path, offset, length)
        }

        fn delete_physical(&self, relative_path: &str) -> Result<()> {
            self.inner.delete_physical(relative_path)
        }

        fn exists_physical(&self, relative_path: &str) -> bool {
            self.inner.exists_physical(relative_path)
        }

        fn stat_physical(&self, relative_path: &str) -> Result<e2v_store::ObjectStat> {
            if relative_path.starts_with("transactions/active/") {
                *self.intent_stats.lock().unwrap() += 1;
            }
            self.inner.stat_physical(relative_path)
        }

        fn list_physical(&self, prefix: &str) -> Result<Vec<String>> {
            self.inner.list_physical(prefix)
        }
    }

    impl RefStore for UnsupportedIfAbsentLeaseBackend {
        fn read_ref(&self, token: &RefToken) -> Result<Option<StoredRef>> {
            self.inner.read_ref(token)
        }

        fn list_refs(&self) -> Result<Vec<ListedRef>> {
            self.inner.list_refs()
        }

        fn compare_and_swap_ref(
            &self,
            token: &RefToken,
            expected: Option<RefVersion>,
            next: EncryptedRef,
        ) -> Result<CasResult> {
            self.inner.compare_and_swap_ref(token, expected, next)
        }
    }

    impl LayoutRootStore for UnsupportedIfAbsentLeaseBackend {
        fn read_layout_root(&self) -> Result<LayoutRoot> {
            self.inner.read_layout_root()
        }

        fn compare_and_swap_layout_root(
            &self,
            expected: u64,
            next: LayoutRoot,
        ) -> Result<CasResult> {
            self.inner.compare_and_swap_layout_root(expected, next)
        }

        fn list_retained_layout_roots(&self) -> Result<Vec<LayoutRoot>> {
            self.inner.list_retained_layout_roots()
        }
    }

    impl e2v_store::RemoteBackend for UnsupportedIfAbsentLeaseBackend {
        fn capability(&self) -> &BackendCapability {
            self.inner.capability()
        }
    }

    #[test]
    fn begin_selects_multi_writer_mode_when_capability_supports_conditional_put() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            e2v_store::MemoryBackend::new(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-1".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        assert_eq!(session.writer_mode, WriterMode::MultiWriter);
    }

    #[test]
    fn begin_writes_structured_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-intent".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let active_intent = remote
            .get_physical("transactions/active/op-intent.intent")
            .unwrap();
        let intent: serde_json::Value = serde_json::from_slice(&active_intent).unwrap();

        assert!(!active_intent.is_empty());
        assert_eq!(session.operation_id.value, "op-intent");
        assert_eq!(intent["operation_id"], "op-intent");
        assert_eq!(intent["writer_id"], "writer:op-intent");
        assert_eq!(intent["target_branch_token"], "branch-token");
        assert_eq!(intent["client_version"], env!("CARGO_PKG_VERSION"));
        assert!(intent["expected_ref_version"].is_null());
        assert!(intent["planned_snapshot_id"].is_null());
        assert!(intent["started_at_remote_unix_ms"].as_u64().unwrap() > 0);
        assert!(
            intent["heartbeat"]["remote_observed_at_unix_ms"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(intent["heartbeat"]["sequence"], 1);
    }

    #[test]
    fn pre_commit_verify_rejects_missing_active_intent() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-precommit".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        remote
            .put_physical("transactions/active/op-precommit.intent", b"")
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("intent"));
    }

    #[test]
    fn pre_commit_verify_rejects_foreign_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: true,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::StrongWhitelisted,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-intent-owner".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        remote
            .put_physical(
                "transactions/active/op-intent-owner.intent",
                serde_json::to_vec(&serde_json::json!({
                    "operation_id": "other-operation",
                    "target_branch_token": "branch-token",
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("intent"));
    }

    #[test]
    fn begin_uses_single_writer_mode_when_cas_is_unavailable_but_lease_exists() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-single-writer".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        let lease = remote.get_physical("leases/branch-token.lock").unwrap();
        let lease_marker: serde_json::Value = serde_json::from_slice(&lease).unwrap();

        assert_eq!(session.writer_mode, WriterMode::SingleWriter);
        assert_eq!(lease_marker["operation_id"], "op-single-writer");
        assert_eq!(lease_marker["writer_id"], "writer:op-single-writer");
        assert_eq!(lease_marker["target_branch_token"], "branch-token");
        assert_eq!(lease_marker["lease_generation"], 1);
        assert!(lease_marker["remote_observed_at_unix_ms"].as_u64().unwrap() > 0);
        assert!(
            lease_marker["heartbeat"]["remote_observed_at_unix_ms"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert_eq!(lease_marker["heartbeat"]["sequence"], 1);
    }

    #[test]
    fn begin_uses_remote_modified_time_for_marker_observed_timestamps() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let fixed_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let fixed_ms = fixed_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let remote = FixedRemoteTimeBackend::new(fixed_time);
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let _session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-remote-time".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: Some(7),
                planned_snapshot_id: Some("snapshot-123".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let intent: serde_json::Value = serde_json::from_slice(
            &remote
                .get_physical("transactions/active/op-remote-time.intent")
                .unwrap(),
        )
        .unwrap();
        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert_eq!(intent["started_at_remote_unix_ms"], fixed_ms);
        assert_eq!(intent["heartbeat"]["remote_observed_at_unix_ms"], fixed_ms);
        assert_eq!(intent["expected_ref_version"], 7);
        assert_eq!(intent["planned_snapshot_id"], "snapshot-123");
        assert_eq!(lease["remote_observed_at_unix_ms"], fixed_ms);
        assert_eq!(lease["heartbeat"]["remote_observed_at_unix_ms"], fixed_ms);
    }

    #[test]
    fn publish_layout_if_needed_does_not_rewrite_layout_root_json_after_successful_cas() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = LayoutRootWriteCountingBackend::new();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-layout-root".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        let next_layout_root = LayoutRoot {
            generation: 2,
            ..LayoutRoot::direct_default()
        };
        let session = PublishSession {
            next_layout_root: Some(next_layout_root.clone()),
            next_layout_root_bytes: Some(serde_json::to_vec(&next_layout_root).unwrap()),
            ..session
        };

        publisher.publish_layout_if_needed(&session).unwrap();

        assert_eq!(
            remote.layout_root_put_count(),
            1,
            "layout root publish should not rewrite layout_root.json after the layout CAS already stored it"
        );
    }

    #[test]
    fn publish_layout_if_needed_reuses_observed_layout_generation_without_extra_read() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = LayoutRootWriteCountingBackend::new();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-layout-root-reuse".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        let next_layout_root = LayoutRoot {
            generation: 2,
            ..LayoutRoot::direct_default()
        };
        let session = PublishSession {
            observed_layout_root_generation: Some(1),
            next_layout_root: Some(next_layout_root.clone()),
            next_layout_root_bytes: Some(serde_json::to_vec(&next_layout_root).unwrap()),
            ..session
        };

        publisher.publish_layout_if_needed(&session).unwrap();

        assert_eq!(
            remote.layout_root_read_count(),
            0,
            "layout root publish should reuse the already observed generation instead of reading layout_root.json again before CAS"
        );
    }

    #[test]
    fn begin_rejects_single_writer_when_lease_cannot_be_acquired() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        remote
            .put_physical("leases/branch-token.lock", b"held-by-other-writer")
            .unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote,
        );

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-single-writer-fail".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("lease"));
    }

    #[test]
    fn begin_rejects_single_writer_when_foreign_writer_wins_lease_race() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = RaceInjectingLeaseBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-race".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert!(error.to_string().contains("lease"));
        assert_eq!(lease["operation_id"], "foreign-operation");
    }

    #[test]
    fn begin_reacquires_expired_single_writer_lease_from_previous_operation() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let fixed_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let remote = FixedRemoteTimeBackend::new(fixed_time);
        let previous_observed_ms =
            fixed_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        remote
            .put_physical(
                "leases/branch-token.lock",
                serde_json::to_vec(&serde_json::json!({
                    "writer_id": "writer:old-operation",
                    "operation_id": "old-operation",
                    "target_branch_token": "branch-token",
                    "remote_observed_at_unix_ms": previous_observed_ms,
                    "lease_generation": 7,
                    "heartbeat": {
                        "remote_observed_at_unix_ms": previous_observed_ms,
                        "sequence": 4
                    }
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();
        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "leases/branch-token.lock",
            fixed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("new-operation".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert_eq!(session.writer_mode, WriterMode::SingleWriter);
        assert_eq!(lease["operation_id"], "new-operation");
        assert_eq!(lease["writer_id"], "writer:new-operation");
    }

    #[test]
    fn begin_still_rejects_fresh_single_writer_lease_from_previous_operation() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let fixed_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let remote = FixedRemoteTimeBackend::new(fixed_time);
        let observed_ms = fixed_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        remote
            .put_physical(
                "leases/branch-token.lock",
                serde_json::to_vec(&serde_json::json!({
                    "writer_id": "writer:old-operation",
                    "operation_id": "old-operation",
                    "target_branch_token": "branch-token",
                    "remote_observed_at_unix_ms": observed_ms,
                    "lease_generation": 7,
                    "heartbeat": {
                        "remote_observed_at_unix_ms": observed_ms,
                        "sequence": 4
                    }
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote,
        );

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("new-operation".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("lease"));
    }

    #[test]
    fn begin_reacquires_expired_single_writer_lease_after_corrupted_stale_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path().join("journal")).unwrap();
        let remote_root = temp.path().join("remote");
        std::fs::create_dir_all(&remote_root).unwrap();
        let remote = e2v_store::LocalFolderBackend::new(&remote_root);
        remote
            .put_physical("leases/branch-token.lock", br#"{"broken":true"#)
            .unwrap();
        e2v_store::testing::override_local_backend_modified_time(
            &remote,
            "leases/branch-token.lock",
            UNIX_EPOCH + Duration::from_secs(1),
        )
        .unwrap();

        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("new-operation".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert_eq!(session.writer_mode, WriterMode::SingleWriter);
        assert_eq!(lease["operation_id"], "new-operation");
        assert_eq!(lease["writer_id"], "writer:new-operation");
    }

    #[test]
    fn heartbeat_renews_intent_and_lease_during_long_running_publish() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let fixed_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let next_time = fixed_time + Duration::from_secs(11 * 60);
        let initial_ms = fixed_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let next_ms = next_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let remote = FixedRemoteTimeBackend::new(fixed_time);
        let _heartbeat_clock_guard = override_heartbeat_time_for_test(remote.fixed_time.clone());
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-heartbeat".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-heartbeat".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        remote.set_fixed_time_for_test(next_time);

        publisher.heartbeat(&session).unwrap();

        let intent: serde_json::Value = serde_json::from_slice(
            &remote
                .get_physical("transactions/active/op-heartbeat.intent")
                .unwrap(),
        )
        .unwrap();
        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert_eq!(intent["started_at_remote_unix_ms"], initial_ms);
        assert_eq!(intent["heartbeat"]["remote_observed_at_unix_ms"], next_ms);
        assert_eq!(intent["heartbeat"]["sequence"], 2);
        assert_eq!(lease["remote_observed_at_unix_ms"], next_ms);
        assert_eq!(lease["heartbeat"]["remote_observed_at_unix_ms"], next_ms);
        assert_eq!(lease["heartbeat"]["sequence"], 2);
        assert_eq!(lease["lease_generation"], 2);
    }

    #[test]
    fn heartbeat_if_needed_skips_renewal_for_fresh_markers() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let fixed_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let remote = FixedRemoteTimeBackend::new(fixed_time);
        let _heartbeat_clock_guard = override_heartbeat_time_for_test(remote.fixed_time.clone());
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-fresh-heartbeat".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-fresh-heartbeat".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let before = remote.marker_io_counts();
        publisher.heartbeat_if_needed(&session).unwrap();
        let after = remote.marker_io_counts();

        let intent: serde_json::Value = serde_json::from_slice(
            &remote
                .get_physical("transactions/active/op-fresh-heartbeat.intent")
                .unwrap(),
        )
        .unwrap();
        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert_eq!(intent["heartbeat"]["sequence"], 1);
        assert_eq!(lease["heartbeat"]["sequence"], 1);
        assert_eq!(after.intent_gets - before.intent_gets, 0);
        assert_eq!(after.lease_gets - before.lease_gets, 0);
        assert_eq!(after.probe_puts - before.probe_puts, 0);
    }

    #[test]
    fn pre_commit_verify_skips_remote_time_probe_for_fresh_markers() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let fixed_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let remote = FixedRemoteTimeBackend::new(fixed_time);
        let _heartbeat_clock_guard = override_heartbeat_time_for_test(remote.fixed_time.clone());
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-fresh-precommit".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-fresh-precommit".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let before = remote.marker_io_counts();
        publisher.pre_commit_verify(&session).unwrap();
        let after = remote.marker_io_counts();

        let intent: serde_json::Value = serde_json::from_slice(
            &remote
                .get_physical("transactions/active/op-fresh-precommit.intent")
                .unwrap(),
        )
        .unwrap();
        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert_eq!(intent["heartbeat"]["sequence"], 1);
        assert_eq!(lease["heartbeat"]["sequence"], 1);
        assert_eq!(after.probe_puts - before.probe_puts, 0);
        assert_eq!(after.probe_stats - before.probe_stats, 0);
        assert_eq!(after.probe_deletes - before.probe_deletes, 0);
    }

    #[test]
    fn heartbeat_recovers_from_corrupted_stale_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let renewed_time = initial_time + Duration::from_secs(73 * 60 * 60);
        let initial_ms = initial_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let renewed_ms = renewed_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let _heartbeat_clock_guard = override_heartbeat_time_for_test(remote.fixed_time.clone());
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-heartbeat-corrupt-intent".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-heartbeat".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        remote
            .put_physical(
                "transactions/active/op-heartbeat-corrupt-intent.intent",
                br#"{"broken":true"#,
            )
            .unwrap();
        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "transactions/active/op-heartbeat-corrupt-intent.intent",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        remote.set_fixed_time_for_test(renewed_time);

        publisher.heartbeat(&session).unwrap();

        let intent: serde_json::Value = serde_json::from_slice(
            &remote
                .get_physical("transactions/active/op-heartbeat-corrupt-intent.intent")
                .unwrap(),
        )
        .unwrap();

        assert_eq!(intent["operation_id"], "op-heartbeat-corrupt-intent");
        assert_eq!(intent["planned_snapshot_id"], "snapshot-heartbeat");
        assert_eq!(intent["started_at_remote_unix_ms"], initial_ms);
        assert_eq!(
            intent["heartbeat"]["remote_observed_at_unix_ms"],
            renewed_ms
        );
        assert_eq!(intent["heartbeat"]["sequence"], 2);
    }

    #[test]
    fn heartbeat_still_rejects_corrupted_fresh_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-heartbeat-fresh-intent".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-heartbeat".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        remote
            .put_physical(
                "transactions/active/op-heartbeat-fresh-intent.intent",
                br#"{"broken":true"#,
            )
            .unwrap();

        let error = publisher.heartbeat(&session).unwrap_err();

        assert!(error.to_string().contains("invalid active intent marker"));
    }

    #[test]
    fn heartbeat_rejects_stale_active_intent_marker_when_branch_token_mismatches_session() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let renewed_time = initial_time + Duration::from_secs(73 * 60 * 60);
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-heartbeat-intent-mismatch".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-heartbeat".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        remote
            .put_physical(
                "transactions/active/op-heartbeat-intent-mismatch.intent",
                serde_json::to_vec(&serde_json::json!({
                    "operation_id": "op-heartbeat-intent-mismatch",
                    "writer_id": "writer:op-heartbeat-intent-mismatch",
                    "started_at_remote_unix_ms": 1731000000000u64,
                    "heartbeat": {
                        "remote_observed_at_unix_ms": 1731000000000u64,
                        "sequence": 1u64
                    },
                    "expected_ref_version": null,
                    "target_branch_token": "other-branch",
                    "planned_snapshot_id": "snapshot-heartbeat",
                    "client_version": env!("CARGO_PKG_VERSION"),
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();
        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "transactions/active/op-heartbeat-intent-mismatch.intent",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        remote.set_fixed_time_for_test(renewed_time);

        let error = publisher.heartbeat(&session).unwrap_err();

        assert!(
            error.to_string().contains("active intent") && error.to_string().contains("branch")
                || error.to_string().contains("invalid active intent marker"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn heartbeat_rejects_stale_writer_lease_marker_when_branch_token_mismatches_session() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let renewed_time = initial_time + Duration::from_secs(73 * 60 * 60);
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-heartbeat-lease-mismatch".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-heartbeat".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        remote
            .put_physical(
                "leases/branch-token.lock",
                serde_json::to_vec(&serde_json::json!({
                    "operation_id": "op-heartbeat-lease-mismatch",
                    "writer_id": "writer:op-heartbeat-lease-mismatch",
                    "target_branch_token": "other-branch",
                    "remote_observed_at_unix_ms": 1731000000000u64,
                    "lease_generation": 1u64,
                    "heartbeat": {
                        "remote_observed_at_unix_ms": 1731000000000u64,
                        "sequence": 1u64
                    },
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();
        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "leases/branch-token.lock",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        remote.set_fixed_time_for_test(renewed_time);

        let error = publisher.heartbeat(&session).unwrap_err();

        assert!(
            error.to_string().contains("writer lease") && error.to_string().contains("branch")
                || error.to_string().contains("invalid writer lease marker"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn begin_rejects_read_only_backend_capabilities() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: false,
                supports_atomic_create_if_absent: false,
                supports_transaction_markers: false,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: false,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
            journal,
            e2v_store::MemoryBackend::new(),
        );

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-read-only".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("read-only"));
    }

    #[test]
    fn begin_rejects_risky_single_writer_backend_by_default() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: false,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
            journal,
            e2v_store::MemoryBackend::new(),
        );

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-risky-single-writer".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("risky"));
    }

    #[test]
    fn begin_allowing_risky_single_writer_can_continue_without_remote_lease_primitive() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = UnsupportedIfAbsentLeaseBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: false,
                supports_transaction_markers: true,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: false,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin_allowing_risky_single_writer(
                PublishPlan {
                    operation_id: OperationId::new("op-risky-no-lease".to_string()).unwrap(),
                    target_branch_token: "branch-token".to_string(),
                    expected_ref_version: None,
                    planned_snapshot_id: None,
                    writer_mode: WriterMode::ReadOnly,
                },
                true,
            )
            .unwrap();

        assert_eq!(session.writer_mode, WriterMode::SingleWriter);
        assert!(session.lease_path.is_none());
        assert!(
            remote.exists_physical("transactions/active/op-risky-no-lease.intent"),
            "explicit risky single-writer mode should still publish an active intent marker"
        );
    }

    #[test]
    fn begin_allowing_risky_single_writer_writes_intent_once_without_remote_time_rewrite() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = UnsupportedIfAbsentLeaseBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: false,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: false,
                supports_transaction_markers: true,
                supports_reliable_remote_time: false,
                supports_object_generation_or_etag: false,
                supports_layout_root_cas: false,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin_allowing_risky_single_writer(
                PublishPlan {
                    operation_id: OperationId::new("op-risky-single-intent-write".to_string())
                        .unwrap(),
                    target_branch_token: "branch-token".to_string(),
                    expected_ref_version: None,
                    planned_snapshot_id: None,
                    writer_mode: WriterMode::ReadOnly,
                },
                true,
            )
            .unwrap();

        assert!(
            session.started_at_remote_unix_ms > 0,
            "the session should still remember the observed remote marker timestamp in memory"
        );
        assert_eq!(
            remote.intent_put_count(),
            1,
            "risky single-writer mode without verified remote time should not rewrite the active intent just to persist observed timestamps"
        );
        assert_eq!(
            remote.intent_stat_count(),
            1,
            "the publisher may still stat the active intent once to capture an in-memory observed timestamp"
        );
    }

    #[test]
    fn pre_commit_verify_rejects_foreign_writer_lease_holder() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-lease-owner".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        remote
            .put_physical(
                "leases/branch-token.lock",
                serde_json::to_vec(&serde_json::json!({
                    "operation_id": "other-operation",
                    "target_branch_token": "branch-token",
                }))
                .unwrap()
                .as_slice(),
            )
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("lease"));
    }

    #[test]
    fn pre_commit_verify_rejects_stale_remote_ref_version() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let token = e2v_store::RefToken::new("branch-token".to_string());
        remote
            .compare_and_swap_ref(&token, None, e2v_store::EncryptedRef::new(vec![1, 2, 3]))
            .unwrap();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());
        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-stale-precommit".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: Some(1),
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        remote
            .compare_and_swap_ref(
                &token,
                Some(e2v_store::RefVersion { value: 1 }),
                e2v_store::EncryptedRef::new(vec![4, 5, 6]),
            )
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("ref"));
    }

    #[test]
    fn pre_commit_verify_renews_expired_intent_and_lease_with_remote_observed_time() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let renewed_time = initial_time + Duration::from_secs(73 * 60 * 60);
        let initial_ms = initial_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let renewed_ms = renewed_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let _heartbeat_clock_guard = override_heartbeat_time_for_test(remote.fixed_time.clone());
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-renew-precommit".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-renew".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "transactions/active/op-renew-precommit.intent",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "leases/branch-token.lock",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        remote.set_fixed_time_for_test(renewed_time);

        publisher.pre_commit_verify(&session).unwrap();

        let intent: serde_json::Value = serde_json::from_slice(
            &remote
                .get_physical("transactions/active/op-renew-precommit.intent")
                .unwrap(),
        )
        .unwrap();
        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert_eq!(intent["started_at_remote_unix_ms"], initial_ms);
        assert_eq!(
            intent["heartbeat"]["remote_observed_at_unix_ms"],
            renewed_ms
        );
        assert_eq!(intent["heartbeat"]["sequence"], 2);
        assert_eq!(lease["remote_observed_at_unix_ms"], renewed_ms);
        assert_eq!(lease["heartbeat"]["remote_observed_at_unix_ms"], renewed_ms);
        assert_eq!(lease["heartbeat"]["sequence"], 2);
        assert_eq!(lease["lease_generation"], 2);
    }

    #[test]
    fn pre_commit_verify_recovers_from_corrupted_stale_lease_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let renewed_time = initial_time + Duration::from_secs(73 * 60 * 60);
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let _heartbeat_clock_guard = override_heartbeat_time_for_test(remote.fixed_time.clone());
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-corrupt-stale-precommit".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-precommit".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "transactions/active/op-corrupt-stale-precommit.intent",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "leases/branch-token.lock",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        remote
            .put_physical("leases/branch-token.lock", br#"{"broken":true"#)
            .unwrap();
        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "leases/branch-token.lock",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        remote.set_fixed_time_for_test(renewed_time);

        publisher.pre_commit_verify(&session).unwrap();

        let lease: serde_json::Value =
            serde_json::from_slice(&remote.get_physical("leases/branch-token.lock").unwrap())
                .unwrap();

        assert_eq!(lease["operation_id"], "op-corrupt-stale-precommit");
        assert_eq!(lease["writer_id"], "writer:op-corrupt-stale-precommit");
    }

    #[test]
    fn pre_commit_verify_recovers_from_corrupted_stale_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let renewed_time = initial_time + Duration::from_secs(73 * 60 * 60);
        let initial_ms = initial_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let renewed_ms = renewed_time.duration_since(UNIX_EPOCH).unwrap().as_millis() as u64;
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let _heartbeat_clock_guard = override_heartbeat_time_for_test(remote.fixed_time.clone());
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-corrupt-stale-intent".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-precommit".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        remote
            .put_physical(
                "transactions/active/op-corrupt-stale-intent.intent",
                br#"{"broken":true"#,
            )
            .unwrap();
        e2v_store::testing::override_memory_backend_modified_time(
            &remote.inner,
            "transactions/active/op-corrupt-stale-intent.intent",
            renewed_time - Duration::from_secs(73 * 60 * 60),
        )
        .unwrap();
        remote.set_fixed_time_for_test(renewed_time);

        publisher.pre_commit_verify(&session).unwrap();

        let intent: serde_json::Value = serde_json::from_slice(
            &remote
                .get_physical("transactions/active/op-corrupt-stale-intent.intent")
                .unwrap(),
        )
        .unwrap();

        assert_eq!(intent["operation_id"], "op-corrupt-stale-intent");
        assert_eq!(intent["writer_id"], "writer:op-corrupt-stale-intent");
        assert_eq!(intent["target_branch_token"], "branch-token");
        assert_eq!(intent["planned_snapshot_id"], "snapshot-precommit");
        assert_eq!(intent["started_at_remote_unix_ms"], initial_ms);
        assert_eq!(
            intent["heartbeat"]["remote_observed_at_unix_ms"],
            renewed_ms
        );
        assert_eq!(intent["heartbeat"]["sequence"], 1);
    }

    #[test]
    fn pre_commit_verify_still_rejects_corrupted_fresh_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-corrupt-fresh-intent".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-precommit".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        remote
            .put_physical(
                "transactions/active/op-corrupt-fresh-intent.intent",
                br#"{"broken":true"#,
            )
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("invalid active intent marker"));
    }

    #[test]
    fn pre_commit_verify_still_rejects_corrupted_fresh_lease_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let initial_time = UNIX_EPOCH + Duration::from_secs(1_731_000_000);
        let remote = FixedRemoteTimeBackend::new(initial_time);
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );

        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-corrupt-fresh-precommit".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: Some("snapshot-precommit".to_string()),
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        remote
            .put_physical("leases/branch-token.lock", br#"{"broken":true"#)
            .unwrap();

        let error = publisher.pre_commit_verify(&session).unwrap_err();

        assert!(error.to_string().contains("invalid writer lease marker"));
    }

    #[test]
    fn publish_ref_respects_expected_remote_ref_version() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let token = e2v_store::RefToken::new("branch-token".to_string());
        remote
            .compare_and_swap_ref(&token, None, e2v_store::EncryptedRef::new(vec![1, 2, 3]))
            .unwrap();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());
        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-publish-ref".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: Some(1),
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        let applied = publisher
            .publish_ref(&session, e2v_store::EncryptedRef::new(vec![4, 5, 6]))
            .unwrap();
        assert!(applied.applied);
        assert_eq!(applied.current.unwrap().version.value, 2);

        let stale = publisher
            .publish_ref(&session, e2v_store::EncryptedRef::new(vec![7, 8, 9]))
            .unwrap();
        assert!(!stale.applied);
        assert_eq!(stale.current.unwrap().version.value, 2);
    }
    #[test]
    fn complete_clears_active_intent_marker() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());
        let session = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-complete-intent".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        publisher.complete(session).unwrap();

        assert!(!remote.exists_physical("transactions/active/op-complete-intent.intent"));
    }

    #[test]
    fn complete_releases_single_writer_lease_for_next_publish() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher = SimpleTransactionPublisher::new(
            BackendCapability {
                supports_conditional_put: false,
                supports_range_read: true,
                supports_atomic_rename: true,
                supports_paged_list: true,
                consistency_class: ConsistencyClass::UnknownOrEventual,
                supports_remote_lock_or_lease: true,
                supports_atomic_create_if_absent: true,
                supports_transaction_markers: true,
                supports_reliable_remote_time: true,
                supports_object_generation_or_etag: true,
                supports_layout_root_cas: true,
                supports_oblivious_access_schedule: false,
            },
            journal,
            remote.clone(),
        );
        let first = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-complete-lease-1".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();

        publisher.complete(first).unwrap();

        let second = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-complete-lease-2".to_string()).unwrap(),
                target_branch_token: "branch-token".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap();
        let lease_bytes = remote.get_physical("leases/branch-token.lock").unwrap();
        let lease_marker: serde_json::Value = serde_json::from_slice(&lease_bytes).unwrap();

        assert_eq!(second.writer_mode, WriterMode::SingleWriter);
        assert_eq!(lease_marker["operation_id"], "op-complete-lease-2");
    }

    #[test]
    fn begin_rejects_branch_token_with_path_traversal_before_writing_markers() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote.clone());

        let error = publisher
            .begin(PublishPlan {
                operation_id: OperationId::new("op-invalid-branch".to_string()).unwrap(),
                target_branch_token: "../evil".to_string(),
                expected_ref_version: None,
                planned_snapshot_id: None,
                writer_mode: WriterMode::ReadOnly,
            })
            .unwrap_err();

        assert!(error.to_string().contains("branch"));
        assert!(!remote.exists_physical("transactions/active/op-invalid-branch.intent"));
        assert!(!remote.exists_physical("leases/../evil.lock"));
    }

    #[test]
    fn publish_ref_rejects_branch_token_with_path_traversal_in_session() {
        let temp = tempdir().unwrap();
        let journal = OperationJournal::new(temp.path()).unwrap();
        let remote = e2v_store::MemoryBackend::new();
        let publisher =
            SimpleTransactionPublisher::new(remote.capability().clone(), journal, remote);
        let session = PublishSession {
            operation_id: OperationId::new("op-invalid-session".to_string()).unwrap(),
            target_branch_token: "../evil".to_string(),
            expected_ref_version: None,
            planned_snapshot_id: None,
            writer_mode: WriterMode::ReadOnly,
            started_at_remote_unix_ms: 0,
            observed_layout_root_generation: None,
            next_layout_root: None,
            next_layout_root_bytes: None,
            active_intent_path: "transactions/active/op-invalid-session.intent".to_string(),
            lease_path: None,
        };

        let error = publisher
            .publish_ref(&session, EncryptedRef::new(vec![1, 2, 3]))
            .unwrap_err();

        assert!(error.to_string().contains("branch"));
    }
}
