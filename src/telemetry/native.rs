//! Windows ABI boundary. Handles stay on their owning collector thread.
//! References: Microsoft GetSystemTimes / GlobalMemoryStatusEx / PDH documentation;
//! DirectX-Headers/include/directx/dxcore_interface.h for newer DXCore metadata.

use super::{CpuTimes, Metric, MetricState, RESUME_GAP, busiest_disk, busiest_engine, engine_busy};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};
use windows::Win32::Graphics::DXCore::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

#[repr(C)]
#[derive(Default)]
struct FileTime {
    low: u32,
    high: u32,
}
impl FileTime {
    fn ticks(&self) -> u64 {
        ((self.high as u64) << 32) | self.low as u64
    }
}

#[repr(C)]
struct MemoryStatusEx {
    length: u32,
    load: u32,
    total_phys: u64,
    avail_phys: u64,
    total_page_file: u64,
    avail_page_file: u64,
    total_virtual: u64,
    avail_virtual: u64,
    avail_extended_virtual: u64,
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
    fn GlobalMemoryStatusEx(memory: *mut MemoryStatusEx) -> i32;
    fn GetLastError() -> u32;
    fn GetActiveProcessorCount(group: u16) -> u32;
}

pub(super) fn cpu_times() -> Result<CpuTimes, String> {
    let (mut idle, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // GetSystemTimes is only a machine-wide total on <=64 logical processors.
    // Do not silently present one processor group as the entire machine.
    unsafe {
        if GetActiveProcessorCount(0xffff) > 64 {
            return Err(
                "64개 초과 논리 CPU는 현재 수집기에서 전체 그룹 합산을 지원하지 않습니다".into(),
            );
        }
        if GetSystemTimes(&mut idle, &mut kernel, &mut user) == 0 {
            return Err(format!("GetSystemTimes 오류 {}", GetLastError()));
        }
    }
    Ok(CpuTimes {
        idle: idle.ticks(),
        kernel: kernel.ticks(),
        user: user.ticks(),
    })
}

pub(super) fn physical_memory() -> Result<(u64, u64), String> {
    // SAFETY: all fields are plain integers; dwLength describes the exact C layout.
    let mut memory: MemoryStatusEx = unsafe { zeroed() };
    memory.length = size_of::<MemoryStatusEx>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut memory) } == 0 {
        return Err(format!("GlobalMemoryStatusEx 오류 {}", unsafe {
            GetLastError()
        }));
    }
    Ok((memory.total_phys, memory.avail_phys))
}

type PdhHandle = isize;
const PDH_MORE_DATA: u32 = 0x8000_07d2;
const PDH_CSTATUS_NO_INSTANCE: u32 = 0x8000_07d1;
const PDH_CSTATUS_ITEM_NOT_VALIDATED: u32 = 0x8000_07d3;
const PDH_RETRY: u32 = 0x8000_07d4;
const PDH_NO_DATA: u32 = 0x8000_07d5;
const PDH_CALC_NEGATIVE_DENOMINATOR: u32 = 0x8000_07d6;
const PDH_CALC_NEGATIVE_TIMEBASE: u32 = 0x8000_07d7;
const PDH_CALC_NEGATIVE_VALUE: u32 = 0x8000_07d8;
const PDH_CSTATUS_NO_OBJECT: u32 = 0xc000_0bb8;
const PDH_CSTATUS_NO_COUNTER: u32 = 0xc000_0bb9;
const PDH_CSTATUS_INVALID_DATA: u32 = 0xc000_0bba;
const PDH_INVALID_DATA: u32 = 0xc000_0bc6;
const PDH_FMT_DOUBLE: u32 = 0x200;
const PDH_FMT_NOCAP100: u32 = 0x8000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CounterValue {
    status: u32,
    value: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct CounterItem {
    name: *const u16,
    formatted: CounterValue,
}

#[link(name = "pdh", kind = "raw-dylib")]
unsafe extern "system" {
    fn PdhOpenQueryW(source: *const u16, user_data: usize, query: *mut PdhHandle) -> u32;
    fn PdhCloseQuery(query: PdhHandle) -> u32;
    fn PdhAddEnglishCounterW(
        query: PdhHandle,
        path: *const u16,
        user_data: usize,
        counter: *mut PdhHandle,
    ) -> u32;
    fn PdhCollectQueryData(query: PdhHandle) -> u32;
    fn PdhGetFormattedCounterArrayW(
        counter: PdhHandle,
        format: u32,
        buffer_size: *mut u32,
        item_count: *mut u32,
        buffer: *mut CounterItem,
    ) -> u32;
    fn PdhGetFormattedCounterValue(
        counter: PdhHandle,
        format: u32,
        kind: *mut u32,
        value: *mut CounterValue,
    ) -> u32;
}

struct Query(PdhHandle);
impl Query {
    fn new() -> Result<Self, u32> {
        let mut handle = 0;
        let status = unsafe { PdhOpenQueryW(null(), 0, &mut handle) };
        if status == 0 {
            Ok(Self(handle))
        } else {
            Err(status)
        }
    }
    fn add(&self, path: &str) -> Result<PdhHandle, u32> {
        let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
        let mut handle = 0;
        let status = unsafe { PdhAddEnglishCounterW(self.0, wide.as_ptr(), 0, &mut handle) };
        if status == 0 { Ok(handle) } else { Err(status) }
    }
    fn collect(&self) -> Result<(), u32> {
        match unsafe { PdhCollectQueryData(self.0) } {
            0 => Ok(()),
            error => Err(error),
        }
    }
}
impl Drop for Query {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.0);
        }
    }
}

fn pdh_metric(error: u32, source: &'static str) -> Metric {
    let state = match error {
        PDH_CSTATUS_NO_OBJECT | PDH_CSTATUS_NO_COUNTER => MetricState::Unsupported,
        PDH_NO_DATA
        | PDH_INVALID_DATA
        | PDH_CSTATUS_INVALID_DATA
        | PDH_CSTATUS_NO_INSTANCE
        | PDH_CSTATUS_ITEM_NOT_VALIDATED
        | PDH_RETRY
        | PDH_CALC_NEGATIVE_DENOMINATOR
        | PDH_CALC_NEGATIVE_TIMEBASE
        | PDH_CALC_NEGATIVE_VALUE
        | PDH_MORE_DATA => MetricState::WarmingUp,
        _ => MetricState::Error,
    };
    Metric::unavailable(state, source, format!("{source} 상태 0x{error:08X}"))
}

/// A word-aligned buffer is necessary: Vec<u8> alone does not promise ABI alignment.
/// The provider-returned strings are copied only after verifying they stay in this buffer.
fn counter_array(counter: PdhHandle) -> Result<Vec<(String, f64)>, u32> {
    const MAX_BUFFER: usize = 16 * 1024 * 1024;
    // Requery size after a race; PDH explicitly says a too-small nonzero size is unreliable.
    for _ in 0..3 {
        let mut bytes = 0u32;
        let mut count = 0u32;
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE | PDH_FMT_NOCAP100,
                &mut bytes,
                &mut count,
                null_mut(),
            )
        };
        if status != PDH_MORE_DATA {
            return if status == 0 && count == 0 {
                Ok(Vec::new())
            } else {
                Err(status)
            };
        }
        if bytes == 0 || bytes as usize > MAX_BUFFER {
            return Err(PDH_INVALID_DATA);
        }
        let mut words = vec![0u64; (bytes as usize).div_ceil(size_of::<u64>())];
        let capacity_bytes = words.len() * size_of::<u64>();
        bytes = capacity_bytes as u32;
        let buffer = words.as_mut_ptr().cast::<CounterItem>();
        let status = unsafe {
            PdhGetFormattedCounterArrayW(
                counter,
                PDH_FMT_DOUBLE | PDH_FMT_NOCAP100,
                &mut bytes,
                &mut count,
                buffer,
            )
        };
        if status == PDH_MORE_DATA {
            continue;
        }
        if status != 0 {
            return Err(status);
        }
        if bytes as usize > capacity_bytes
            || count as usize > bytes as usize / size_of::<CounterItem>()
        {
            return Err(PDH_INVALID_DATA);
        }
        let base = words.as_ptr() as usize;
        let end = base + bytes as usize;
        let items = unsafe { std::slice::from_raw_parts(buffer, count as usize) };
        let mut result = Vec::with_capacity(count as usize);
        for item in items {
            if item.formatted.status > 1
                || !item.formatted.value.is_finite()
                || item.formatted.value < 0.0
            {
                continue;
            }
            let name_address = item.name as usize;
            if name_address < base || name_address >= end || name_address % 2 != 0 {
                return Err(PDH_INVALID_DATA);
            }
            let remaining = (end - name_address) / size_of::<u16>();
            let name_units = unsafe { std::slice::from_raw_parts(item.name, remaining) };
            let Some(length) = name_units.iter().position(|&ch| ch == 0) else {
                return Err(PDH_INVALID_DATA);
            };
            result.push((
                String::from_utf16_lossy(&name_units[..length]),
                item.formatted.value,
            ));
        }
        return Ok(result);
    }
    Err(PDH_MORE_DATA)
}

fn counter_scalar(counter: PdhHandle) -> Result<f64, u32> {
    let mut value = CounterValue::default();
    let status = unsafe {
        PdhGetFormattedCounterValue(
            counter,
            PDH_FMT_DOUBLE | PDH_FMT_NOCAP100,
            null_mut(),
            &mut value,
        )
    };
    if status != 0 {
        return Err(status);
    }
    if value.status > 1 || !value.value.is_finite() || value.value < 0.0 {
        return Err(PDH_INVALID_DATA);
    }
    Ok(value.value)
}

struct PdhQuery {
    query: Query,
    previous_collect: Option<Instant>,
    failed: bool,
}

impl PdhQuery {
    fn new() -> Result<Self, u32> {
        Ok(Self {
            query: Query::new()?,
            previous_collect: None,
            failed: false,
        })
    }

    fn collect(&mut self) -> Result<Option<Duration>, u32> {
        if let Err(error) = self.query.collect() {
            self.failed = true;
            self.previous_collect = None;
            return Err(error);
        }
        let current = Instant::now();
        let window = self
            .previous_collect
            .map(|previous| current.duration_since(previous));
        self.previous_collect = Some(current);
        self.failed = false;
        Ok(window.filter(|window| !window.is_zero() && *window <= RESUME_GAP))
    }
}

struct GpuCounters {
    pdh: PdhQuery,
    gpu: Result<PdhHandle, u32>,
}

impl GpuCounters {
    fn new() -> Result<Self, u32> {
        let pdh = PdhQuery::new()?;
        let gpu = pdh.query.add(r"\GPU Engine(*)\Utilization Percentage");
        Ok(Self { pdh, gpu })
    }
}

struct DiskCounters {
    pdh: PdhQuery,
    disk: Result<PdhHandle, u32>,
    read: Result<PdhHandle, u32>,
    write: Result<PdhHandle, u32>,
}

impl DiskCounters {
    fn new() -> Result<Self, u32> {
        let pdh = PdhQuery::new()?;
        let disk = pdh.query.add(r"\PhysicalDisk(*)\% Idle Time");
        let read = pdh.query.add(r"\PhysicalDisk(_Total)\Disk Read Bytes/sec");
        let write = pdh.query.add(r"\PhysicalDisk(_Total)\Disk Write Bytes/sec");
        Ok(Self {
            pdh,
            disk,
            read,
            write,
        })
    }
}

fn warm(source: &'static str) -> Metric {
    Metric::unavailable(
        MetricState::WarmingUp,
        source,
        "속도 계산용 두 번째 표본 준비 중",
    )
}

pub(super) struct GpuSample {
    pub gpu: Metric,
    pub adapter: String,
}

pub(super) struct GpuProvider {
    counters: Result<GpuCounters, u32>,
    metadata: GpuMetadata,
}

impl GpuProvider {
    pub fn new() -> Self {
        Self {
            counters: GpuCounters::new(),
            metadata: GpuMetadata::new(),
        }
    }
}

impl super::Collector for GpuProvider {
    type Sample = GpuSample;

    fn needs_retry(&self) -> bool {
        self.counters.as_ref().map_or(true, |counters| {
            counters.pdh.failed || counters.gpu.is_err()
        }) || self.metadata.needs_retry()
    }

    fn sample(&mut self) -> GpuSample {
        let mut sample = GpuSample {
            gpu: warm("PDH GPU Engine"),
            adapter: self.metadata.names.clone(),
        };
        let counters = match self.counters.as_mut() {
            Ok(counters) => counters,
            Err(error) => {
                sample.gpu = pdh_metric(*error, "PDH GPU Engine");
                return sample;
            }
        };
        let window = match counters.pdh.collect() {
            Ok(Some(window)) => window,
            Ok(None) => return sample,
            Err(error) => {
                sample.gpu = pdh_metric(error, "PDH GPU Engine");
                return sample;
            }
        };
        sample.gpu = match counters.gpu.and_then(counter_array) {
            Ok(values) => match busiest_engine(&values) {
                Some((engine, percent)) => Metric::reading(
                    percent,
                    "PDH GPU Engine",
                    window,
                    format!("가장 바쁜 물리 GPU 엔진: {engine} (엔진별 프로세스 사용률 합계)"),
                ),
                None => Metric::unavailable(
                    MetricState::WarmingUp,
                    "PDH GPU Engine",
                    "GPU 엔진의 유효한 표본이 아직 없습니다",
                ),
            },
            Err(error) => pdh_metric(error, "PDH GPU Engine"),
        };
        counters.pdh.failed = sample.gpu.state == MetricState::Error;
        sample
    }
}

pub(super) struct DiskSample {
    pub disk: Metric,
    pub read: Metric,
    pub write: Metric,
}

pub(super) struct DiskProvider {
    counters: Result<DiskCounters, u32>,
}

impl DiskProvider {
    pub fn new() -> Self {
        Self {
            counters: DiskCounters::new(),
        }
    }
}

impl super::Collector for DiskProvider {
    type Sample = DiskSample;

    fn needs_retry(&self) -> bool {
        self.counters.as_ref().map_or(true, |counters| {
            counters.pdh.failed
                || counters.disk.is_err()
                || counters.read.is_err()
                || counters.write.is_err()
        })
    }

    fn sample(&mut self) -> DiskSample {
        let mut sample = DiskSample {
            disk: warm("PDH PhysicalDisk"),
            read: warm("PDH PhysicalDisk"),
            write: warm("PDH PhysicalDisk"),
        };
        let counters = match self.counters.as_mut() {
            Ok(counters) => counters,
            Err(error) => {
                sample.disk = pdh_metric(*error, "PDH PhysicalDisk");
                sample.read = sample.disk.clone();
                sample.write = sample.disk.clone();
                return sample;
            }
        };
        let window = match counters.pdh.collect() {
            Ok(Some(window)) => window,
            Ok(None) => return sample,
            Err(error) => {
                sample.disk = pdh_metric(error, "PDH PhysicalDisk");
                sample.read = sample.disk.clone();
                sample.write = sample.disk.clone();
                return sample;
            }
        };
        sample.disk = match counters.disk.and_then(counter_array) {
            Ok(values) => match busiest_disk(&values) {
                Some((disk, percent)) => Metric::reading(
                    percent,
                    "PDH PhysicalDisk",
                    window,
                    format!("가장 바쁜 물리 디스크 {disk}: 100 - % Idle Time"),
                ),
                None => Metric::unavailable(
                    MetricState::WarmingUp,
                    "PDH PhysicalDisk",
                    "물리 디스크 표본이 아직 없습니다",
                ),
            },
            Err(error) => pdh_metric(error, "PDH PhysicalDisk"),
        };
        sample.read = match counters.read.and_then(counter_scalar) {
            Ok(value) => Metric::reading(
                value,
                "PDH PhysicalDisk",
                window,
                "모든 물리 디스크 읽기 속도 합계 (bytes/s)",
            ),
            Err(error) => pdh_metric(error, "PDH PhysicalDisk"),
        };
        sample.write = match counters.write.and_then(counter_scalar) {
            Ok(value) => Metric::reading(
                value,
                "PDH PhysicalDisk",
                window,
                "모든 물리 디스크 쓰기 속도 합계 (bytes/s)",
            ),
            Err(error) => pdh_metric(error, "PDH PhysicalDisk"),
        };
        counters.pdh.failed = [&sample.disk, &sample.read, &sample.write]
            .iter()
            .any(|metric| metric.state == MetricState::Error);
        sample
    }
}

// GPU description enumeration stays with GPU collection, never with NPU calls.
struct GpuMetadata {
    names: String,
    list: Option<IDXCoreAdapterList>,
}

impl GpuMetadata {
    fn new() -> Self {
        Self::enumerate().unwrap_or(Self {
            names: String::new(),
            list: None,
        })
    }

    fn enumerate() -> windows::core::Result<Self> {
        let factory: IDXCoreAdapterFactory = unsafe { DXCoreCreateAdapterFactory()? };
        let list: IDXCoreAdapterList =
            unsafe { factory.CreateAdapterList(&[DXCORE_ADAPTER_ATTRIBUTE_D3D11_GRAPHICS])? };
        let mut names = Vec::new();
        for index in 0..unsafe { list.GetAdapterCount() }.min(64) {
            if let Ok(adapter) = unsafe { list.GetAdapter::<IDXCoreAdapter>(index) } {
                if property_u8(&adapter, IsHardware).unwrap_or(0) != 0 {
                    names.push(adapter_name(&adapter));
                }
            }
        }
        Ok(Self {
            names: names.join(", "),
            list: Some(list),
        })
    }

    fn needs_retry(&self) -> bool {
        self.list
            .as_ref()
            .is_none_or(|list| unsafe { list.IsStale() })
    }
}

pub(super) struct NpuProvider(DxCoreDevices);

impl NpuProvider {
    pub fn new() -> Self {
        Self(DxCoreDevices::new())
    }
}

impl super::Collector for NpuProvider {
    type Sample = Metric;

    fn sample(&mut self) -> Metric {
        self.0.sample_npu()
    }

    fn needs_retry(&self) -> bool {
        self.0.needs_retry()
    }
}

// windows 0.62 metadata predates these additions. Values and layout are from the
// Microsoft DirectX-Headers repository, not driver-private or guessed APIs.
const NPU_HARDWARE_ATTRIBUTE: GUID = GUID::from_u128(0xd46140c4_add7_451b_9e56_06fe8c3b58ed);
const ADAPTER1_IID: GUID = GUID::from_u128(0xa0783366_cfa3_43be_9d79_55b2da97c63c);
const PHYSICAL_ADAPTER_COUNT: DXCoreAdapterProperty = DXCoreAdapterProperty(15);
const ADAPTER_ENGINE_COUNT: DXCoreAdapterProperty = DXCoreAdapterProperty(16);
const ENGINE_RUNNING_TIME: DXCoreAdapterState = DXCoreAdapterState(4);

#[repr(C)]
struct EngineIndex {
    physical_adapter: u32,
    engine: u32,
}

#[repr(C)]
struct Adapter1Vtable {
    base: IDXCoreAdapter_Vtbl,
    get_property_with_input: unsafe extern "system" fn(
        *mut c_void,
        DXCoreAdapterProperty,
        usize,
        *const c_void,
        usize,
        *mut c_void,
    ) -> HRESULT,
}

struct Adapter1(IUnknown);
impl Adapter1 {
    fn from_adapter(adapter: &IDXCoreAdapter) -> windows::core::Result<Self> {
        let mut pointer = null_mut();
        // SAFETY: QueryInterface returns an owned reference of exactly ADAPTER1_IID.
        unsafe {
            adapter.query(&ADAPTER1_IID, &mut pointer).ok()?;
            Ok(Self(IUnknown::from_raw(pointer)))
        }
    }
    fn engine_count(&self, physical: u32) -> windows::core::Result<u32> {
        let pointer = self.0.as_raw();
        let mut count = 0u32;
        // SAFETY: this object was queried for Adapter1; the vtable extends Adapter's ABI.
        unsafe {
            let vtable = &**(pointer as *const *const Adapter1Vtable);
            (vtable.get_property_with_input)(
                pointer,
                ADAPTER_ENGINE_COUNT,
                size_of::<u32>(),
                (&physical as *const u32).cast(),
                size_of::<u32>(),
                (&mut count as *mut u32).cast(),
            )
            .ok()?;
        }
        Ok(count)
    }
}

struct NpuEngine {
    adapter: IDXCoreAdapter,
    name: String,
    index: EngineIndex,
    previous: Option<(u64, Instant)>,
}

struct DxCoreDevices {
    npu: Result<Vec<NpuEngine>, Metric>,
    list: Option<IDXCoreAdapterList>,
    failed: bool,
}

impl DxCoreDevices {
    fn new() -> Self {
        match Self::enumerate() {
            Ok(devices) => devices,
            Err(error) => Self {
                npu: Err(Metric::unavailable(
                    MetricState::Error,
                    "DXCore",
                    format!("DXCore 장치 열거 실패: {error}"),
                )),
                list: None,
                failed: true,
            },
        }
    }

    fn enumerate() -> windows::core::Result<Self> {
        // DXCore is COM-like but does not require COM apartment initialization.
        let factory: IDXCoreAdapterFactory = unsafe { DXCoreCreateAdapterFactory()? };
        let npus: IDXCoreAdapterList =
            unsafe { factory.CreateAdapterList(&[NPU_HARDWARE_ATTRIBUTE])? };
        let count = unsafe { npus.GetAdapterCount() };
        let mut devices = Self {
            npu: Ok(Vec::new()),
            list: Some(npus.clone()),
            failed: false,
        };
        if count == 0 {
            devices.npu = Err(Metric::unavailable(
                MetricState::NotPresent,
                "DXCore NPU hardware attribute",
                "DXCore가 노출하는 NPU 장치가 없습니다. Intel GNA를 NPU 사용률로 대체하지 않습니다.",
            ));
            return Ok(devices);
        }
        if count > 64 {
            devices.npu = Err(Metric::unavailable(
                MetricState::Unsupported,
                "DXCore",
                "NPU 장치 수가 수집기 범위를 초과합니다",
            ));
            return Ok(devices);
        }
        let mut engines = Vec::new();
        for index in 0..count {
            let adapter: IDXCoreAdapter = unsafe { npus.GetAdapter(index)? };
            let name = adapter_name(&adapter);
            if !unsafe { adapter.IsQueryStateSupported(ENGINE_RUNNING_TIME) }
                || !unsafe { adapter.IsPropertySupported(PHYSICAL_ADAPTER_COUNT) }
                || !unsafe { adapter.IsPropertySupported(ADAPTER_ENGINE_COUNT) }
            {
                devices.npu = Err(Metric::unavailable(
                    MetricState::Unsupported,
                    "DXCore",
                    format!(
                        "{name}: 드라이버가 NPU 엔진 누적 시간/엔진 수 조회를 지원하지 않습니다"
                    ),
                ));
                return Ok(devices);
            }
            let adapter1 = match Adapter1::from_adapter(&adapter) {
                Ok(adapter) => adapter,
                Err(_) => {
                    devices.npu = Err(Metric::unavailable(
                        MetricState::Unsupported,
                        "DXCore",
                        format!("{name}: IDXCoreAdapter1 미지원"),
                    ));
                    return Ok(devices);
                }
            };
            let physical_count = property_u32(&adapter, PHYSICAL_ADAPTER_COUNT)?;
            if physical_count == 0 || physical_count > 64 {
                devices.npu = Err(Metric::unavailable(
                    MetricState::Unsupported,
                    "DXCore",
                    format!("{name}: 물리 어댑터 수 범위 오류"),
                ));
                return Ok(devices);
            }
            for physical in 0..physical_count {
                let engine_count = adapter1.engine_count(physical)?;
                if engine_count == 0
                    || engine_count > 256
                    || engines.len() + engine_count as usize > 4096
                {
                    devices.npu = Err(Metric::unavailable(
                        MetricState::Unsupported,
                        "DXCore",
                        format!("{name}: 엔진 수 범위 오류"),
                    ));
                    return Ok(devices);
                }
                for engine in 0..engine_count {
                    engines.push(NpuEngine {
                        adapter: adapter.clone(),
                        name: name.clone(),
                        index: EngineIndex {
                            physical_adapter: physical,
                            engine,
                        },
                        previous: None,
                    });
                }
            }
        }
        devices.npu = Ok(engines);
        Ok(devices)
    }

    fn needs_retry(&self) -> bool {
        self.failed
            || self
                .list
                .as_ref()
                .is_some_and(|list| unsafe { list.IsStale() })
    }

    fn sample_npu(&mut self) -> Metric {
        let engines = match &mut self.npu {
            Ok(engines) => engines,
            Err(metric) => return metric.clone(),
        };
        let mut busiest: Option<(f64, Duration, String)> = None;
        let mut warming = false;
        for engine in engines.iter_mut() {
            if !unsafe { engine.adapter.IsValid() } {
                self.failed = true;
                for engine in engines.iter_mut() {
                    engine.previous = None;
                }
                return Metric::unavailable(
                    MetricState::Error,
                    "DXCore",
                    "NPU 장치가 변경되었습니다. 다시 확인 중",
                );
            }
            let mut running = 0u64;
            let result = unsafe {
                engine.adapter.QueryState(
                    ENGINE_RUNNING_TIME,
                    size_of::<EngineIndex>(),
                    Some((&engine.index as *const EngineIndex).cast()),
                    size_of::<u64>(),
                    (&mut running as *mut u64).cast(),
                )
            };
            if let Err(error) = result {
                self.failed = true;
                for engine in engines.iter_mut() {
                    engine.previous = None;
                }
                return Metric::unavailable(
                    MetricState::Error,
                    "DXCore",
                    format!("NPU 누적 시간 읽기 실패: {error}"),
                );
            }
            let now = Instant::now();
            if let Some((previous, at)) = engine.previous {
                let window = now.duration_since(at);
                if let Some(percent) = engine_busy(previous, running, window) {
                    if busiest.as_ref().is_none_or(|current| percent > current.0) {
                        busiest = Some((
                            percent,
                            window,
                            format!(
                                "{}: 가장 바쁜 NPU 엔진 {}:{}",
                                engine.name, engine.index.physical_adapter, engine.index.engine
                            ),
                        ));
                    }
                } else {
                    warming = true;
                }
            } else {
                warming = true;
            }
            engine.previous = Some((running, now));
        }
        self.failed = false;
        if warming {
            Metric::unavailable(
                MetricState::WarmingUp,
                "DXCore AdapterEngineRunningTimeMicroseconds",
                "NPU 엔진 기준 표본 수집 중",
            )
        } else if let Some((percent, window, detail)) = busiest {
            Metric::reading(
                percent,
                "DXCore AdapterEngineRunningTimeMicroseconds",
                window,
                detail,
            )
        } else {
            Metric::unavailable(
                MetricState::Unsupported,
                "DXCore",
                "조회 가능한 NPU 엔진이 없습니다",
            )
        }
    }
}

fn property_u32(
    adapter: &IDXCoreAdapter,
    property: DXCoreAdapterProperty,
) -> windows::core::Result<u32> {
    let mut value = 0u32;
    unsafe {
        adapter.GetProperty(property, size_of::<u32>(), (&mut value as *mut u32).cast())?;
    }
    Ok(value)
}

fn property_u8(
    adapter: &IDXCoreAdapter,
    property: DXCoreAdapterProperty,
) -> windows::core::Result<u8> {
    let mut value = 0u8;
    unsafe {
        adapter.GetProperty(property, size_of::<u8>(), (&mut value as *mut u8).cast())?;
    }
    Ok(value)
}

fn adapter_name(adapter: &IDXCoreAdapter) -> String {
    let size = match unsafe { adapter.GetPropertySize(DriverDescription) } {
        Ok(size) if size > 0 && size <= 16_384 => size,
        _ => return "어댑터 이름 미제공".into(),
    };
    let mut bytes = vec![0u8; size];
    if unsafe { adapter.GetProperty(DriverDescription, bytes.len(), bytes.as_mut_ptr().cast()) }
        .is_err()
    {
        return "어댑터 이름 읽기 실패".into();
    }
    let end = bytes
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_counter_instances_are_warming_while_real_failures_remain_errors() {
        for status in [
            PDH_CSTATUS_NO_INSTANCE,
            PDH_CSTATUS_ITEM_NOT_VALIDATED,
            PDH_CSTATUS_INVALID_DATA,
            PDH_NO_DATA,
            PDH_CALC_NEGATIVE_TIMEBASE,
        ] {
            let metric = pdh_metric(status, "test");
            assert_eq!(metric.state, MetricState::WarmingUp);
            assert_eq!(metric.value, None);
        }
        assert_eq!(
            pdh_metric(PDH_CSTATUS_NO_OBJECT, "test").state,
            MetricState::Unsupported
        );
        assert_eq!(pdh_metric(0xc000_0bbc, "test").state, MetricState::Error);
    }
}
