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

/// Owns two bounded workers. There is no telemetry network traffic or process inventory.
pub struct Telemetry {
    shared: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    notify: Notification,
    workers: Vec<JoinHandle<()>>,
}

impl Telemetry {
    pub fn start() -> Self {
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let generation = Arc::new(AtomicU64::new(0));
        let notify: Notification = Arc::new(Mutex::new(None));
        let mut workers = Vec::new();

        #[cfg(windows)]
        {
            let state = shared.clone();
            let stopping = stop.clone();
            let version = generation.clone();
            let notification = notify.clone();
            match thread::Builder::new()
                .name("telemetry-fast".into())
                .spawn(move || {
                    fast_worker(state, stopping, version, notification);
                }) {
                Ok(worker) => workers.push(worker),
                Err(error) => with_snapshot(&shared, |snapshot| {
                    snapshot.cpu =
                        Metric::unavailable(MetricState::Error, "Worker", error.to_string());
                    snapshot.ram = snapshot.cpu.clone();
                }),
            }
            let state = shared.clone();
            let stopping = stop.clone();
            let version = generation.clone();
            let notification = notify.clone();
            match thread::Builder::new()
                .name("telemetry-devices".into())
                .spawn(move || {
                    slow_worker(state, stopping, version, notification);
                }) {
                Ok(worker) => workers.push(worker),
                Err(error) => with_snapshot(&shared, |snapshot| {
                    snapshot.gpu =
                        Metric::unavailable(MetricState::Error, "Worker", error.to_string());
                    snapshot.disk = snapshot.gpu.clone();
                    snapshot.npu = snapshot.gpu.clone();
                    snapshot.disk_read_bytes_sec = snapshot.gpu.clone();
                    snapshot.disk_write_bytes_sec = snapshot.gpu.clone();
                }),
            }
        }

        #[cfg(not(windows))]
        with_snapshot(&shared, |snapshot| {
            let missing =
                Metric::unavailable(MetricState::Unsupported, "Platform", "Windows 11 전용");
            snapshot.cpu = missing.clone();
            snapshot.ram = missing.clone();
            snapshot.gpu = missing.clone();
            snapshot.disk = missing.clone();
            snapshot.npu = missing;
        });

        Self {
            shared,
            stop,
            generation,
            notify,
            workers,
        }
    }

    /// The callback runs on collector threads outside the snapshot lock; post a UI message.
    pub fn set_notify(&self, callback: impl Fn() + Send + Sync + 'static) {
        *self.notify.lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(callback));
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

    /// Call on resume, device changes or explicit refresh. Neither worker is blocked here.
    pub fn reset_baselines(&self) {
        self.generation.fetch_add(1, Ordering::Release);
        with_snapshot(&self.shared, |snapshot| {
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
        notify_changed(&self.notify);
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
        self.stop.store(true, Ordering::Release);
        for worker in &self.workers {
            worker.thread().unpark();
        }
        for worker in self.workers.drain(..) {
            let _ = worker.join();
        }
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
    expected_generation: u64,
    update: impl FnOnce(&mut Snapshot),
) -> bool {
    let mut snapshot = shared.lock().unwrap_or_else(|e| e.into_inner());
    if generation.load(Ordering::Acquire) != expected_generation {
        return false;
    }
    update(&mut snapshot);
    snapshot.sequence = snapshot.sequence.wrapping_add(1);
    true
}

fn notify_changed(notify: &Notification) {
    let callback = notify.lock().unwrap_or_else(|e| e.into_inner()).clone();
    if let Some(callback) = callback {
        callback();
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
fn fast_worker(
    shared: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    notify: Notification,
) {
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
        let memory = native::physical_memory();
        let committed = commit_generation(&shared, &generation, observed_generation, |snapshot| {
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
        });
        if committed {
            notify_changed(&notify);
        }
        thread::park_timeout(FAST_INTERVAL.saturating_sub(started.elapsed()));
    }
}

#[cfg(windows)]
fn slow_worker(
    shared: Arc<Mutex<Snapshot>>,
    stop: Arc<AtomicBool>,
    generation: Arc<AtomicU64>,
    notify: Notification,
) {
    let mut observed_generation = generation.load(Ordering::Acquire);
    let mut providers = native::DeviceProviders::new();
    let mut previous_tick = Instant::now();
    let mut retry_after = Instant::now() + Duration::from_secs(30);
    while !stop.load(Ordering::Acquire) {
        let started = Instant::now();
        let current_generation = generation.load(Ordering::Acquire);
        if current_generation != observed_generation
            || started.duration_since(previous_tick) > RESUME_GAP
        {
            providers = native::DeviceProviders::new();
            observed_generation = current_generation;
            retry_after = Instant::now() + Duration::from_secs(30);
        } else if started >= retry_after && providers.needs_retry() {
            providers = native::DeviceProviders::new();
            retry_after = Instant::now() + Duration::from_secs(30);
        }
        previous_tick = started;
        let sample = providers.sample();
        let committed = commit_generation(&shared, &generation, observed_generation, |snapshot| {
            snapshot.gpu = sample.gpu;
            snapshot.disk = sample.disk;
            snapshot.npu = sample.npu;
            snapshot.disk_read_bytes_sec = sample.read;
            snapshot.disk_write_bytes_sec = sample.write;
            snapshot.gpu_adapter = sample.gpu_adapter;
        });
        if committed {
            notify_changed(&notify);
        }
        thread::park_timeout(SLOW_INTERVAL.saturating_sub(started.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            7,
            |_| {
                panic!("a pre-reset worker must not publish");
            }
        ));
        assert_eq!(telemetry.shared.lock().unwrap().sequence, 21);
        assert!(commit_generation(
            &telemetry.shared,
            &telemetry.generation,
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
