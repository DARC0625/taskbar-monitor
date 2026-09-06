//! Bounded, passive telemetry diagnostics. This mode never creates a window.

use crate::telemetry::{Metric, MetricState, Snapshot, Telemetry};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const POLL_INTERVAL: Duration = Duration::from_millis(250);

struct Options {
    seconds: u64,
    out: PathBuf,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut result = Self {
            seconds: 15,
            out: default_output_path(),
        };
        let mut index = 1;
        while index < args.len() {
            match args[index].as_str() {
                "--probe" => {}
                "--seconds" => {
                    index += 1;
                    result.seconds = args
                        .get(index)
                        .ok_or("--seconds requires an integer between 1 and 300")?
                        .parse::<u64>()
                        .map_err(|_| "--seconds requires an integer between 1 and 300")?;
                    if !(1..=300).contains(&result.seconds) {
                        return Err("--seconds must be between 1 and 300".into());
                    }
                }
                "--out" => {
                    index += 1;
                    let value = args
                        .get(index)
                        .filter(|value| !value.is_empty())
                        .ok_or("--out requires a file path")?;
                    result.out = PathBuf::from(value);
                }
                _ => {
                    return Err(
                        "Unknown probe argument. Use --probe --seconds 15 --out PATH".into(),
                    );
                }
            }
            index += 1;
        }
        Ok(result)
    }
}

fn default_output_path() -> PathBuf {
    crate::config::diagnostics_dir().join("probe-report.json")
}

/// Prints a report and always attempts to save it, including release GUI-subsystem builds.
pub fn probe(args: &[String]) {
    let options = match Options::parse(args) {
        Ok(options) => options,
        Err(error) => {
            let out = args
                .windows(2)
                .find(|pair| pair[0] == "--out")
                .map(|pair| PathBuf::from(&pair[1]))
                .unwrap_or_else(default_output_path);
            let report = json!({
                "schema_version": 1,
                "status": "invalid_arguments",
                "timestamp_unix_ms": unix_ms(),
                "error": error,
            });
            emit_report(&report, &out);
            std::process::exit(2);
        }
    };

    let start_timestamp = unix_ms();
    let started = Instant::now();
    let cpu_start = process::cpu_time_100ns();
    let logical_processors = process::logical_processors();
    let mut memory = MemorySummary::default();
    memory.sample();
    let telemetry = Telemetry::start();
    let deadline = started + Duration::from_secs(options.seconds);
    let mut ranges = Ranges::default();
    let mut observations = 0_u64;
    let mut first_sequence = None;
    let last: Snapshot;

    loop {
        let snapshot = telemetry.snapshot();
        first_sequence.get_or_insert(snapshot.sequence);
        ranges.observe(&snapshot);
        observations += 1;
        memory.sample();
        let now = Instant::now();
        if now >= deadline {
            last = snapshot;
            break;
        }
        thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
    }

    let measured_at = Instant::now();
    let measured_seconds = measured_at.saturating_duration_since(started).as_secs_f64();
    let cpu_end = process::cpu_time_100ns();
    let (cpu_seconds, cpu_percent, cpu_error) = match (cpu_start, cpu_end) {
        (Ok(start), Ok(end)) if end >= start => {
            let seconds = (end - start) as f64 / 10_000_000.0;
            (
                Some(seconds),
                normalized_cpu_percent(seconds, measured_seconds, logical_processors),
                None,
            )
        }
        (Err(error), _) | (_, Err(error)) => (None, None, Some(error)),
        _ => (
            None,
            None,
            Some("Process CPU counter moved backwards".into()),
        ),
    };

    let report = json!({
        "schema_version": 1,
        "status": "completed",
        "mode": "passive_telemetry_probe",
        "started_unix_ms": start_timestamp,
        "completed_unix_ms": unix_ms(),
        "requested_duration_seconds": options.seconds,
        "measured_duration_seconds": measured_seconds,
        "poll_interval_ms": POLL_INTERVAL.as_millis() as u64,
        "snapshots_observed": observations,
        "first_sequence": first_sequence,
        "last_sequence": last.sequence,
        "last_snapshot": snapshot_json(&last, measured_at),
        "sample_ranges": ranges.to_json(),
        "support_status": {
            "cpu": state_code(last.cpu.state),
            "ram": state_code(last.ram.state),
            "gpu": state_code(last.gpu.state),
            "disk": state_code(last.disk.state),
            "npu": state_code(last.npu.state),
            "fan": state_code(last.fan.state),
        },
        "process": {
            "scope": "This probe process including telemetry workers; no widget renderer",
            "logical_processor_count": logical_processors,
            "cpu_time_seconds_during_probe": cpu_seconds,
            "cpu_average_percent_all_logical_processors": cpu_percent,
            "cpu_error": cpu_error,
            "memory": memory.to_json(),
        },
        "validation": {
            "sequence_advanced": first_sequence.is_some_and(|first| last.sequence > first),
            "cpu_has_valid_sample": ranges.cpu.count > 0,
            "ram_has_valid_sample": ranges.ram.count > 0,
            "memory_used_not_above_total": (last.memory_total_bytes > 0)
                .then_some(last.memory_used_bytes <= last.memory_total_bytes),
            "percentage_ranges_within_0_100": ranges.percent_ranges_valid(),
        },
        "measurement_limits": [
            "Passive observation only. No synthetic CPU, GPU, disk, NPU or fan load was generated.",
            "A successful read does not independently verify sensor accuracy or hardware compatibility.",
            "Unique ready samples determine range statistics. Repeated snapshots and unavailable values are excluded.",
            "The mean is an unweighted mean of unique samples, not a time-weighted utilization average.",
            "Process CPU covers initialization, telemetry and probe collection; it excludes UI drawing and final JSON writing.",
            "Sampled private-memory peak may miss peaks between 250 ms polls. Process-lifetime private-commit and working-set peaks use Windows peak counters.",
            "A short run does not establish 24-hour stability, suspend/resume behavior or Explorer restart recovery.",
            "Unsupported, missing, stale and failed sensors are states, not zero-valued measurements."
        ],
    });
    emit_report(&report, &options.out);
}

fn emit_report(report: &Value, path: &std::path::Path) {
    let text = match serde_json::to_string_pretty(report) {
        Ok(text) => text + "\n",
        Err(error) => {
            eprintln!("Could not serialize diagnostic report: {error}");
            std::process::exit(3);
        }
    };
    println!("{text}");
    let write = (|| -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)
    })();
    if let Err(error) = write {
        eprintln!("Could not save diagnostic report: {error}");
        std::process::exit(3);
    }
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn state_code(state: MetricState) -> &'static str {
    match state {
        MetricState::Ready => "ready",
        MetricState::WarmingUp => "warming_up",
        MetricState::NotPresent => "not_present",
        MetricState::Unsupported => "unsupported",
        MetricState::Error => "error",
        MetricState::Stale => "stale",
    }
}

fn metric_json(metric: &Metric, now: Instant, unit: &str) -> Value {
    let finite_value = metric.value.filter(|value| value.is_finite());
    json!({
        "state": state_code(metric.state),
        "state_label": metric.state.label(),
        "value": if metric.state == MetricState::Ready { finite_value } else { None },
        "last_value": finite_value,
        "unit": unit,
        "source": metric.source,
        "detail": metric.detail,
        "sample_age_ms": now.saturating_duration_since(metric.sampled_at).as_secs_f64() * 1000.0,
        "sample_window_ms": metric.window.as_secs_f64() * 1000.0,
    })
}

fn snapshot_json(snapshot: &Snapshot, now: Instant) -> Value {
    json!({
        "sequence": snapshot.sequence,
        "cpu": metric_json(&snapshot.cpu, now, "percent"),
        "ram": metric_json(&snapshot.ram, now, "percent"),
        "gpu": metric_json(&snapshot.gpu, now, "percent"),
        "disk": metric_json(&snapshot.disk, now, "percent"),
        "npu": metric_json(&snapshot.npu, now, "percent"),
        "fan": metric_json(&snapshot.fan, now, "rpm"),
        "memory_used_bytes": snapshot.memory_used_bytes,
        "memory_total_bytes": snapshot.memory_total_bytes,
        "disk_read": metric_json(&snapshot.disk_read_bytes_sec, now, "bytes_per_second"),
        "disk_write": metric_json(&snapshot.disk_write_bytes_sec, now, "bytes_per_second"),
        "gpu_adapter": snapshot.gpu_adapter,
    })
}

#[derive(Default)]
struct SampleRange {
    count: u64,
    min: Option<f64>,
    max: Option<f64>,
    sum: f64,
    last_seen: Option<Instant>,
    invalid_ready_observations: u64,
    states: BTreeMap<&'static str, u64>,
}

impl SampleRange {
    fn observe(&mut self, metric: &Metric) {
        *self.states.entry(state_code(metric.state)).or_default() += 1;
        if metric.state != MetricState::Ready {
            return;
        }
        let Some(value) = metric
            .value
            .filter(|value| value.is_finite() && *value >= 0.0)
        else {
            self.invalid_ready_observations += 1;
            return;
        };
        if self.last_seen == Some(metric.sampled_at) {
            return;
        }
        self.last_seen = Some(metric.sampled_at);
        self.count += 1;
        self.sum += value;
        self.min = Some(self.min.map_or(value, |min| min.min(value)));
        self.max = Some(self.max.map_or(value, |max| max.max(value)));
    }

    fn to_json(&self, unit: &str) -> Value {
        json!({
            "unit": unit,
            "unique_ready_samples": self.count,
            "min": self.min,
            "max": self.max,
            "mean": if self.count > 0 { Some(self.sum / self.count as f64) } else { None },
            "invalid_ready_observations": self.invalid_ready_observations,
            "state_observations": self.states,
        })
    }

    fn valid_percentage(&self) -> Option<bool> {
        if self.invalid_ready_observations > 0 {
            Some(false)
        } else {
            self.max.map(|max| max <= 100.0)
        }
    }
}

#[derive(Default)]
struct Ranges {
    cpu: SampleRange,
    ram: SampleRange,
    gpu: SampleRange,
    disk: SampleRange,
    npu: SampleRange,
    fan: SampleRange,
    disk_read: SampleRange,
    disk_write: SampleRange,
}

impl Ranges {
    fn observe(&mut self, snapshot: &Snapshot) {
        self.cpu.observe(&snapshot.cpu);
        self.ram.observe(&snapshot.ram);
        self.gpu.observe(&snapshot.gpu);
        self.disk.observe(&snapshot.disk);
        self.npu.observe(&snapshot.npu);
        self.fan.observe(&snapshot.fan);
        self.disk_read.observe(&snapshot.disk_read_bytes_sec);
        self.disk_write.observe(&snapshot.disk_write_bytes_sec);
    }

    fn to_json(&self) -> Value {
        json!({
            "cpu": self.cpu.to_json("percent"),
            "ram": self.ram.to_json("percent"),
            "gpu": self.gpu.to_json("percent"),
            "disk": self.disk.to_json("percent"),
            "npu": self.npu.to_json("percent"),
            "fan": self.fan.to_json("rpm"),
            "disk_read": self.disk_read.to_json("bytes_per_second"),
            "disk_write": self.disk_write.to_json("bytes_per_second"),
        })
    }

    fn percent_ranges_valid(&self) -> Value {
        json!({
            "cpu": self.cpu.valid_percentage(),
            "ram": self.ram.valid_percentage(),
            "gpu": self.gpu.valid_percentage(),
            "disk": self.disk.valid_percentage(),
            "npu": self.npu.valid_percentage(),
        })
    }
}

fn normalized_cpu_percent(
    cpu_seconds: f64,
    wall_seconds: f64,
    logical_processors: u32,
) -> Option<f64> {
    if logical_processors == 0 || wall_seconds <= 0.0 {
        return None;
    }
    let value = 100.0 * cpu_seconds / wall_seconds / f64::from(logical_processors);
    value.is_finite().then_some(value)
}

#[derive(Default)]
struct MemorySummary {
    successful_samples: u64,
    failed_samples: u64,
    last_error: Option<String>,
    last_private_bytes: Option<u64>,
    sampled_peak_private_bytes: Option<u64>,
    process_peak_private_bytes: Option<u64>,
    last_working_set_bytes: Option<u64>,
    process_peak_working_set_bytes: Option<u64>,
}

impl MemorySummary {
    fn sample(&mut self) {
        match process::memory() {
            Ok(memory) => {
                self.successful_samples += 1;
                self.last_private_bytes = Some(memory.private_bytes);
                self.sampled_peak_private_bytes = Some(
                    self.sampled_peak_private_bytes
                        .unwrap_or(0)
                        .max(memory.private_bytes),
                );
                self.process_peak_private_bytes = Some(memory.peak_private_bytes);
                self.last_working_set_bytes = Some(memory.working_set_bytes);
                self.process_peak_working_set_bytes = Some(memory.peak_working_set_bytes);
            }
            Err(error) => {
                self.failed_samples += 1;
                self.last_error = Some(error);
            }
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "successful_samples": self.successful_samples,
            "failed_samples": self.failed_samples,
            "last_error": self.last_error,
            "last_private_bytes": self.last_private_bytes,
            "sampled_peak_private_bytes": self.sampled_peak_private_bytes,
            "process_lifetime_peak_private_bytes": self.process_peak_private_bytes,
            "last_working_set_bytes": self.last_working_set_bytes,
            "process_lifetime_peak_working_set_bytes": self.process_peak_working_set_bytes,
        })
    }
}

#[cfg(windows)]
mod process {
    use std::ffi::c_void;

    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[repr(C)]
    #[derive(Default)]
    struct MemoryCounters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
        private_usage: usize,
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentProcess() -> *mut c_void;
        fn GetProcessTimes(
            process: *mut c_void,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        fn K32GetProcessMemoryInfo(
            process: *mut c_void,
            counters: *mut MemoryCounters,
            size: u32,
        ) -> i32;
        fn GetActiveProcessorCount(group: u16) -> u32;
    }

    pub struct Memory {
        pub private_bytes: u64,
        pub peak_private_bytes: u64,
        pub working_set_bytes: u64,
        pub peak_working_set_bytes: u64,
    }

    pub fn cpu_time_100ns() -> Result<u64, String> {
        let (mut creation, mut exit, mut kernel, mut user) = (
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
            FileTime::default(),
        );
        // All pointers target correctly sized stack values; the pseudo-handle is process-local.
        let success = unsafe {
            GetProcessTimes(
                GetCurrentProcess(),
                &mut creation,
                &mut exit,
                &mut kernel,
                &mut user,
            )
        };
        if success == 0 {
            return Err(format!(
                "GetProcessTimes: {}",
                std::io::Error::last_os_error()
            ));
        }
        let ticks = |value: FileTime| (u64::from(value.high) << 32) | u64::from(value.low);
        Ok(ticks(kernel).saturating_add(ticks(user)))
    }

    pub fn logical_processors() -> u32 {
        // ALL_PROCESSOR_GROUPS counts logical processors across Windows processor groups.
        let count = unsafe { GetActiveProcessorCount(0xffff) };
        if count > 0 { count } else { 0 }
    }

    pub fn memory() -> Result<Memory, String> {
        let mut counters = MemoryCounters {
            cb: std::mem::size_of::<MemoryCounters>() as u32,
            ..MemoryCounters::default()
        };
        // PROCESS_MEMORY_COUNTERS_EX ABI: cb and the buffer size match the complete struct.
        let success = unsafe {
            K32GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut counters,
                std::mem::size_of::<MemoryCounters>() as u32,
            )
        };
        if success == 0 {
            return Err(format!(
                "GetProcessMemoryInfo: {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(Memory {
            private_bytes: counters.private_usage as u64,
            peak_private_bytes: counters.peak_pagefile_usage as u64,
            working_set_bytes: counters.working_set_size as u64,
            peak_working_set_bytes: counters.peak_working_set_size as u64,
        })
    }
}

#[cfg(not(windows))]
mod process {
    pub struct Memory {
        pub private_bytes: u64,
        pub peak_private_bytes: u64,
        pub working_set_bytes: u64,
        pub peak_working_set_bytes: u64,
    }
    pub fn cpu_time_100ns() -> Result<u64, String> {
        Err("Windows API unavailable".into())
    }
    pub fn logical_processors() -> u32 {
        0
    }
    pub fn memory() -> Result<Memory, String> {
        Err("Windows API unavailable".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(value: f64, sampled_at: Instant) -> Metric {
        Metric {
            value: Some(value),
            state: MetricState::Ready,
            detail: String::new(),
            source: "test",
            sampled_at,
            window: Duration::from_secs(1),
        }
    }

    #[test]
    fn ranges_deduplicate_snapshots_and_exclude_stale_values() {
        let mut range = SampleRange::default();
        let start = Instant::now();
        let first = ready(10.0, start);
        range.observe(&first);
        range.observe(&first);
        let mut stale = ready(90.0, start + Duration::from_secs(1));
        stale.state = MetricState::Stale;
        range.observe(&stale);
        range.observe(&ready(30.0, start + Duration::from_secs(2)));
        assert_eq!(range.count, 2);
        assert_eq!(range.to_json("percent")["mean"], 20.0);
        assert_eq!(range.states["stale"], 1);
    }

    #[test]
    fn unavailable_ranges_are_null_and_invalid_ready_values_are_counted() {
        let mut range = SampleRange::default();
        range.observe(&Metric::unavailable(
            MetricState::Unsupported,
            "test",
            "unavailable",
        ));
        range.observe(&ready(f64::NAN, Instant::now()));
        assert_eq!(range.count, 0);
        assert_eq!(range.invalid_ready_observations, 1);
        assert!(range.to_json("percent")["mean"].is_null());
    }

    #[test]
    fn cpu_percentage_is_normalized_to_all_logical_processors() {
        assert_eq!(normalized_cpu_percent(2.0, 10.0, 8), Some(2.5));
        assert_eq!(normalized_cpu_percent(2.0, 0.0, 8), None);
        assert_eq!(normalized_cpu_percent(2.0, 10.0, 0), None);
    }

    #[test]
    fn probe_duration_must_be_bounded() {
        for seconds in ["0", "301", "-1", "abc"] {
            let args = ["monitor", "--probe", "--seconds", seconds].map(String::from);
            assert!(Options::parse(&args).is_err());
        }
        let args = ["monitor", "--probe", "--seconds", "1"].map(String::from);
        assert_eq!(Options::parse(&args).unwrap().seconds, 1);
    }
}
