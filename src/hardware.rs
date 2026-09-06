//! Startup-only local hardware inventory. Call `load` on a background thread.
//! WMI requests only named display properties; identifiers such as serial numbers
//! are neither requested nor retained. This module does not sample performance.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};
use windows::Win32::System::Com::*;
use windows::Win32::System::Variant::{VARIANT, VT_EMPTY, VT_NULL, VariantClear, VariantToString};
use windows::Win32::System::Wmi::*;
use windows::core::{BSTR, PCWSTR};

#[derive(Clone, Debug)]
pub struct HardwareInfo {
    pub cpu: String,
    pub ram: String,
    pub gpu: String,
    pub disk: String,
    pub cpu_detail: String,
    pub ram_detail: String,
    pub gpu_detail: String,
    pub disk_detail: String,
    pub cpu_meta: String,
    pub ram_meta: String,
    pub gpu_meta: String,
    pub disks: Vec<HardwareDisk>,
}

#[derive(Clone, Debug)]
pub struct HardwareDisk {
    /// WMI Win32_DiskDrive.Index, which corresponds to the physical disk number.
    pub index: u32,
    pub name: String,
    pub detail: String,
    /// Zero means that this property was not supplied by the provider.
    pub capacity_bytes: u64,
}

impl Default for HardwareInfo {
    fn default() -> Self {
        Self {
            cpu: "CPU".into(),
            ram: "RAM".into(),
            gpu: "GPU".into(),
            disk: "DISK".into(),
            cpu_detail: "CPU 모델 확인 중".into(),
            ram_detail: "RAM 구성 확인 중".into(),
            gpu_detail: "GPU 모델 확인 중".into(),
            disk_detail: "디스크 모델과 용량 확인 중".into(),
            cpu_meta: String::new(),
            ram_meta: String::new(),
            gpu_meta: String::new(),
            disks: Vec::new(),
        }
    }
}

/// Opens native WMI once, reads four inventories and releases all COM resources.
/// Do not call from the UI thread or from the periodic telemetry collectors.
pub fn load() -> HardwareInfo {
    let mut info = HardwareInfo::default();
    let connection = match WmiConnection::open() {
        Ok(connection) => connection,
        Err(error) => {
            let detail = format!("하드웨어 정보를 읽지 못했습니다: {error}");
            info.cpu_detail = detail.clone();
            info.ram_detail = detail.clone();
            info.gpu_detail = detail.clone();
            info.disk_detail = detail;
            return info;
        }
    };
    match connection.query(
        "Win32_Processor",
        &["Name", "NumberOfCores", "NumberOfLogicalProcessors"],
    ) {
        Ok(rows) => {
            (info.cpu, info.cpu_detail) = cpu_info(&rows);
            let cores = sum_property(&rows, "NumberOfCores");
            let threads = sum_property(&rows, "NumberOfLogicalProcessors");
            if let (Some(cores), Some(threads)) = (cores, threads) {
                info.cpu_meta = format!("{cores}C / {threads}T");
                info.cpu_detail.push_str(&format!(
                    "\n물리 코어 {cores}개 / 논리 프로세서 {threads}개"
                ));
            }
        }
        Err(error) => info.cpu_detail = format!("CPU 모델 조회 실패: {error}"),
    }
    match connection.query(
        "Win32_PhysicalMemory",
        &["Capacity", "SMBIOSMemoryType", "ConfiguredClockSpeed"],
    ) {
        Ok(rows) => {
            (info.ram, info.ram_detail) = ram_info(&rows);
            info.ram_meta = ram_meta(&rows);
        }
        Err(error) => info.ram_detail = format!("RAM 구성 조회 실패: {error}"),
    }
    match connection.query("Win32_VideoController", &["Name"]) {
        Ok(rows) => {
            (info.gpu, info.gpu_detail) = gpu_info(&rows);
            if rows.len() == 1 {
                if let Some(name) = field(&rows[0], "Name") {
                    info.gpu_meta = gpu_integration(name).unwrap_or_default();
                    if !info.gpu_meta.is_empty() {
                        info.gpu_detail
                            .push_str(&format!("\n{} (DXCore)", info.gpu_meta));
                    }
                }
            } else if !rows.is_empty() {
                info.gpu_meta = format!("{} GPUs", rows.len());
            }
        }
        Err(error) => info.gpu_detail = format!("GPU 모델 조회 실패: {error}"),
    }
    match connection.query("Win32_DiskDrive", &["Index", "Model", "Size"]) {
        Ok(rows) => {
            (info.disk, info.disk_detail) = disk_info(&rows);
            info.disks = disk_list(&rows);
        }
        Err(error) => info.disk_detail = format!("디스크 모델/용량 조회 실패: {error}"),
    }
    info
}

type Row = BTreeMap<&'static str, String>;

struct Apartment;
impl Drop for Apartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct WmiConnection {
    // Fields drop in declaration order: release WMI before uninitializing COM.
    services: IWbemServices,
    _apartment: Apartment,
}

impl WmiConnection {
    fn open() -> Result<Self, String> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|e| e.to_string())?;
        }
        let apartment = Apartment;
        // COM may already have initialized process-wide security (e.g. WinUI).
        // Only the private WMI proxies get explicit settings; other proxies are untouched.
        let locator: IWbemLocator = unsafe {
            CoCreateInstance(&WbemLocator, None, CLSCTX_INPROC_SERVER).map_err(|e| e.to_string())?
        };
        let empty = BSTR::new();
        let services = unsafe {
            locator
                .ConnectServer(
                    &BSTR::from("ROOT\\CIMV2"),
                    &empty,
                    &empty,
                    &empty,
                    WBEM_FLAG_CONNECT_USE_MAX_WAIT.0,
                    &empty,
                    None,
                )
                .map_err(|e| e.to_string())?
        };
        unsafe {
            // RPC_C_AUTHN_WINNT=10, RPC_C_AUTHZ_NONE=0; current local user only.
            CoSetProxyBlanket(
                &services,
                10,
                0,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
            .map_err(|e| e.to_string())?;
        }
        Ok(Self {
            services,
            _apartment: apartment,
        })
    }

    fn query(&self, class: &str, properties: &[&'static str]) -> Result<Vec<Row>, String> {
        let statement = format!("SELECT {} FROM {class}", properties.join(","));
        let enumerator = unsafe {
            self.services
                .ExecQuery(
                    &BSTR::from("WQL"),
                    &BSTR::from(statement),
                    WBEM_FLAG_FORWARD_ONLY | WBEM_FLAG_RETURN_IMMEDIATELY,
                    None,
                )
                .map_err(|e| e.to_string())?
        };
        unsafe {
            CoSetProxyBlanket(
                &enumerator,
                10,
                0,
                PCWSTR::null(),
                RPC_C_AUTHN_LEVEL_CALL,
                RPC_C_IMP_LEVEL_IMPERSONATE,
                None,
                EOAC_NONE,
            )
            .map_err(|e| e.to_string())?;
        }
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut rows = Vec::new();
        loop {
            if Instant::now() >= deadline {
                // Do not publish partial inventories as a complete capacity/device count.
                return Err("WMI 장치 열거 제한 시간 초과".into());
            }
            let mut objects = [None];
            let mut returned = 0;
            let result = unsafe { enumerator.Next(200, &mut objects, &mut returned) };
            if result.is_err() {
                return Err(windows::core::Error::from_hresult(result).to_string());
            }
            if let Some(object) = objects[0].take() {
                let mut row = Row::new();
                for &property in properties {
                    if let Some(value) = read_property(&object, property) {
                        row.insert(property, value);
                    }
                }
                rows.push(row);
                if rows.len() > 128 {
                    return Err("하드웨어 장치 수가 조회 범위를 초과합니다".into());
                }
            }
            // WBEM_S_FALSE is end-of-enumeration. WBEM_S_TIMEDOUT is not the end.
            if result.0 == 1 {
                return Ok(rows);
            }
        }
    }
}

struct OwnedVariant(VARIANT);
impl Drop for OwnedVariant {
    fn drop(&mut self) {
        unsafe {
            let _ = VariantClear(&mut self.0);
        }
    }
}

fn read_property(object: &IWbemClassObject, property: &str) -> Option<String> {
    let name: Vec<u16> = property.encode_utf16().chain(Some(0)).collect();
    let mut value = OwnedVariant(VARIANT::default());
    unsafe {
        object
            .Get(PCWSTR(name.as_ptr()), 0, &mut value.0, None, None)
            .ok()?;
    }
    let kind = unsafe { value.0.Anonymous.Anonymous.vt };
    if kind == VT_EMPTY || kind == VT_NULL {
        return None;
    }
    let mut text = [0u16; 512];
    unsafe {
        VariantToString(&value.0, &mut text).ok()?;
    }
    let end = text.iter().position(|&ch| ch == 0).unwrap_or(text.len());
    let result = String::from_utf16_lossy(&text[..end]).trim().to_owned();
    (!result.is_empty()).then_some(result)
}

fn field<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key).map(String::as_str)
}
fn number(row: &Row, key: &str) -> Option<u64> {
    field(row, key)?.parse().ok()
}

fn sum_property(rows: &[Row], key: &str) -> Option<u64> {
    if rows.is_empty() {
        return None;
    }
    rows.iter().try_fold(0u64, |sum, row| {
        sum.checked_add(number(row, key).filter(|&n| n > 0)?)
    })
}

fn cpu_info(rows: &[Row]) -> (String, String) {
    let names: Vec<_> = rows.iter().filter_map(|row| field(row, "Name")).collect();
    match names.as_slice() {
        [] => (
            "CPU".into(),
            "WMI에서 CPU 모델을 제공하지 않았습니다".into(),
        ),
        [name] if rows.len() == 1 => (compact_cpu(name), format!("CPU: {name}")),
        _ => (
            format!("{} CPUs", rows.len()),
            format!("CPU {}개: {}", rows.len(), names.join(" / ")),
        ),
    }
}

fn ram_info(rows: &[Row]) -> (String, String) {
    if rows.is_empty() {
        return (
            "RAM".into(),
            "WMI에서 메모리 모듈 정보를 제공하지 않았습니다".into(),
        );
    }
    let total = sum_property(rows, "Capacity");
    let types: BTreeSet<_> = rows
        .iter()
        .filter_map(|row| number(row, "SMBIOSMemoryType"))
        .collect();
    let all_types_known = rows.iter().all(|row| {
        number(row, "SMBIOSMemoryType")
            .and_then(memory_type)
            .is_some()
    });
    let capacity = total
        .filter(|&capacity| capacity > 0)
        .map(ram_capacity)
        .unwrap_or_else(|| "용량 미상".into());
    let kind = if all_types_known && types.len() == 1 {
        memory_type(*types.first().unwrap())
            .unwrap_or("RAM")
            .to_owned()
    } else if types.len() > 1 {
        "혼합 RAM".into()
    } else {
        "RAM".into()
    };
    let label = format!("{capacity} {kind}");
    let mut lines = vec![format!("설치 메모리: {capacity} / 모듈 {}개", rows.len())];
    for (index, row) in rows.iter().enumerate() {
        let capacity = number(row, "Capacity")
            .filter(|&n| n > 0)
            .map(ram_capacity)
            .unwrap_or_else(|| "용량 미상".into());
        let kind = number(row, "SMBIOSMemoryType")
            .and_then(memory_type)
            .unwrap_or("종류 미상");
        let speed = number(row, "ConfiguredClockSpeed")
            .filter(|&n| n > 0)
            .map(|n| n.to_string())
            .unwrap_or_else(|| "미제공".into());
        lines.push(format!(
            "모듈 {}: {capacity}, {kind}, 구성 속도 {speed}",
            index + 1
        ));
    }
    lines.push("속도는 SMBIOS ConfiguredClockSpeed 보고값입니다. 순간 동작 클럭이 아닙니다. 용량 표시는 GiB 기준입니다.".into());
    (label, lines.join("\n"))
}

fn ram_meta(rows: &[Row]) -> String {
    if rows.is_empty() {
        return String::new();
    }
    let speeds: Option<BTreeSet<_>> = rows
        .iter()
        .map(|row| number(row, "ConfiguredClockSpeed").filter(|&speed| speed > 0))
        .collect();
    match speeds {
        Some(speeds) if speeds.len() == 1 => format!("{} MT/s", speeds.first().unwrap()),
        Some(speeds) if speeds.len() > 1 => "속도 혼합".into(),
        _ => String::new(),
    }
}

fn gpu_info(rows: &[Row]) -> (String, String) {
    let names: Vec<_> = rows.iter().filter_map(|row| field(row, "Name")).collect();
    match names.as_slice() {
        [] => (
            "GPU".into(),
            "WMI에서 GPU 모델을 제공하지 않았습니다".into(),
        ),
        [name] if rows.len() == 1 => (compact_gpu(name), format!("GPU: {name}")),
        _ => (
            format!("{} GPUs", rows.len()),
            format!(
                "그래픽 장치 {}개: {}\n사용률은 GPU 엔진 중 최댓값입니다.",
                rows.len(),
                names.join(" / ")
            ),
        ),
    }
}

fn disk_info(rows: &[Row]) -> (String, String) {
    if rows.is_empty() {
        return (
            "DISK".into(),
            "WMI에서 디스크 모델/용량을 제공하지 않았습니다".into(),
        );
    }
    let mut sorted: Vec<_> = rows.iter().collect();
    sorted.sort_by_key(|row| number(row, "Index").unwrap_or(u64::MAX));
    let total = rows.iter().try_fold(0u64, |sum, row| {
        sum.checked_add(number(row, "Size").filter(|&size| size > 0)?)
    });
    let capacity = total
        .map(disk_capacity)
        .unwrap_or_else(|| "용량 미상".into());
    let label = if rows.len() == 1 {
        let model = field(&rows[0], "Model")
            .map(compact_disk)
            .unwrap_or_else(|| "DISK".into());
        format!("{model} · {capacity}")
    } else {
        format!("{} disks · {capacity}", rows.len())
    };
    let mut lines = Vec::new();
    for row in sorted {
        let index = field(row, "Index").unwrap_or("?");
        let model = field(row, "Model").unwrap_or("모델 미제공");
        let size = number(row, "Size").filter(|&size| size > 0);
        let capacity = size
            .map(disk_capacity)
            .unwrap_or_else(|| "용량 미상".into());
        lines.push(format!("디스크 {index}: {model} / {capacity}"));
    }
    lines.push("용량은 물리 디스크 크기이며 GB/TB는 10진수 기준입니다. 사용률은 연결된 물리 디스크 중 가장 바쁜 장치의 활성률입니다.".into());
    (label, lines.join("\n"))
}

fn disk_list(rows: &[Row]) -> Vec<HardwareDisk> {
    let mut disks: Vec<_> = rows
        .iter()
        .filter_map(|row| {
            let index = number(row, "Index").and_then(|index| u32::try_from(index).ok())?;
            let model = field(row, "Model").unwrap_or("모델 미제공");
            let capacity_bytes = number(row, "Size").unwrap_or(0);
            let capacity = if capacity_bytes > 0 {
                disk_capacity(capacity_bytes)
            } else {
                "용량 미상".into()
            };
            Some(HardwareDisk {
                index,
                name: compact_disk(model),
                capacity_bytes,
                detail: format!("디스크 {index}: {model} / {capacity}"),
            })
        })
        .collect();
    disks.sort_by_key(|disk| disk.index);
    disks
}

fn gpu_integration(wmi_name: &str) -> Option<String> {
    use windows::Win32::Graphics::DXCore::*;
    let factory: IDXCoreAdapterFactory = unsafe { DXCoreCreateAdapterFactory().ok()? };
    let list: IDXCoreAdapterList = unsafe {
        factory
            .CreateAdapterList(&[DXCORE_ADAPTER_ATTRIBUTE_D3D11_GRAPHICS])
            .ok()?
    };
    for index in 0..unsafe { list.GetAdapterCount() }.min(64) {
        let adapter: IDXCoreAdapter = unsafe { list.GetAdapter(index).ok()? };
        let size = unsafe { adapter.GetPropertySize(DriverDescription).ok()? };
        if size == 0 || size > 16_384 {
            continue;
        }
        let mut bytes = vec![0u8; size];
        unsafe {
            adapter
                .GetProperty(DriverDescription, size, bytes.as_mut_ptr().cast())
                .ok()?;
        }
        let length = bytes
            .iter()
            .position(|&byte| byte == 0)
            .unwrap_or(bytes.len());
        if clean_name(&String::from_utf8_lossy(&bytes[..length])) != clean_name(wmi_name) {
            continue;
        }
        let mut hardware = 0u8;
        let mut integrated = 0u8;
        unsafe {
            if !adapter.IsPropertySupported(IsHardware)
                || !adapter.IsPropertySupported(IsIntegrated)
            {
                return None;
            }
            adapter
                .GetProperty(IsHardware, 1, (&mut hardware as *mut u8).cast())
                .ok()?;
            if hardware == 0 {
                return Some("소프트웨어 어댑터".into());
            }
            adapter
                .GetProperty(IsIntegrated, 1, (&mut integrated as *mut u8).cast())
                .ok()?;
        }
        return Some(
            if integrated == 0 {
                "외장 GPU"
            } else {
                "통합 GPU"
            }
            .into(),
        );
    }
    None
}

fn memory_type(kind: u64) -> Option<&'static str> {
    match kind {
        18 => Some("DDR"),
        19 => Some("DDR2"),
        20 => Some("DDR2 FB-DIMM"),
        24 => Some("DDR3"),
        26 => Some("DDR4"),
        27 => Some("LPDDR"),
        28 => Some("LPDDR2"),
        29 => Some("LPDDR3"),
        30 => Some("LPDDR4"),
        34 => Some("DDR5"),
        35 => Some("LPDDR5"),
        _ => None,
    }
}

fn ram_capacity(bytes: u64) -> String {
    format!("{}GB", concise_number(bytes as f64 / 1_073_741_824.0))
}
fn disk_capacity(bytes: u64) -> String {
    if bytes >= 1_000_000_000_000 {
        format!("{}TB", concise_number(bytes as f64 / 1_000_000_000_000.0))
    } else {
        format!("{}GB", concise_number(bytes as f64 / 1_000_000_000.0))
    }
}
fn concise_number(value: f64) -> String {
    let one_decimal = format!("{value:.1}");
    one_decimal
        .strip_suffix(".0")
        .unwrap_or(&one_decimal)
        .to_owned()
}

fn clean_name(name: &str) -> String {
    name.replace("(R)", "")
        .replace("(TM)", "")
        .replace(['®', '™'], "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact_cpu(name: &str) -> String {
    let clean = clean_name(name);
    let words: Vec<_> = clean.split_whitespace().collect();
    if let Some(model) = words.iter().find(|word| {
        ["i3-", "i5-", "i7-", "i9-"]
            .iter()
            .any(|prefix| word.starts_with(prefix))
    }) {
        return (*model).to_owned();
    }
    if let Some(index) = words.iter().position(|word| *word == "Ultra") {
        return words[index..(index + 3).min(words.len())].join(" ");
    }
    if let Some(index) = words.iter().position(|word| *word == "Ryzen") {
        return truncate(
            &words[index..(index + 4).min(words.len())]
                .iter()
                .take_while(|word| **word != "w/")
                .copied()
                .collect::<Vec<_>>()
                .join(" "),
            24,
        );
    }
    truncate(
        clean
            .split(" @ ")
            .next()
            .unwrap_or(&clean)
            .trim_start_matches("Intel ")
            .trim_start_matches("AMD "),
        24,
    )
}

fn compact_gpu(name: &str) -> String {
    let clean = clean_name(name);
    let clean = clean
        .trim_start_matches("Intel ")
        .trim_start_matches("NVIDIA ")
        .trim_start_matches("AMD ");
    let clean = clean
        .trim_end_matches(" Graphics")
        .trim_start_matches("GeForce ");
    truncate(clean, 24)
}

fn compact_disk(name: &str) -> String {
    let clean = clean_name(name);
    // Preserve the genuine model token rather than replacing all NVMe drives with "SSD".
    if let Some(word) = clean
        .split_whitespace()
        .find(|word| word.starts_with("SN") && word.chars().any(|ch| ch.is_ascii_digit()))
    {
        return (*word).to_owned();
    }
    truncate(
        clean
            .trim_end_matches(" USB Device")
            .trim_end_matches(" SCSI Disk Device"),
        20,
    )
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    format!(
        "{}…",
        text.chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(values: &[(&'static str, &str)]) -> Row {
        values
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect()
    }

    #[test]
    fn real_names_are_compacted_without_guessing_a_model() {
        assert_eq!(
            compact_cpu("11th Gen Intel(R) Core(TM) i5-1145G7 @ 2.60GHz"),
            "i5-1145G7"
        );
        assert_eq!(compact_gpu("Intel(R) Iris(R) Xe Graphics"), "Iris Xe");
        assert_eq!(compact_disk("WD_BLACK SN850X 2000GB"), "SN850X");
    }

    #[test]
    fn ram_preserves_installed_capacity_type_and_configured_speed() {
        let module = row(&[
            ("Capacity", "8589934592"),
            ("SMBIOSMemoryType", "26"),
            ("ConfiguredClockSpeed", "3200"),
        ]);
        assert_eq!(ram_info(&[module.clone(), module.clone()]).0, "16GB DDR4");
        assert_eq!(ram_meta(&[module.clone(), module]), "3200 MT/s");
    }

    #[test]
    fn unknown_capacity_or_mixed_speeds_are_not_reported_as_a_complete_total() {
        let a = row(&[
            ("Capacity", "8589934592"),
            ("SMBIOSMemoryType", "26"),
            ("ConfiguredClockSpeed", "3200"),
        ]);
        let b = row(&[("SMBIOSMemoryType", "26"), ("ConfiguredClockSpeed", "2666")]);
        let (label, detail) = ram_info(&[a, b]);
        assert_eq!(label, "용량 미상 DDR4");
        assert!(detail.contains("2666"));
        assert!(detail.contains("3200"));
    }

    #[test]
    fn multiple_disks_label_the_group_and_retain_each_actual_model() {
        let disks = [
            row(&[
                ("Index", "0"),
                ("Model", "WD_BLACK SN850X 2000GB"),
                ("Size", "2000396321280"),
            ]),
            row(&[
                ("Index", "1"),
                ("Model", "HIKSEMI USB Device"),
                ("Size", "503313108480"),
            ]),
        ];
        let (label, detail) = disk_info(&disks);
        assert_eq!(label, "2 disks · 2.5TB");
        assert!(detail.contains("WD_BLACK SN850X 2000GB"));
        assert!(detail.contains("HIKSEMI USB Device"));
        assert!(detail.contains("503.3GB"));
    }
}
