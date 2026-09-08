//! Local-only telemetry. The UI only clones snapshots; provider calls run on workers.
//! CPU is time-based busy time, disk is the busiest physical disk's active time,
//! and GPU is the busiest physical engine after summing its per-process counters.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[cfg(windows)]
mod native;

pub const FAST_INTERVAL: Duration = Duration::from_millis(250);
pub const SLOW_INTERVAL: Duration = Duration::from_secs(1);
const RESUME_GAP: Duration = Duration::from_secs(3);
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const SHUTDOWN_BUDGET: Duration = Duration::from_millis(250);
type Notification = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricState {
    Ready,
    WarmingUp,
    NotPresent,
    Unsupported,
    Error,
    Stale,
}

impl MetricState {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "정상",
            Self::WarmingUp => "준비 중",
            Self::NotPresent => "장치 없음",
            Self::Unsupported => "미지원",
            Self::Error => "읽기 오류",
            Self::Stale => "갱신 지연",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Metric {
    /// Percent for CPU/RAM/GPU/disk/NPU; RPM for fan; bytes/s for disk throughput.
    /// A missing sensor never receives a fabricated zero.
    pub value: Option<f64>,
    pub state: MetricState,
    pub detail: String,
    pub source: &'static str,
    /// Completion time of the read, using a monotonic clock.
    pub sampled_at: Instant,
    /// Actual time between the two reads for a rate, or zero for a point reading.
    pub window: Duration,
}

impl Metric {
    pub fn unavailable(
        state: MetricState,
        source: &'static str,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            value: None,
            state,
            detail: detail.into(),
            source,
            sampled_at: Instant::now(),
            window: Duration::ZERO,
        }
    }

    fn reading(
        value: f64,
        source: &'static str,
        window: Duration,
        detail: impl Into<String>,
    ) -> Self {
        if !value.is_finite() || value < 0.0 {
            return Self::unavailable(MetricState::Error, source, "유효하지 않은 센서 값");
        }
        Self {
            value: Some(value),
            state: MetricState::Ready,
            detail: detail.into(),
            source,
            sampled_at: Instant::now(),
            window,
        }
    }

    fn expire(&mut self, now: Instant, timeout: Duration) {
        if matches!(self.state, MetricState::Ready | MetricState::WarmingUp)
            && now.saturating_duration_since(self.sampled_at) > timeout
        {
            self.state = MetricState::Stale;
            // Preserve the last sample for diagnostics, but the UI must display state.
        }
    }
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub cpu: Metric,
    pub ram: Metric,
    pub gpu: Metric,
    pub disk: Metric,
    pub npu: Metric,
    pub fan: Metric,
    pub memory_used_bytes: u64,
    pub memory_total_bytes: u64,
    pub disk_read_bytes_sec: Metric,
    pub disk_write_bytes_sec: Metric,
    pub gpu_adapter: String,
    pub sequence: u64,
}

impl Default for Snapshot {
    fn default() -> Self {
        let warming =
            |source| Metric::unavailable(MetricState::WarmingUp, source, "첫 표본 준비 중");
        Self {
            cpu: warming("GetSystemTimes"),
            ram: warming("GlobalMemoryStatusEx"),
            gpu: warming("PDH GPU Engine"),
            disk: warming("PDH PhysicalDisk"),
            npu: warming("DXCore"),
            fan: Metric::unavailable(
                MetricState::Unsupported,
                "Fan provider",
                "현재 PC에서 검증된 실제 팬 RPM 제공 경로가 없습니다. 목표 속도를 실제 RPM으로 표시하지 않습니다.",
            ),
            memory_used_bytes: 0,
            memory_total_bytes: 0,
            disk_read_bytes_sec: warming("PDH PhysicalDisk"),
            disk_write_bytes_sec: warming("PDH PhysicalDisk"),
            gpu_adapter: String::new(),
            sequence: 0,
        }
    }
}

/// Owns four fixed workers: CPU/RAM, GPU, disk and NPU. Slow native calls never
/// cause replacement threads to be spawned. There is no telemetry network traffic.
pub struct Telemetry {
    shared: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    notify: Notification,
    workers: Vec<JoinHandle<()>>,
}

impl Telemetry {
    pub fn start() -> Self {
        let mut telemetry = Self::without_workers();
        #[cfg(windows)]
        {
            if let Err(error) = telemetry.spawn_worker("telemetry-fast", fast_worker) {
                with_snapshot(&telemetry.shared, |snapshot| {
                    snapshot.cpu =
                        Metric::unavailable(MetricState::Error, "Worker", error.to_string());
                    snapshot.ram = snapshot.cpu.clone();
                });
            }
            if let Err(error) = telemetry.spawn_worker("telemetry-gpu", |context| {
                collector_worker(
                    context,
                    SLOW_INTERVAL,
                    native::GpuProvider::new,
                    |snapshot, sample| {
                        snapshot.gpu = sample.gpu;
                        snapshot.gpu_adapter = sample.adapter;
                    },
                );
            }) {
                with_snapshot(&telemetry.shared, |snapshot| {
                    snapshot.gpu =
                        Metric::unavailable(MetricState::Error, "Worker", error.to_string());
                });
            }
            if let Err(error) = telemetry.spawn_worker("telemetry-disk", |context| {
                collector_worker(
                    context,
                    SLOW_INTERVAL,
                    native::DiskProvider::new,
                    |snapshot, sample| {
                        snapshot.disk = sample.disk;
                        snapshot.disk_read_bytes_sec = sample.read;
                        snapshot.disk_write_bytes_sec = sample.write;
                    },
                );
            }) {
                with_snapshot(&telemetry.shared, |snapshot| {
                    snapshot.disk =
                        Metric::unavailable(MetricState::Error, "Worker", error.to_string());
                    snapshot.disk_read_bytes_sec = snapshot.disk.clone();
                    snapshot.disk_write_bytes_sec = snapshot.disk.clone();
                });
            }
            if let Err(error) = telemetry.spawn_worker("telemetry-npu", |context| {
                collector_worker(
                    context,
                    SLOW_INTERVAL,
                    native::NpuProvider::new,
                    |snapshot, sample| {
                        snapshot.npu = sample;
                    },
                );
            }) {
                with_snapshot(&telemetry.shared, |snapshot| {
                    snapshot.npu =
                        Metric::unavailable(MetricState::Error, "Worker", error.to_string());
                });
            }
        }
        #[cfg(not(windows))]
        with_snapshot(&telemetry.shared, |snapshot| {
            let missing =
                Metric::unavailable(MetricState::Unsupported, "Platform", "Windows 11 전용");
            snapshot.cpu = missing.clone();
            snapshot.ram = missing.clone();
            snapshot.gpu = missing.clone();
            snapshot.disk = missing.clone();
            snapshot.disk_read_bytes_sec = missing.clone();
            snapshot.disk_write_bytes_sec = missing.clone();
            snapshot.npu = missing;
        });
        telemetry
    }

    fn without_workers() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Snapshot::default())),
            stop: Arc::new(AtomicBool::new(false)),
            generation: Arc::new(AtomicU64::new(0)),
            notify: Arc::new(Mutex::new(None)),
            workers: Vec::with_capacity(4),
        }
    }

    fn spawn_worker(
        &mut self,
        name: &str,
        run: impl FnOnce(WorkerContext) + Send + 'static,
    ) -> std::io::Result<()> {
        // Fixed at startup. A slow provider is never replaced by another thread.
        if self.workers.len() >= 4 {
            return Err(std::io::Error::other("telemetry worker limit reached"));
        }
        let context = WorkerContext {
            shared: self.shared.clone(),
            stop: self.stop.clone(),
            generation: self.generation.clone(),
            notify: self.notify.clone(),
        };
        self.workers.push(
            thread::Builder::new()
                .name(name.into())
                .spawn(move || run(context))?,
        );
        Ok(())
    }

    /// The callback runs outside the snapshot lock. It must only post a UI message,
    /// remain nonblocking, and must not call set_notify or request_stop recursively. Invocation and
    /// removal are serialized so shutdown cannot leave a copied HWND callback behind.
    pub fn set_notify(&self, callback: impl Fn() + Send + Sync + 'static) {
        let mut notification = self.notify.lock().unwrap_or_else(|e| e.into_inner());
        if !self.stop.load(Ordering::Acquire) {
            *notification = Some(Arc::new(callback));
        }
    }

    /// Disable publication and drain UI callbacks before their HWND is destroyed.
    /// Repeated requests are harmless. Native workers are only signaled here;
    /// Drop waits for them using one shared time budget.
    pub fn request_stop(&self) {
        // Holding this guard serializes simultaneous requests with callbacks and
        // makes the first request's generation invalidation visible before any
        // subsequent request returns. No native provider call holds these locks.
        let mut notification = self.notify.lock().unwrap_or_else(|e| e.into_inner());
        let already_stopped = self.stop.swap(true, Ordering::AcqRel);
        *notification = None;
        {
            let _snapshot = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if !already_stopped {
                self.generation.fetch_add(1, Ordering::AcqRel);
            }
        }
        drop(notification);
        for worker in &self.workers {
            worker.thread().unpark();
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut snapshot = self
            .shared
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let now = Instant::now();
        snapshot.cpu.expire(now, Duration::from_secs(2));
        snapshot.ram.expire(now, Duration::from_secs(2));
        for metric in [
            &mut snapshot.gpu,
            &mut snapshot.disk,
            &mut snapshot.npu,
            &mut snapshot.disk_read_bytes_sec,
            &mut snapshot.disk_write_bytes_sec,
        ] {
            metric.expire(now, Duration::from_secs(4));
        }
        snapshot
    }

    /// Call on resume, device changes or explicit refresh. No provider call is made here.
    pub fn reset_baselines(&self) {
        with_snapshot(&self.shared, |snapshot| {
            // Generation and visible baselines change together: a new-generation
            // result must not be overwritten by a reset that was waiting for this lock.
            self.generation.fetch_add(1, Ordering::Release);
            for metric in [
                &mut snapshot.cpu,
                &mut snapshot.gpu,
                &mut snapshot.disk,
                &mut snapshot.npu,
                &mut snapshot.disk_read_bytes_sec,
                &mut snapshot.disk_write_bytes_sec,
            ] {
                *metric = Metric::unavailable(
                    MetricState::WarmingUp,
                    metric.source,
                    "장치와 측정 기준을 다시 확인 중",
                );
            }
        });
        notify_changed(&self.notify, &self.stop);
        for worker in &self.workers {
            worker.thread().unpark();
        }
    }
}

impl Default for Telemetry {
    fn default() -> Self {
        Self::start()
    }
}

impl Drop for Telemetry {
    fn drop(&mut self) {
        let deadline = Instant::now() + SHUTDOWN_BUDGET;
        self.request_stop();
        wait_until(&mut self.workers, deadline);
    }
}

fn wait_until(workers: &mut Vec<JoinHandle<()>>, deadline: Instant) {
    loop {
        let mut index = 0;
        while index < workers.len() {
            if workers[index].is_finished() {
                // Even after is_finished(), join can still wait for the native
                // thread's final teardown. No borrowed state requires a join.
                drop(workers.swap_remove(index));
            } else {
                index += 1;
            }
        }
        if workers.is_empty() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            // Dropping JoinHandle detaches only. The stopped worker owns its Arc
            // state and native handles until the call and its own cleanup return.
            workers.clear();
            return;
        }
        thread::sleep(remaining.min(Duration::from_millis(2)));
    }
}

fn with_snapshot(shared: &Mutex<Snapshot>, update: impl FnOnce(&mut Snapshot)) {
    let mut snapshot = shared.lock().unwrap_or_else(|e| e.into_inner());
    update(&mut snapshot);
    snapshot.sequence = snapshot.sequence.wrapping_add(1);
}

/// A discarded pre-reset result is not a published sample or a sequence advance.
fn commit_generation(
    shared: &Mutex<Snapshot>,
    generation: &AtomicU64,
    stop: &AtomicBool,
    expected_generation: u64,
    update: impl FnOnce(&mut Snapshot),
) -> bool {
    let mut snapshot = shared.lock().unwrap_or_else(|e| e.into_inner());
    if stop.load(Ordering::Acquire) || generation.load(Ordering::Acquire) != expected_generation {
        return false;
    }
    update(&mut snapshot);
    snapshot.sequence = snapshot.sequence.wrapping_add(1);
    true
}

fn notify_changed(notify: &Notification, stop: &AtomicBool) {
    if stop.load(Ordering::Acquire) {
        return;
    }
    let callback = notify.lock().unwrap_or_else(|e| e.into_inner());
    if !stop.load(Ordering::Acquire) {
        if let Some(callback) = callback.as_ref() {
            callback();
        }
    }
}

struct WorkerContext {
    shared: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    notify: Notification,
}

trait Collector {
    type Sample;
    fn sample(&mut self) -> Self::Sample;
    fn needs_retry(&self) -> bool;
}

fn collector_worker<C: Collector>(
    context: WorkerContext,
    interval: Duration,
    create: impl FnMut() -> C,
    publish: impl Fn(&mut Snapshot, C::Sample),
) {
    collector_worker_with_clock(context, interval, create, publish, Instant::now);
}

fn collector_worker_with_clock<C: Collector>(
    context: WorkerContext,
    interval: Duration,
    mut create: impl FnMut() -> C,
    publish: impl Fn(&mut Snapshot, C::Sample),
    now: impl Fn() -> Instant,
) {
    let mut observed_generation = context.generation.load(Ordering::Acquire);
    let mut collector = create();
    let mut previous_tick = now();
    let mut retry_after = previous_tick + RETRY_INTERVAL;
    while !context.stop.load(Ordering::Acquire) {
        let mut started = now();
        let current_generation = context.generation.load(Ordering::Acquire);
        if current_generation != observed_generation
            || started.saturating_duration_since(previous_tick) > RESUME_GAP
            || (started >= retry_after && collector.needs_retry())
        {
            // Creation and destruction remain on this one provider's thread.
            collector = create();
            observed_generation = current_generation;
            // Rebase after both creation and old-provider cleanup. Their duration
            // is not another suspend gap and must not cause endless recreation.
            started = now();
            retry_after = started + RETRY_INTERVAL;
        }
        if context.stop.load(Ordering::Acquire) {
            break;
        }
        previous_tick = started;
        let sample = collector.sample();
        if commit_generation(
            &context.shared,
            &context.generation,
            &context.stop,
            observed_generation,
            |snapshot| publish(snapshot, sample),
        ) {
            notify_changed(&context.notify, &context.stop);
        }
        if !context.stop.load(Ordering::Acquire) {
            thread::park_timeout(interval.saturating_sub(now().saturating_duration_since(started)));
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct CpuTimes {
    idle: u64,
    kernel: u64,
    user: u64,
}

fn cpu_busy(previous: CpuTimes, current: CpuTimes) -> Option<f64> {
    let idle = current.idle.checked_sub(previous.idle)?;
    let kernel = current.kernel.checked_sub(previous.kernel)?;
    let user = current.user.checked_sub(previous.user)?;
    let total = kernel.checked_add(user)?;
    if total == 0 || idle > total {
        return None;
    }
    // The kernel time already includes idle. Do not add idle to the denominator.
    Some((total - idle) as f64 * 100.0 / total as f64)
}

fn memory_percent(total: u64, available: u64) -> Option<(u64, f64)> {
    if total == 0 || available > total {
        return None;
    }
    let used = total - available;
    Some((used, used as f64 * 100.0 / total as f64))
}

fn engine_busy(previous_us: u64, current_us: u64, window: Duration) -> Option<f64> {
    if window.is_zero() || window > RESUME_GAP {
        return None;
    }
    let delta = current_us.checked_sub(previous_us)?;
    Some((delta as f64 / window.as_secs_f64() / 10_000.0).clamp(0.0, 100.0))
}

/// Removes only process identity; LUID, physical adapter and engine remain in the key.
fn physical_engine_key(instance: &str) -> Option<&str> {
    let start = instance.find("luid_")?;
    let suffix = &instance[start..];
    if !suffix.contains("_phys_") || !suffix.contains("_eng_") {
        return None;
    }
    Some(suffix.split("_engtype_").next().unwrap_or(suffix))
}

fn busiest_engine(samples: &[(String, f64)]) -> Option<(String, f64)> {
    let mut engines = BTreeMap::<&str, f64>::new();
    for (name, value) in samples {
        if !value.is_finite() || *value < 0.0 {
            continue;
        }
        if let Some(key) = physical_engine_key(name) {
            *engines.entry(key).or_default() += value;
        }
    }
    engines
        .into_iter()
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(key, value)| (key.to_owned(), value.clamp(0.0, 100.0)))
}

fn busiest_disk(samples: &[(String, f64)]) -> Option<(String, f64)> {
    samples
        .iter()
        .filter(|(name, idle)| name != "_Total" && idle.is_finite() && *idle >= 0.0)
        .map(|(name, idle)| (name.clone(), (100.0 - idle).clamp(0.0, 100.0)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
}

#[cfg(windows)]
fn fast_worker(context: WorkerContext) {
    let WorkerContext {
        shared,
        stop,
        generation,
        notify,
    } = context;
    let mut previous: Option<(CpuTimes, Instant)> = None;
    let mut observed_generation = generation.load(Ordering::Acquire);
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let current_generation = generation.load(Ordering::Acquire);
        if current_generation != observed_generation {
            previous = None;
            observed_generation = current_generation;
        }
        let cpu = match native::cpu_times() {
            Ok(times) => {
                let read_at = Instant::now();
                let metric = match previous {
                    Some((last, at)) if read_at.duration_since(at) <= RESUME_GAP => {
                        match cpu_busy(last, times) {
                            Some(value) => Metric::reading(
                                value,
                                "GetSystemTimes",
                                read_at.duration_since(at),
                                "전체 논리 CPU 용량 대비 시간 기반 Busy 사용률",
                            ),
                            None => Metric::unavailable(
                                MetricState::WarmingUp,
                                "GetSystemTimes",
                                "CPU 누적 시간 변경: 새 기준 표본 수집 중",
                            ),
                        }
                    }
                    _ => Metric::unavailable(
                        MetricState::WarmingUp,
                        "GetSystemTimes",
                        "CPU 기준 표본 수집 중",
                    ),
                };
                previous = Some((times, read_at));
                metric
            }
            Err(error) => {
                previous = None;
                Metric::unavailable(MetricState::Error, "GetSystemTimes", error)
            }
        };
        if stop.load(Ordering::Acquire) {
            break;
        }
        let memory = native::physical_memory();
        let committed = commit_generation(
            &shared,
            &generation,
            &stop,
            observed_generation,
            |snapshot| {
                snapshot.cpu = cpu;
                match memory {
                    Ok((total, available)) => match memory_percent(total, available) {
                        Some((used, percent)) => {
                            snapshot.ram = Metric::reading(
                                percent,
                                "GlobalMemoryStatusEx",
                                Duration::ZERO,
                                "사용 중인 물리 메모리 / 전체 물리 메모리",
                            );
                            snapshot.memory_total_bytes = total;
                            snapshot.memory_used_bytes = used;
                        }
                        None => {
                            snapshot.ram = Metric::unavailable(
                                MetricState::Error,
                                "GlobalMemoryStatusEx",
                                "물리 메모리 범위 오류",
                            )
                        }
                    },
                    Err(error) => {
                        snapshot.ram =
                            Metric::unavailable(MetricState::Error, "GlobalMemoryStatusEx", error)
                    }
                }
            },
        );
        if committed {
            notify_changed(&notify, &stop);
        }
        if !stop.load(Ordering::Acquire) {
            thread::park_timeout(FAST_INTERVAL.saturating_sub(started.elapsed()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Condvar, mpsc};

    // Gates, rather than sleeps, establish exactly when a provider is blocked.
    // The release guard also unblocks detached threads if an assertion panics.
    #[derive(Default)]
    struct Gate {
        open: Mutex<bool>,
        changed: Condvar,
    }

    impl Gate {
        fn wait(&self) {
            let mut open = self.open.lock().unwrap();
            while !*open {
                open = self.changed.wait(open).unwrap();
            }
        }

        fn release(&self) {
            *self.open.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    struct ReleaseGate(Arc<Gate>);

    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            self.0.release();
        }
    }

    struct FakeCollector {
        value: f64,
        gate: Option<Arc<Gate>>,
        entered: Option<mpsc::Sender<()>>,
        finished: Option<mpsc::Sender<()>>,
    }

    impl Collector for FakeCollector {
        type Sample = Metric;

        fn sample(&mut self) -> Metric {
            if let Some(entered) = self.entered.take() {
                let _ = entered.send(());
            }
            if let Some(gate) = self.gate.take() {
                gate.wait();
            }
            Metric::reading(self.value, "fake", SLOW_INTERVAL, "controlled provider")
        }

        fn needs_retry(&self) -> bool {
            false
        }
    }

    impl Drop for FakeCollector {
        fn drop(&mut self) {
            if let Some(finished) = self.finished.take() {
                let _ = finished.send(());
            }
        }
    }

    #[test]
    fn blocked_gpu_does_not_stop_cpu_disk_or_npu_and_does_not_spawn_replacements() {
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGate(gate.clone());
        let (entered_tx, entered_rx) = mpsc::channel();
        let created = Arc::new(AtomicU64::new(0));
        let creations = created.clone();
        let mut telemetry = Telemetry::without_workers();
        telemetry
            .spawn_worker("test-blocked-gpu", move |context| {
                collector_worker(
                    context,
                    Duration::from_millis(1),
                    move || {
                        creations.fetch_add(1, Ordering::Relaxed);
                        FakeCollector {
                            value: 90.0,
                            gate: Some(gate.clone()),
                            entered: Some(entered_tx.clone()),
                            finished: None,
                        }
                    },
                    |snapshot, sample| snapshot.gpu = sample,
                );
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();

        let (published_tx, published_rx) = mpsc::channel();
        for sensor in 0..3 {
            let published = published_tx.clone();
            telemetry
                .spawn_worker("test-independent-provider", move |context| {
                    collector_worker(
                        context,
                        Duration::from_millis(1),
                        || FakeCollector {
                            value: 20.0 + sensor as f64,
                            gate: None,
                            entered: None,
                            finished: None,
                        },
                        move |snapshot, sample| {
                            match sensor {
                                0 => snapshot.cpu = sample,
                                1 => snapshot.disk = sample,
                                _ => snapshot.npu = sample,
                            }
                            let _ = published.send(sensor);
                        },
                    );
                })
                .unwrap();
        }
        let mut updates = [0; 3];
        while updates.iter().any(|count| *count < 2) {
            let sensor = published_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            updates[sensor] += 1;
        }
        let snapshot = telemetry.snapshot();
        assert_eq!(snapshot.cpu.value, Some(20.0));
        assert_eq!(snapshot.disk.value, Some(21.0));
        assert_eq!(snapshot.npu.value, Some(22.0));
        assert_eq!(snapshot.gpu.value, None);
        assert_eq!(created.load(Ordering::Relaxed), 1);
        assert_eq!(telemetry.workers.len(), 4);
        assert!(
            telemetry
                .spawn_worker("test-excess-worker", |_| {
                    panic!("a fifth telemetry worker must never start");
                })
                .is_err()
        );
        // Release before normal shutdown; the separate regression tests detachment.
        _release.0.release();
    }

    #[test]
    fn reset_during_blocked_collection_rejects_old_result_before_fresh_publication() {
        let old_gate = Arc::new(Gate::default());
        let fresh_gate = Arc::new(Gate::default());
        let _release_old = ReleaseGate(old_gate.clone());
        let _release_fresh = ReleaseGate(fresh_gate.clone());
        let (old_entered_tx, old_entered_rx) = mpsc::channel();
        let (fresh_entered_tx, fresh_entered_rx) = mpsc::channel();
        let (published_tx, published_rx) = mpsc::channel();
        let created = Arc::new(AtomicU64::new(0));
        let creations = created.clone();
        let mut telemetry = Telemetry::without_workers();
        telemetry
            .spawn_worker("test-reset-provider", move |context| {
                collector_worker(
                    context,
                    Duration::from_millis(1),
                    move || {
                        let incarnation = creations.fetch_add(1, Ordering::Relaxed) + 1;
                        let (gate, entered) = if incarnation == 1 {
                            (old_gate.clone(), old_entered_tx.clone())
                        } else {
                            (fresh_gate.clone(), fresh_entered_tx.clone())
                        };
                        FakeCollector {
                            value: incarnation as f64,
                            gate: Some(gate),
                            entered: Some(entered),
                            finished: None,
                        }
                    },
                    move |snapshot, sample| {
                        let value = sample.value;
                        snapshot.gpu = sample;
                        let _ = published_tx.send(value);
                    },
                );
            })
            .unwrap();
        old_entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        telemetry.reset_baselines();
        _release_old.0.release();
        fresh_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        // The old call has returned, and the new generation is blocked before
        // committing. A stale publication cannot hide behind a fresh result.
        assert!(matches!(
            published_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        let reset = telemetry.snapshot();
        assert_eq!(reset.sequence, 1);
        assert_eq!(reset.gpu.state, MetricState::WarmingUp);
        assert_eq!(reset.gpu.value, None);
        assert_eq!(created.load(Ordering::Relaxed), 2);
        _release_fresh.0.release();
        assert_eq!(
            published_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            Some(2.0)
        );
        assert_eq!(telemetry.snapshot().gpu.value, Some(2.0));
    }

    #[test]
    fn slow_provider_reinitialization_does_not_trigger_an_endless_warmup_loop() {
        let old_gate = Arc::new(Gate::default());
        let release_old = ReleaseGate(old_gate.clone());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (published_tx, published_rx) = mpsc::channel();
        let created = Arc::new(AtomicU64::new(0));
        let creations = created.clone();
        let clock_millis = Arc::new(AtomicU64::new(0));
        let init_clock = clock_millis.clone();
        let origin = Instant::now();
        let mut telemetry = Telemetry::without_workers();
        telemetry
            .spawn_worker("test-slow-initialization", move |context| {
                collector_worker_with_clock(
                    context,
                    Duration::from_millis(1),
                    move || {
                        // Model expensive native initialization without a real sleep.
                        // The next poll must measure its gap from after this work.
                        init_clock
                            .fetch_add((RESUME_GAP.as_millis() + 1_000) as u64, Ordering::Relaxed);
                        let incarnation = creations.fetch_add(1, Ordering::Relaxed) + 1;
                        FakeCollector {
                            value: incarnation as f64,
                            gate: (incarnation == 1).then(|| old_gate.clone()),
                            entered: (incarnation == 1).then(|| entered_tx.clone()),
                            finished: None,
                        }
                    },
                    move |snapshot, sample| {
                        let value = sample.value;
                        snapshot.gpu = sample;
                        let _ = published_tx.send(value);
                    },
                    move || origin + Duration::from_millis(clock_millis.load(Ordering::Relaxed)),
                );
            })
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        telemetry.reset_baselines();
        release_old.0.release();
        // Two later polls must use the same recreated provider. The former bug
        // recreated it on every poll because initialization exceeded RESUME_GAP.
        for _ in 0..2 {
            assert_eq!(
                published_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
                Some(2.0)
            );
        }
        assert_eq!(created.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn shutdown_has_one_budget_and_late_collectors_cannot_publish_or_notify() {
        let gate = Arc::new(Gate::default());
        let _release = ReleaseGate(gate.clone());
        let early_gate = Arc::new(Gate::default());
        let release_early = ReleaseGate(early_gate.clone());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let callbacks = Arc::new(AtomicU64::new(0));
        let notification_count = callbacks.clone();
        let mut telemetry = Telemetry::without_workers();
        telemetry.set_notify(move || {
            notification_count.fetch_add(1, Ordering::Relaxed);
        });
        for sensor in 0..4 {
            let gate = if sensor == 0 {
                early_gate.clone()
            } else {
                gate.clone()
            };
            let entered = entered_tx.clone();
            let finished = finished_tx.clone();
            telemetry
                .spawn_worker("test-shutdown-provider", move |context| {
                    collector_worker(
                        context,
                        Duration::from_millis(1),
                        move || FakeCollector {
                            value: 99.0,
                            gate: Some(gate.clone()),
                            entered: Some(entered.clone()),
                            finished: Some(finished.clone()),
                        },
                        move |snapshot, sample| match sensor {
                            0 => snapshot.cpu = sample,
                            1 => snapshot.gpu = sample,
                            2 => snapshot.disk = sample,
                            _ => snapshot.npu = sample,
                        },
                    );
                })
                .unwrap();
        }
        for _ in 0..4 {
            entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let shared = telemetry.shared.clone();
        let generation = telemetry.generation.clone();
        let stop = telemetry.stop.clone();
        let notify = telemetry.notify.clone();
        telemetry.request_stop();
        telemetry.request_stop();
        telemetry.set_notify(|| panic!("a stopped callback must not be reinstalled"));
        notify_changed(&notify, &stop);
        // This collector returns while Telemetry is still alive, after the caller
        // has stopped publication but before the later, bounded Drop wait.
        release_early.0.release();
        finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(shared.lock().unwrap().sequence, 0);
        assert_eq!(callbacks.load(Ordering::Relaxed), 0);
        assert_eq!(generation.load(Ordering::Acquire), 1);
        let started = Instant::now();
        drop(telemetry);
        // Allow scheduling jitter while still rejecting three per-worker budgets.
        assert!(started.elapsed() < SHUTDOWN_BUDGET + Duration::from_millis(500));
        assert!(stop.load(Ordering::Acquire));
        assert_eq!(generation.load(Ordering::Acquire), 1);
        assert!(notify.lock().unwrap().is_none());
        assert!(matches!(
            finished_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        _release.0.release();
        for _ in 0..3 {
            finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        let snapshot = shared.lock().unwrap();
        assert_eq!(snapshot.sequence, 0);
        for metric in [&snapshot.cpu, &snapshot.gpu, &snapshot.disk, &snapshot.npu] {
            assert_eq!(metric.value, None);
            assert_eq!(metric.state, MetricState::WarmingUp);
        }
        assert_eq!(callbacks.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn cpu_counts_idle_once_and_normalizes_all_cores() {
        let first = CpuTimes {
            idle: 100,
            kernel: 200,
            user: 50,
        };
        let second = CpuTimes {
            idle: 130,
            kernel: 250,
            user: 100,
        };
        assert_eq!(cpu_busy(first, second), Some(70.0));
    }

    #[test]
    fn cpu_reset_and_missing_window_are_not_zero_load() {
        let first = CpuTimes {
            idle: 100,
            kernel: 200,
            user: 50,
        };
        assert_eq!(cpu_busy(first, first), None);
        assert_eq!(
            cpu_busy(
                first,
                CpuTimes {
                    idle: 0,
                    kernel: 0,
                    user: 0
                }
            ),
            None
        );
        let zero = CpuTimes {
            idle: 0,
            kernel: 0,
            user: 0,
        };
        assert_eq!(
            cpu_busy(
                zero,
                CpuTimes {
                    idle: 0,
                    kernel: u64::MAX,
                    user: 1
                }
            ),
            None
        );
        assert_eq!(
            cpu_busy(
                zero,
                CpuTimes {
                    idle: u64::MAX,
                    kernel: u64::MAX,
                    user: 0
                }
            ),
            Some(0.0)
        );
        assert_eq!(
            cpu_busy(
                zero,
                CpuTimes {
                    idle: 0,
                    kernel: u64::MAX,
                    user: 0
                }
            ),
            Some(100.0)
        );
        assert_eq!(
            cpu_busy(
                first,
                CpuTimes {
                    idle: 300,
                    kernel: 201,
                    user: 51
                }
            ),
            None
        );
    }

    #[test]
    fn memory_rejects_invalid_or_unknown_capacity() {
        assert_eq!(memory_percent(1_000, 250), Some((750, 75.0)));
        assert_eq!(memory_percent(0, 0), None);
        assert_eq!(memory_percent(100, 101), None);
        assert_eq!(memory_percent(u64::MAX, u64::MAX), Some((0, 0.0)));
        assert_eq!(memory_percent(u64::MAX, 0), Some((u64::MAX, 100.0)));
    }

    #[test]
    fn engine_uses_microseconds_and_restarts_after_reset_or_suspend() {
        assert_eq!(
            engine_busy(500, 250_500, Duration::from_millis(500)),
            Some(50.0)
        );
        assert_eq!(engine_busy(500, 500, Duration::from_secs(1)), Some(0.0));
        assert_eq!(engine_busy(500, 0, Duration::from_secs(1)), None);
        assert_eq!(engine_busy(0, 500, Duration::ZERO), None);
        assert_eq!(engine_busy(0, 500, Duration::from_secs(30)), None);
        assert_eq!(engine_busy(0, 3_000_000, RESUME_GAP), Some(100.0));
        assert_eq!(
            engine_busy(0, 3_000_000, RESUME_GAP + Duration::from_nanos(1)),
            None
        );
        assert_eq!(
            engine_busy(0, u64::MAX, Duration::from_nanos(1)),
            Some(100.0)
        );
    }

    #[test]
    fn gpu_sums_processes_per_engine_before_finding_busiest() {
        let data = vec![
            ("pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3D".into(), 35.0),
            ("pid_2_luid_0x0_0x1_phys_0_eng_0_engtype_3D".into(), 40.0),
            ("pid_2_luid_0x0_0x1_phys_0_eng_1_engtype_Copy".into(), 60.0),
            ("pid_3_luid_0x0_0x2_phys_0_eng_0_engtype_3D".into(), 65.0),
        ];
        let (engine, percent) = busiest_engine(&data).unwrap();
        assert_eq!(engine, "luid_0x0_0x1_phys_0_eng_0");
        assert_eq!(percent, 75.0);
    }

    #[test]
    fn gpu_does_not_conflate_absence_invalid_values_and_real_zero() {
        assert!(busiest_engine(&[]).is_none());
        assert!(busiest_engine(&[("unknown".into(), 20.0)]).is_none());
        let name = "pid_1_luid_a_b_phys_0_eng_0_engtype_3D".to_owned();
        assert!(busiest_engine(&[(name.clone(), f64::NAN)]).is_none());
        assert_eq!(busiest_engine(&[(name.clone(), 0.0)]).unwrap().1, 0.0);
        assert_eq!(busiest_engine(&[(name, 120.0)]).unwrap().1, 100.0);
    }

    #[test]
    fn disk_uses_idle_complement_and_ignores_total() {
        let sample = vec![
            ("_Total".into(), 20.0),
            ("0 C:".into(), 90.0),
            ("1 D:".into(), 70.0),
        ];
        assert_eq!(busiest_disk(&sample), Some(("1 D:".into(), 30.0)));
    }

    #[test]
    fn bad_provider_values_cannot_become_ready_samples_or_poison_valid_devices() {
        let engine = "pid_1_luid_a_b_phys_0_eng_0_engtype_3D".to_owned();
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0] {
            let reading = Metric::reading(value, "test", FAST_INTERVAL, "invalid input");
            assert_eq!(reading.state, MetricState::Error);
            assert_eq!(reading.value, None);
            assert_eq!(busiest_engine(&[(engine.clone(), value)]), None);
            assert_eq!(busiest_disk(&[("0 C:".into(), value)]), None);
            let valid_engine =
                busiest_engine(&[(engine.clone(), value), (engine.clone(), 25.0)]).unwrap();
            assert_eq!(valid_engine.1, 25.0);
            assert_eq!(
                busiest_disk(&[("0 C:".into(), value), ("1 D:".into(), 75.0)]),
                Some(("1 D:".into(), 25.0))
            );
        }
        let zero = Metric::reading(0.0, "test", FAST_INTERVAL, "idle is a valid measurement");
        assert_eq!(zero.state, MetricState::Ready);
        assert_eq!(zero.value, Some(0.0));
        assert!(busiest_disk(&[("_Total".into(), 0.0)]).is_none());
        // Finite extreme counters are bounded before they leave the aggregator.
        assert_eq!(
            busiest_engine(&[(engine.clone(), f64::MAX), (engine, f64::MAX)])
                .unwrap()
                .1,
            100.0
        );
    }

    #[test]
    fn reset_rejects_old_worker_results_without_fabricating_sequence_progress() {
        // No workers or Windows providers are started: publication order is driven
        // explicitly so this regression does not depend on scheduler timing.
        let ready = Metric::reading(73.0, "test", SLOW_INTERVAL, "old baseline");
        let telemetry = Telemetry {
            shared: Arc::new(Mutex::new(Snapshot {
                cpu: ready.clone(),
                gpu: ready.clone(),
                disk: ready.clone(),
                npu: ready.clone(),
                disk_read_bytes_sec: ready.clone(),
                disk_write_bytes_sec: ready,
                ram: Metric::reading(40.0, "memory", Duration::ZERO, "point reading"),
                sequence: 20,
                ..Snapshot::default()
            })),
            generation: Arc::new(AtomicU64::new(7)),
            stop: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(Mutex::new(None)),
            workers: Vec::new(),
        };
        let notified = Arc::new(AtomicU64::new(0));
        let callback_count = notified.clone();
        let shared = telemetry.shared.clone();
        telemetry.set_notify(move || {
            assert!(
                shared.try_lock().is_ok(),
                "notification held the snapshot lock"
            );
            callback_count.fetch_add(1, Ordering::Relaxed);
        });
        telemetry.reset_baselines();
        let reset = telemetry.shared.lock().unwrap().clone();
        assert_eq!(reset.sequence, 21);
        assert_eq!(telemetry.generation.load(Ordering::Acquire), 8);
        for metric in [
            &reset.cpu,
            &reset.gpu,
            &reset.disk,
            &reset.npu,
            &reset.disk_read_bytes_sec,
            &reset.disk_write_bytes_sec,
        ] {
            assert_eq!(metric.state, MetricState::WarmingUp);
            assert_eq!(metric.value, None);
        }
        assert_eq!(reset.ram.value, Some(40.0));
        assert_eq!(reset.fan.state, MetricState::Unsupported);
        assert_eq!(notified.load(Ordering::Relaxed), 1);
        assert!(!commit_generation(
            &telemetry.shared,
            &telemetry.generation,
            &telemetry.stop,
            7,
            |_| {
                panic!("a pre-reset worker must not publish");
            }
        ));
        assert_eq!(telemetry.shared.lock().unwrap().sequence, 21);
        assert!(commit_generation(
            &telemetry.shared,
            &telemetry.generation,
            &telemetry.stop,
            8,
            |snapshot| {
                snapshot.cpu = Metric::reading(0.0, "test", FAST_INTERVAL, "fresh baseline");
            }
        ));
        let fresh = telemetry.shared.lock().unwrap();
        assert_eq!(fresh.sequence, 22);
        assert_eq!(fresh.cpu.state, MetricState::Ready);
        assert_eq!(fresh.cpu.value, Some(0.0));
    }

    #[test]
    fn stale_value_is_retained_but_its_state_is_not_ready() {
        let mut metric = Metric::reading(0.0, "test", FAST_INTERVAL, "");
        let sampled_at = metric.sampled_at;
        metric.expire(sampled_at + Duration::from_secs(2), Duration::from_secs(2));
        assert_eq!(metric.state, MetricState::Ready);
        metric.expire(
            sampled_at + Duration::from_secs(2) + Duration::from_nanos(1),
            Duration::from_secs(2),
        );
        assert_eq!(metric.state, MetricState::Stale);
        assert_eq!(metric.value, Some(0.0));
        assert_eq!(metric.sampled_at, sampled_at);
        assert_eq!(metric.window, FAST_INTERVAL);
        for state in [
            MetricState::NotPresent,
            MetricState::Unsupported,
            MetricState::Error,
        ] {
            let mut absent = Metric::unavailable(state, "test", "");
            absent.expire(
                absent.sampled_at + Duration::from_secs(60),
                Duration::from_secs(2),
            );
            assert_eq!(absent.state, state);
            assert_eq!(absent.value, None);
        }
        let mut future = Metric::reading(12.0, "test", FAST_INTERVAL, "");
        future.sampled_at = sampled_at + Duration::from_secs(10);
        future.expire(sampled_at, Duration::from_secs(2));
        assert_eq!(future.state, MetricState::Ready);
        let mut warming = Metric::unavailable(MetricState::WarmingUp, "test", "");
        warming.expire(
            warming.sampled_at + Duration::from_secs(3),
            Duration::from_secs(2),
        );
        assert_eq!(warming.state, MetricState::Stale);
        assert_eq!(warming.value, None);
    }
}
