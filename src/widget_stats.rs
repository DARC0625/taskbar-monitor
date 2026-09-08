//! Bounded, opt-in diagnostics for the running GUI process. No collector thread.
use serde::Serialize;
use std::{
    collections::VecDeque,
    mem::size_of,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::{ERROR_SUCCESS, FILETIME, GetLastError, SetLastError},
    System::{
        ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX},
        Threading::*,
    },
};

const LATENCY_CAPACITY: usize = 1200;
const RESOURCE_CAPACITY: usize = 720;
const RESOURCE_INTERVAL: Duration = Duration::from_secs(5);

/// Evict before pushing: backing storage does not grow when the buffer is full.
struct Recent<T> {
    values: VecDeque<T>,
    limit: usize,
    total: u64,
}
impl<T> Recent<T> {
    fn new(limit: usize) -> Self {
        assert!(limit > 0);
        Self {
            values: VecDeque::with_capacity(limit),
            limit,
            total: 0,
        }
    }
    fn push(&mut self, value: T) {
        if self.values.len() == self.limit {
            self.values.pop_front();
        }
        self.values.push_back(value);
        self.total = self.total.saturating_add(1);
    }
}

#[derive(Default, Serialize)]
struct SampledRange<T> {
    valid_samples: u64,
    missing_samples: u64,
    first: Option<T>,
    last: Option<T>,
    min: Option<T>,
    max: Option<T>,
}
impl<T: Copy + PartialOrd> SampledRange<T> {
    fn observe(&mut self, value: Option<T>) {
        if let Some(value) = value {
            self.valid_samples = self.valid_samples.saturating_add(1);
            self.first.get_or_insert(value);
            self.last = Some(value);
            if self.min.is_none_or(|old| value < old) {
                self.min = Some(value);
            }
            if self.max.is_none_or(|old| value > old) {
                self.max = Some(value);
            }
        } else {
            self.missing_samples = self.missing_samples.saturating_add(1);
        }
    }
}

#[derive(Clone, Copy)]
struct LatencySample {
    at: f64,
    milliseconds: f64,
}

#[derive(Serialize)]
struct ResourceSample {
    uptime_seconds: f64,
    process_cpu_seconds_since_process_start: Option<f64>,
    cpu_interval_seconds: Option<f64>,
    cpu_percent_all_logical_processors: Option<f64>,
    private_bytes: Option<u64>,
    working_set_bytes: Option<u64>,
    handles: Option<u32>,
    gdi_objects: Option<u32>,
    user_objects: Option<u32>,
    errors: Vec<String>,
}

#[derive(Default, Serialize)]
struct ResourceRanges {
    private_bytes: SampledRange<u64>,
    working_set_bytes: SampledRange<u64>,
    handles: SampledRange<u32>,
    gdi_objects: SampledRange<u32>,
    user_objects: SampledRange<u32>,
    cpu_percent_all_logical_processors: SampledRange<f64>,
}

pub(super) struct WidgetStats {
    start: Instant,
    last_resource: Option<Instant>,
    logical_processors: u32,
    first_cpu: Option<(f64, u64)>,
    previous_cpu: Option<(f64, u64)>,
    latency: Recent<LatencySample>,
    latency_range: SampledRange<f64>,
    first_latency_at: Option<f64>,
    resources: Recent<ResourceSample>,
    ranges: ResourceRanges,
    first_resource_at: Option<f64>,
    resource_samples_with_errors: u64,
    max_resource_interval_seconds: f64,
    lifecycle: Lifecycle,
}

#[derive(Default, Serialize)]
struct Lifecycle {
    attachment_create_attempts: u64,
    attachment_create_errors: u64,
    externally_destroyed_widgets: u64,
    renderer_instances_created: u64,
    renderer_init_errors: u64,
    resize_errors: u64,
    paint_errors: u64,
    last_paint_uptime_seconds: Option<f64>,
    last_successful_present_return_uptime_seconds: Option<f64>,
    last_error_stage: Option<String>,
    last_error: Option<String>,
    last_error_uptime_seconds: Option<f64>,
}

impl WidgetStats {
    pub(super) fn new(start: Instant) -> Self {
        Self::with_processors(start, unsafe { GetActiveProcessorCount(0xffff) })
    }

    fn with_processors(start: Instant, logical_processors: u32) -> Self {
        Self {
            start,
            last_resource: None,
            logical_processors,
            first_cpu: None,
            previous_cpu: None,
            latency: Recent::new(LATENCY_CAPACITY),
            latency_range: SampledRange::default(),
            first_latency_at: None,
            resources: Recent::new(RESOURCE_CAPACITY),
            ranges: ResourceRanges::default(),
            first_resource_at: None,
            resource_samples_with_errors: 0,
            max_resource_interval_seconds: 0.0,
            lifecycle: Lifecycle::default(),
        }
    }

    pub(super) fn attach_attempted(&mut self) {
        self.lifecycle.attachment_create_attempts =
            self.lifecycle.attachment_create_attempts.saturating_add(1);
    }
    pub(super) fn external_widget_destroyed(&mut self) {
        self.lifecycle.externally_destroyed_widgets = self
            .lifecycle
            .externally_destroyed_widgets
            .saturating_add(1);
    }
    pub(super) fn renderer_created(&mut self) {
        self.lifecycle.renderer_instances_created =
            self.lifecycle.renderer_instances_created.saturating_add(1);
    }
    pub(super) fn paint_finished(&mut self, success: bool) {
        let at = self.start.elapsed().as_secs_f64();
        self.lifecycle.last_paint_uptime_seconds = Some(at);
        if success {
            self.lifecycle.last_successful_present_return_uptime_seconds = Some(at);
        }
    }
    pub(super) fn record_error(&mut self, stage: &str, error: &windows::core::Error) {
        let counter = match stage {
            "attach" => &mut self.lifecycle.attachment_create_errors,
            "renderer_init" => &mut self.lifecycle.renderer_init_errors,
            "resize" => &mut self.lifecycle.resize_errors,
            _ => &mut self.lifecycle.paint_errors,
        };
        *counter = counter.saturating_add(1);
        self.lifecycle.last_error_stage = Some(stage.into());
        self.lifecycle.last_error = Some(error.to_string());
        self.lifecycle.last_error_uptime_seconds = Some(self.start.elapsed().as_secs_f64());
    }
    pub(super) fn lifecycle_report(&self) -> serde_json::Value {
        serde_json::to_value(&self.lifecycle).unwrap_or(serde_json::Value::Null)
    }

    pub(super) fn record_latency(&mut self, sampled_at: Instant, presented_at: Instant) {
        let Some(delay) = presented_at.checked_duration_since(sampled_at) else {
            return;
        };
        let at = presented_at
            .saturating_duration_since(self.start)
            .as_secs_f64();
        let milliseconds = delay.as_secs_f64() * 1000.0;
        self.first_latency_at.get_or_insert(at);
        self.latency_range.observe(Some(milliseconds));
        self.latency.push(LatencySample { at, milliseconds });
    }

    /// Called by the existing UI timer. Delays are measured, never filled with synthetic samples.
    pub(super) fn sample_resources(&mut self, force: bool) {
        let now = Instant::now();
        if !force
            && self
                .last_resource
                .is_some_and(|last| now.duration_since(last) < RESOURCE_INTERVAL)
        {
            return;
        }
        if let Some(last) = self.last_resource {
            self.max_resource_interval_seconds = self
                .max_resource_interval_seconds
                .max(now.duration_since(last).as_secs_f64());
        }
        self.last_resource = Some(now);
        let at = now.saturating_duration_since(self.start).as_secs_f64();
        let (mut sample, ticks) = unsafe { read_resources(at) };
        if let Some(ticks) = ticks {
            if let Some((previous_at, previous_ticks)) = self.previous_cpu {
                let interval = at - previous_at;
                sample.cpu_interval_seconds = Some(interval);
                sample.cpu_percent_all_logical_processors =
                    cpu_percent(previous_ticks, ticks, interval, self.logical_processors);
            }
            self.first_cpu.get_or_insert((at, ticks));
            self.previous_cpu = Some((at, ticks));
        }
        self.first_resource_at.get_or_insert(at);
        if !sample.errors.is_empty() {
            self.resource_samples_with_errors = self.resource_samples_with_errors.saturating_add(1);
        }
        self.ranges.private_bytes.observe(sample.private_bytes);
        self.ranges
            .working_set_bytes
            .observe(sample.working_set_bytes);
        self.ranges.handles.observe(sample.handles);
        self.ranges.gdi_objects.observe(sample.gdi_objects);
        self.ranges.user_objects.observe(sample.user_objects);
        self.ranges
            .cpu_percent_all_logical_processors
            .observe(sample.cpu_percent_all_logical_processors);
        self.resources.push(sample);
    }

    pub(super) fn latency_report(&self) -> serde_json::Value {
        let mut values: Vec<_> = self.latency.values.iter().map(|s| s.milliseconds).collect();
        values.sort_by(f64::total_cmp);
        let percentile = |p: f64| {
            values
                .get(((values.len().saturating_sub(1)) as f64 * p).round() as usize)
                .copied()
        };
        serde_json::json!({
            "scope":"most_recent_successfully_presented_distinct_cpu_samples",
            "count":values.len(), "capacity":LATENCY_CAPACITY, "total_samples":self.latency.total,
            "evicted_samples":self.latency.total.saturating_sub(values.len() as u64),
            "recent_start_uptime_seconds":self.latency.values.front().map(|s| s.at),
            "recent_end_uptime_seconds":self.latency.values.back().map(|s| s.at),
            "p50":percentile(0.5), "p95":percentile(0.95), "max":values.last(),
            "all_observed_samples":self.latency_range,
            "all_observed_start_uptime_seconds":self.first_latency_at,
            "does_not_measure_unpainted_samples_or_physical_scanout":true
        })
    }

    pub(super) fn resource_report(&self) -> serde_json::Value {
        let cpu_coverage = self.first_cpu.zip(self.previous_cpu).map(|((start, first), (end, last))| {
            serde_json::json!({
                "start_uptime_seconds":start, "end_uptime_seconds":end, "wall_seconds":end-start,
                "cpu_seconds":last.checked_sub(first).map(|ticks| ticks as f64 / 1e7),
                "average_percent_all_logical_processors":cpu_percent(first,last,end-start,self.logical_processors)
            })
        });
        serde_json::json!({
            "scope":"this_running_widget_process_including_collectors_renderer_and_diagnostics",
            "nominal_sample_interval_seconds":RESOURCE_INTERVAL.as_secs(),
            "samples_total":self.resources.total, "recent_capacity":RESOURCE_CAPACITY,
            "samples_evicted":self.resources.total.saturating_sub(self.resources.values.len() as u64),
            "all_observed_start_uptime_seconds":self.first_resource_at,
            "all_observed_end_uptime_seconds":self.resources.values.back().map(|s| s.uptime_seconds),
            "recent_start_uptime_seconds":self.resources.values.front().map(|s| s.uptime_seconds),
            "recent_end_uptime_seconds":self.resources.values.back().map(|s| s.uptime_seconds),
            "max_observed_sample_interval_seconds":self.max_resource_interval_seconds,
            "samples_with_errors":self.resource_samples_with_errors,
            "logical_processors_for_cpu_normalization":self.logical_processors,
            "cpu_measured_interval":cpu_coverage,
            "all_observed_sample_ranges":self.ranges,
            "recent_samples":self.resources.values,
            "caveat":"Sampled process counters; maxima are sampled high-water marks, not continuous peaks or proof of leak freedom. Timer gaps and suspend are not reconstructed."
        })
    }
}

fn cpu_percent(first: u64, last: u64, seconds: f64, processors: u32) -> Option<f64> {
    if !seconds.is_finite() || seconds <= 0.0 || processors == 0 {
        return None;
    }
    let ticks = last.checked_sub(first)?;
    let value = ticks as f64 / 1e7 / seconds / f64::from(processors) * 100.0;
    value.is_finite().then_some(value)
}

unsafe fn read_resources(uptime_seconds: f64) -> (ResourceSample, Option<u64>) {
    let process = GetCurrentProcess(); // Pseudo-handle: do not close.
    let mut sample = ResourceSample {
        uptime_seconds,
        process_cpu_seconds_since_process_start: None,
        cpu_interval_seconds: None,
        cpu_percent_all_logical_processors: None,
        private_bytes: None,
        working_set_bytes: None,
        handles: None,
        gdi_objects: None,
        user_objects: None,
        errors: Vec::new(),
    };
    let (mut creation, mut exit, mut kernel, mut user) = (
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
        FILETIME::default(),
    );
    let ticks = match GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) {
        Ok(()) => {
            let ticks =
                |t: FILETIME| (u64::from(t.dwHighDateTime) << 32) | u64::from(t.dwLowDateTime);
            ticks(kernel).checked_add(ticks(user))
        }
        Err(error) => {
            sample.errors.push(format!("GetProcessTimes: {error}"));
            None
        }
    };
    sample.process_cpu_seconds_since_process_start = ticks.map(|ticks| ticks as f64 / 1e7);
    let mut memory = PROCESS_MEMORY_COUNTERS_EX {
        cb: size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32,
        ..Default::default()
    };
    match GetProcessMemoryInfo(
        process,
        (&mut memory as *mut PROCESS_MEMORY_COUNTERS_EX).cast(),
        memory.cb,
    ) {
        Ok(()) => {
            sample.private_bytes = Some(memory.PrivateUsage as u64);
            sample.working_set_bytes = Some(memory.WorkingSetSize as u64);
        }
        Err(error) => sample.errors.push(format!("GetProcessMemoryInfo: {error}")),
    }
    let mut handles = 0;
    match GetProcessHandleCount(process, &mut handles) {
        Ok(()) => sample.handles = Some(handles),
        Err(error) => sample
            .errors
            .push(format!("GetProcessHandleCount: {error}")),
    }
    for (flag, name, result) in [
        (GR_GDIOBJECTS, "GDI", &mut sample.gdi_objects),
        (GR_USEROBJECTS, "USER", &mut sample.user_objects),
    ] {
        SetLastError(ERROR_SUCCESS);
        let count = GetGuiResources(process, flag);
        let error = GetLastError();
        if count == 0 && error != ERROR_SUCCESS {
            sample
                .errors
                .push(format!("GetGuiResources({name}): {}", error.0));
        } else {
            *result = Some(count);
        }
    }
    (sample, ticks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recent_buffer_keeps_tail_without_growing_after_full() {
        let mut recent = Recent::new(3);
        let allocation = recent.values.capacity();
        for n in 0..100_000 {
            recent.push(n);
        }
        assert_eq!(
            recent.values.iter().copied().collect::<Vec<_>>(),
            vec![99_997, 99_998, 99_999]
        );
        assert_eq!(recent.values.capacity(), allocation);
        assert_eq!(recent.total, 100_000);
    }
    #[test]
    fn latency_percentiles_are_recent_but_lifetime_max_survives_eviction() {
        let start = Instant::now();
        let mut stats = WidgetStats::with_processors(start, 8);
        stats.record_latency(start, start + Duration::from_secs(2));
        for n in 0..LATENCY_CAPACITY {
            let sample = start + Duration::from_secs(3 + n as u64);
            stats.record_latency(sample, sample + Duration::from_millis(3));
        }
        let report = stats.latency_report();
        assert_eq!(report["count"], LATENCY_CAPACITY);
        assert_eq!(report["evicted_samples"], 1);
        assert_eq!(report["max"], 3.0);
        assert_eq!(report["all_observed_samples"]["max"], 2000.0);
        assert_eq!(report["all_observed_start_uptime_seconds"], 2.0);
        assert_eq!(report["recent_start_uptime_seconds"], 3.003);
    }
    #[test]
    fn absent_resources_remain_missing_and_do_not_lower_minimum_to_zero() {
        let mut range = SampledRange::<u64>::default();
        for value in [None, Some(40), Some(20), None, Some(30)] {
            range.observe(value);
        }
        assert_eq!(
            (range.first, range.last, range.min, range.max),
            (Some(40), Some(30), Some(20), Some(40))
        );
        assert_eq!((range.valid_samples, range.missing_samples), (3, 2));
    }
    #[test]
    fn cpu_intervals_reject_counter_regression_and_invalid_denominators() {
        assert_eq!(cpu_percent(10_000_000, 30_000_000, 10.0, 8), Some(2.5));
        for seconds in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            assert_eq!(cpu_percent(0, 1, seconds, 8), None);
        }
        assert_eq!(cpu_percent(2, 1, 10.0, 8), None);
        assert_eq!(cpu_percent(0, 1, 10.0, 0), None);
    }
}
