use super::*;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CheckpointReceipt {
    pub run_id: String,
    #[ts(type = "number")]
    pub data_generation: u64,
    #[ts(type = "number")]
    pub scan_count: u64,
    #[ts(type = "number")]
    pub sampled_at_unix_ms: u64,
    #[ts(type = "number")]
    pub completed_at_unix_ms: u64,
    pub sha256: String,
    pub storage: project::AtomicWriteReceipt,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct CheckpointExport {
    /// Exact bytes whose SHA256 appears in the receipt; preserves JSON spelling.
    pub checkpoint_json: String,
    pub receipt: CheckpointReceipt,
    pub state: RetainState,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export)]
#[serde(rename_all = "snake_case")]
pub enum FinalSave {
    #[default]
    NotRequested,
    Pending,
    Durable,
    Unconfirmed,
    Failed,
    Skipped,
    TimedOut,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct PersistenceHealth {
    #[ts(type = "number")]
    pub data_generation: u64,
    #[ts(type = "number")]
    pub sampled_generation: u64,
    #[ts(type = "number | null")]
    pub pending_age_ms: Option<u64>,
    pub writing: bool,
    pub queued: bool,
    pub last_replacement: Option<CheckpointReceipt>,
    pub last_durable: Option<CheckpointReceipt>,
    pub last_error: Option<String>,
    pub final_save: FinalSave,
    pub budget_exceeded: bool,
    pub on_loss: project::LossAction,
    #[ts(type = "number | null")]
    pub max_loss_ms: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export)]
pub struct RetainStatus {
    pub run_id: String,
    pub policy: RetainPolicy,
    pub attention: Option<String>,
    pub recovery: RecoveryResult,
    pub persistence: PersistenceHealth,
}
#[derive(Default)]
struct WriterStatus {
    last_replacement: Option<CheckpointReceipt>,
    last_durable: Option<CheckpointReceipt>,
    export: Option<CheckpointExport>,
    last_error: Option<String>,
    final_save: FinalSave,
}

/// HTTP readers can take the status locks. Scan-round publication uses only
/// atomics and try_send on a mailbox of capacity ONE; it never takes them.
static NEXT_CONTROL: AtomicU64 = AtomicU64::new(1);

pub struct RetainControl {
    id: u64,
    run_id: String,
    config: RetainConfig,
    plan: Result<Plan, String>,
    recovery: Mutex<RecoveryResult>,
    writer: Mutex<WriterStatus>,
    start: Instant,
    generation: AtomicU64,
    observed_scans: AtomicU64,
    sampled: AtomicU64,
    durable: AtomicU64,
    durable_sample_ms: AtomicU64,
    first_dirty_ms: AtomicU64,
    writing: AtomicBool,
    queued: AtomicU64,
    enabled: AtomicBool,
    active: AtomicBool,
    scan_active: AtomicBool,
    writer_failed: AtomicBool,
    resolution: std::sync::OnceLock<Option<ResolutionEvidence>>,
}
impl RetainControl {
    pub fn new(config: RetainConfig, units: &[crate::ProgramUnit]) -> Arc<Self> {
        let plan = Plan::from_units(
            if config.path.is_some() { units } else { &[] },
            &config.policy,
        );
        let id = NEXT_CONTROL.fetch_add(1, Ordering::Relaxed);
        let run_id = format!(
            "{}-{}-{id}",
            std::process::id(),
            crate::monitor::wall_now_us()
        );
        Arc::new(Self {
            id,
            run_id,
            config,
            plan,
            recovery: Mutex::new(RecoveryResult::new(
                RecoveryKind::Pending,
                false,
                "RETAIN admission pending",
            )),
            writer: Mutex::new(WriterStatus::default()),
            start: Instant::now(),
            generation: AtomicU64::new(0),
            observed_scans: AtomicU64::new(0),
            sampled: AtomicU64::new(0),
            durable: AtomicU64::new(0),
            durable_sample_ms: AtomicU64::new(0),
            first_dirty_ms: AtomicU64::new(u64::MAX),
            writing: AtomicBool::new(false),
            queued: AtomicU64::new(0),
            enabled: AtomicBool::new(false),
            active: AtomicBool::new(true),
            scan_active: AtomicBool::new(true),
            writer_failed: AtomicBool::new(false),
            resolution: std::sync::OnceLock::new(),
        })
    }
    pub(crate) fn prepare(
        self: &Arc<Self>,
        io: &tokio::runtime::Handle,
    ) -> Result<(BTreeMap<String, u64>, Option<RetainWriter>), String> {
        let plan = match &self.plan {
            Ok(p) => p,
            Err(e) => {
                *self.recovery.lock() =
                    RecoveryResult::new(RecoveryKind::ConfigurationError, false, e);
                self.finish_without_writer();
                return Err(e.clone());
            }
        };
        let prepared = storage::prepare(&self.config, plan);
        let _ = self.resolution.set(prepared.result.resolution.clone());
        let admitted = prepared.result.admitted;
        let reason = prepared.result.reason.clone();
        self.enabled
            .store(prepared.result.writes_enabled, Ordering::Release);
        *self.recovery.lock() = prepared.result;
        if !admitted {
            self.finish_without_writer();
            return Err(reason);
        }
        let writer = prepared
            .lease
            .map(|mut lease| RetainWriter::spawn(self.clone(), io, move |state| lease.save(state)));
        Ok((prepared.restored, writer))
    }
    pub fn status(&self) -> RetainStatus {
        let recovery = self.recovery.lock().clone();
        let writer = self.writer.lock();
        let pending_age_ms = self.pending_age();
        let last_error = writer.last_error.clone().or_else(|| {
            self.writer_failed
                .load(Ordering::Acquire)
                .then(|| "RETAIN writer panicked".into())
        });
        let attention = if !recovery.admitted && recovery.kind != RecoveryKind::Pending
            || recovery.kind == RecoveryKind::ColdStart
        {
            Some(recovery.reason.clone())
        } else if let Some(error) = &last_error {
            Some(error.clone())
        } else if self.budget_exceeded() {
            Some("RETAIN persistence loss budget exceeded".into())
        } else {
            None
        };
        RetainStatus {
            run_id: self.run_id.clone(),
            policy: self.config.policy.clone(),
            attention,
            recovery,
            persistence: PersistenceHealth {
                data_generation: self.generation.load(Ordering::Acquire),
                sampled_generation: self.sampled.load(Ordering::Acquire),
                pending_age_ms,
                writing: self.writing.load(Ordering::Acquire),
                queued: self.queued.load(Ordering::Acquire) > 0,
                last_replacement: writer.last_replacement.clone(),
                last_durable: writer.last_durable.clone(),
                last_error,
                final_save: writer.final_save,
                budget_exceeded: self.budget_exceeded(),
                on_loss: self.config.policy.on_loss,
                max_loss_ms: self.config.policy.max_loss_ms,
            },
        }
    }
    pub fn supervise(self: &Arc<Self>, alarms: Arc<Mutex<crate::monitor::AlarmEngine>>) {
        alarms.lock().set_retain_owner(self.id);
        let control = self.clone();
        tokio::spawn(async move {
            loop {
                let status = control.status();
                if let Some(mut engine) = alarms.try_lock() {
                    engine.note_retain(
                        control.id,
                        status.attention.as_deref(),
                        crate::monitor::wall_now_us(),
                    );
                }
                if !control.active.load(Ordering::Acquire)
                    && !control.scan_active.load(Ordering::Acquire)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    }
    pub fn checkpoint(&self) -> Option<CheckpointExport> {
        self.writer.lock().export.clone()
    }
    /// Explicit handling never un-latches or restarts a runtime. The owning
    /// process must be stopped; the on-disk lease also excludes other processes.
    pub fn resolve(&self, request: &ResolveRequest) -> Result<ResolveResult, String> {
        if self.active.load(Ordering::Acquire) || self.scan_active.load(Ordering::Acquire) {
            return Err("stop this run and wait for its RETAIN writer before resolving".into());
        }
        storage::resolve(
            &self.config,
            self.plan.as_ref().map_err(Clone::clone)?,
            request,
        )
    }
    pub(crate) fn entries(&self) -> &[Entry] {
        self.plan.as_ref().map_or(&[], |p| p.entries.as_slice())
    }
    pub(crate) fn sample_interval(&self) -> Duration {
        Duration::from_millis(self.config.policy.sample_interval_ms)
    }
    pub(crate) fn mark_scan(&self, scan: u64) {
        let previous = self.observed_scans.swap(scan, Ordering::Relaxed);
        self.mark_change(scan.saturating_sub(previous));
    }
    pub(crate) fn mark_write(&self, unit: usize, slot: u16) {
        if self
            .entries()
            .iter()
            .any(|e| e.unit == unit && e.slot == slot)
        {
            self.mark_change(1);
        }
    }
    fn mark_change(&self, count: u64) {
        if count > 0 && self.enabled.load(Ordering::Acquire) {
            if self.generation.load(Ordering::Acquire) == self.durable.load(Ordering::Acquire) {
                self.first_dirty_ms
                    .store(self.elapsed_ms(), Ordering::Release);
            }
            self.generation.fetch_add(count, Ordering::Release);
        }
    }
    fn elapsed_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    fn pending_age(&self) -> Option<u64> {
        (self.enabled.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) > self.durable.load(Ordering::Acquire))
        .then(|| {
            self.elapsed_ms().saturating_sub(
                self.first_dirty_ms
                    .load(Ordering::Acquire)
                    .max(self.durable_sample_ms.load(Ordering::Acquire)),
            )
        })
    }
    pub(crate) fn budget_exceeded(&self) -> bool {
        self.config
            .policy
            .max_loss_ms
            .zip(self.pending_age())
            .is_some_and(|(budget, age)| age > budget)
    }
    pub(crate) fn must_inhibit(&self) -> bool {
        self.config.policy.on_loss == project::LossAction::Inhibit && self.budget_exceeded()
    }
    pub(crate) fn finish_without_writer(&self) {
        self.active.store(false, Ordering::Release);
        self.writer.lock().final_save = FinalSave::Skipped;
    }
    pub(crate) fn scan_finished(&self) {
        self.scan_active.store(false, Ordering::Release);
    }
    pub fn mark_final_timeout(&self) {
        self.writer.lock().final_save = FinalSave::TimedOut;
    }
}

struct Sample {
    state: RetainState,
    sampled_ms: u64,
}
pub(crate) struct RetainWriter {
    control: Arc<RetainControl>,
    samples: Option<mpsc::Sender<Sample>>,
    final_sample: Option<Sample>,
    task: tokio::task::JoinHandle<()>,
}
impl RetainWriter {
    fn spawn(
        control: Arc<RetainControl>,
        io: &tokio::runtime::Handle,
        mut save: impl FnMut(&RetainState) -> Result<(project::AtomicWriteReceipt, String), String>
            + Send
            + 'static,
    ) -> Self {
        let (tx, mut rx) = mpsc::channel::<Sample>(1);
        let shared = control.clone();
        let task = io.spawn_blocking(move || {
            struct Activity(Arc<RetainControl>);
            impl Drop for Activity {
                fn drop(&mut self) {
                    if std::thread::panicking() {
                        self.0.writer_failed.store(true, Ordering::Release);
                    }
                    self.0.writing.store(false, Ordering::Release);
                    self.0.queued.store(0, Ordering::Release);
                    self.0.active.store(false, Ordering::Release);
                }
            }
            let _activity = Activity(shared.clone());
            while let Some(sample) = rx.blocking_recv() {
                shared.queued.fetch_sub(1, Ordering::AcqRel);
                shared.writing.store(true, Ordering::Release);
                let result = save(&sample.state);
                let mut status = shared.writer.lock();
                match result {
                    Ok((storage, sha256)) => {
                        let receipt = CheckpointReceipt {
                            run_id: shared.run_id.clone(),
                            data_generation: sample.state.data_generation,
                            scan_count: sample.state.scan_count,
                            sampled_at_unix_ms: sample.state.saved_at_unix_ms,
                            completed_at_unix_ms: now_ms(),
                            sha256,
                            storage,
                        };
                        if receipt.storage.durable {
                            status.last_error = None;
                            status.last_durable = Some(receipt.clone());
                            status.export = Some(CheckpointExport {
                                checkpoint_json: serde_json::to_string_pretty(&sample.state)
                                    .expect("scalar checkpoint serialization"),
                                receipt: receipt.clone(),
                                state: sample.state,
                            });
                            shared
                                .durable_sample_ms
                                .store(sample.sampled_ms, Ordering::Release);
                            shared
                                .durable
                                .store(receipt.data_generation, Ordering::Release);
                        } else {
                            status.last_error = receipt.storage.directory_sync_error.clone();
                        }
                        status.last_replacement = Some(receipt);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "RETAIN checkpoint failed");
                        status.last_error = Some(e);
                    }
                }
                shared.writing.store(false, Ordering::Release);
            }
            shared.active.store(false, Ordering::Release);
        });
        Self {
            control,
            samples: Some(tx),
            final_sample: None,
            task,
        }
    }
    /// A full mailbox skips this sample; the next period samples the current
    /// VM. There is at most one queued sample plus one in flight.
    pub(crate) fn sample(
        &mut self,
        raw: BTreeMap<String, u64>,
        scan_count: u64,
        final_sample: bool,
    ) {
        let plan = self
            .control
            .plan
            .as_ref()
            .expect("writer has a validated plan");
        if raw.len() != plan.entries.len() {
            return;
        } // no partial checkpoint
        let generation = self.control.generation.load(Ordering::Acquire);
        let sample = Sample {
            state: {
                let mut state = plan.sample(raw, generation, scan_count);
                state.resolution = self.control.resolution.get().cloned().flatten();
                state
            },
            sampled_ms: self.control.elapsed_ms(),
        };
        if final_sample {
            self.final_sample = Some(sample);
            return;
        }
        if let Some(sender) = &self.samples {
            if let Ok(permit) = sender.try_reserve() {
                self.control.queued.fetch_add(1, Ordering::AcqRel);
                self.control.sampled.store(generation, Ordering::Release);
                permit.send(sample);
            }
        }
    }
    /// Called after device teardown, outside all scan rounds. Timeout does not
    /// cancel a blocking OS write: the worker keeps its exclusive lease until
    /// it actually exits; a replacement run cannot race it.
    pub(crate) async fn finish(mut self) {
        self.control.writer.lock().final_save = FinalSave::Pending;
        let final_generation = self.final_sample.as_ref().map(|s| s.state.data_generation);
        let tx = self.samples.take().unwrap();
        let shared = self.control.clone();
        let wait = async {
            if let Some(sample) = self.final_sample.take() {
                shared
                    .sampled
                    .store(sample.state.data_generation, Ordering::Release);
                let permit = tx
                    .reserve()
                    .await
                    .map_err(|_| "RETAIN writer closed".to_string())?;
                shared.queued.fetch_add(1, Ordering::AcqRel);
                permit.send(sample);
            }
            drop(tx);
            self.task.await.map_err(|e| e.to_string())
        };
        let result = tokio::time::timeout(Duration::from_secs(2), wait).await;
        let mut status = shared.writer.lock();
        status.final_save = match result {
            Err(_) => FinalSave::TimedOut,
            Ok(Err(e)) => {
                status.last_error = Some(e);
                FinalSave::Failed
            }
            Ok(Ok(())) if final_generation.is_none() => FinalSave::Skipped,
            Ok(Ok(()))
                if status.last_durable.as_ref().map(|r| r.data_generation) == final_generation =>
            {
                FinalSave::Durable
            }
            Ok(Ok(()))
                if status.last_replacement.as_ref().map(|r| r.data_generation)
                    == final_generation =>
            {
                FinalSave::Unconfirmed
            }
            _ => FinalSave::Failed,
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn control(action: project::LossAction) -> Arc<RetainControl> {
        let (container, metadata) = crate::compile_with_metadata(
            "PROGRAM main VAR RETAIN value : DINT; END_VAR END_PROGRAM",
        )
        .unwrap();
        let units = [crate::ProgramUnit {
            instance: "a".into(),
            task_name: "t".into(),
            interval_ms: 10,
            priority: 1,
            container,
            retain_vars: metadata.retain_vars,
        }];
        let control = RetainControl::new(
            RetainConfig {
                path: Some(PathBuf::from("unused")),
                policy: RetainPolicy {
                    project_id: Some("test".into()),
                    max_loss_ms: Some(20),
                    on_loss: action,
                    ..Default::default()
                },
            },
            &units,
        );
        control.enabled.store(true, Ordering::Release);
        *control.recovery.lock() = RecoveryResult::new(RecoveryKind::FirstStart, true, "test");
        control
    }
    fn raw(n: u64) -> BTreeMap<String, u64> {
        BTreeMap::from([("a.value".into(), n)])
    }
    #[tokio::test]
    async fn stalled_disk_has_one_pending_sample_growing_age_and_final_receipt() {
        let control = control(project::LossAction::Inhibit);
        let (entered, mut receives) = mpsc::unbounded_channel();
        let (release, blocker) = std::sync::mpsc::channel();
        let mut writer = RetainWriter::spawn(
            control.clone(),
            &tokio::runtime::Handle::current(),
            move |s| {
                entered.send(s.scan_count).unwrap();
                if s.scan_count == 1 {
                    blocker.recv().unwrap();
                }
                Ok((
                    project::AtomicWriteReceipt {
                        replaced: true,
                        durable: true,
                        directory_sync_error: None,
                    },
                    "hash".into(),
                ))
            },
        );
        control.mark_scan(1);
        writer.sample(raw(1), 1, false);
        assert_eq!(receives.recv().await, Some(1));
        for n in 2..1000 {
            control.mark_scan(n);
            writer.sample(raw(n), n, false);
        }
        tokio::time::sleep(Duration::from_millis(35)).await;
        let status = control.status();
        assert!(status.persistence.writing);
        assert!(status.persistence.queued);
        assert!(status.persistence.pending_age_ms.unwrap() >= 20);
        assert!(control.must_inhibit());
        assert_eq!(control.queued.load(Ordering::Acquire), 1);
        control.mark_scan(1000);
        writer.sample(raw(1000), 1000, true);
        release.send(()).unwrap();
        writer.finish().await;
        let status = control.status();
        assert_eq!(status.persistence.final_save, FinalSave::Durable);
        assert_eq!(status.persistence.last_durable.unwrap().scan_count, 1000);
        assert_eq!(status.persistence.pending_age_ms, None);
        assert_eq!(
            control.checkpoint().unwrap().state.vars["a.value"].bits,
            "1000"
        );
    }
    #[tokio::test]
    async fn write_and_directory_sync_failures_are_distinct_and_final_save_is_honest() {
        for directory_sync in [false, true] {
            let control = control(project::LossAction::Warn);
            let mut writer = RetainWriter::spawn(
                control.clone(),
                &tokio::runtime::Handle::current(),
                move |_| {
                    if directory_sync {
                        Ok((
                            project::AtomicWriteReceipt {
                                replaced: true,
                                durable: false,
                                directory_sync_error: Some("directory sync unsupported".into()),
                            },
                            "hash".into(),
                        ))
                    } else {
                        Err("disk full / write / file sync / rename denied".into())
                    }
                },
            );
            control.mark_scan(7);
            writer.sample(raw(7), 7, true);
            writer.finish().await;
            let status = control.status();
            assert!(status.persistence.last_durable.is_none());
            assert!(status.persistence.last_error.is_some());
            assert_eq!(
                status.persistence.last_replacement.is_some(),
                directory_sync
            );
            assert_eq!(
                status.persistence.final_save,
                if directory_sync {
                    FinalSave::Unconfirmed
                } else {
                    FinalSave::Failed
                }
            );
            assert!(control.checkpoint().is_none());
        }
    }
    #[tokio::test]
    async fn final_timeout_is_retained_even_if_the_write_later_completes() {
        let control = control(project::LossAction::Warn);
        let (release, blocker) = std::sync::mpsc::channel();
        let mut writer = RetainWriter::spawn(
            control.clone(),
            &tokio::runtime::Handle::current(),
            move |_| {
                blocker.recv().unwrap();
                Ok((
                    project::AtomicWriteReceipt {
                        replaced: true,
                        durable: true,
                        directory_sync_error: None,
                    },
                    "hash".into(),
                ))
            },
        );
        control.mark_scan(1);
        writer.sample(raw(1), 1, true);
        writer.finish().await;
        assert_eq!(control.status().persistence.final_save, FinalSave::TimedOut);
        assert!(control.active.load(Ordering::Acquire));
        release.send(()).unwrap();
        for _ in 0..100 {
            if !control.active.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(control.status().persistence.final_save, FinalSave::TimedOut);
        assert!(control.status().persistence.last_durable.is_some());
    }
    #[tokio::test]
    async fn writer_panic_does_not_authorize_resolution_during_a_live_scan() {
        let control = control(project::LossAction::Warn);
        let mut writer =
            RetainWriter::spawn(control.clone(), &tokio::runtime::Handle::current(), |_| {
                panic!("injected storage worker panic")
            });
        control.mark_scan(1);
        writer.sample(raw(1), 1, true);
        writer.finish().await;
        assert_eq!(control.status().persistence.final_save, FinalSave::Failed);
        assert!(control.status().persistence.last_error.is_some());
        assert!(!control.active.load(Ordering::Acquire));
        let request = ResolveRequest {
            expected_source_sha256: digest(&[]),
            migration_source_sha256: None,
            action: ResolutionAction::ColdStart,
            initialize_required: false,
            attested_source_identity: None,
            rules: vec![],
        };
        assert!(control
            .resolve(&request)
            .unwrap_err()
            .contains("stop this run"));
        control.scan_finished();
        assert!(!control.scan_active.load(Ordering::Acquire));
    }
}
