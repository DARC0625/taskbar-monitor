use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const APP_DIRECTORY: &str = "TaskbarMonitor";
const CONFIG_FILE: &str = "widget.json";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub theme: String,
    pub offset_dip: i32,
    pub column_dip: u32,
    pub visible: [bool; 6],
    pub style: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            theme: "auto".into(),
            offset_dip: 12,
            column_dip: 96,
            visible: [true; 6],
            style: "hud".into(),
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        resolve_paths().destination
    }
    pub fn load() -> Self {
        load_from_paths(&resolve_paths())
    }
    pub fn normalize(&mut self) {
        self.column_dip = self.column_dip.clamp(88, 116);
        if !["hud", "eva", "minimal"].contains(&self.style.as_str()) {
            self.style = "hud".into();
        }
        self.offset_dip = self.offset_dip.clamp(0, 16000);
        if !["auto", "dark", "light"].contains(&self.theme.as_str()) {
            self.theme = "auto".into();
        }
        if !self.visible.iter().any(|v| *v) {
            self.visible[0] = true;
        }
    }
    pub fn width(&self) -> i32 {
        (self.visible.iter().filter(|v| **v).count() as u32 * self.column_dip + 8) as i32
    }
    pub fn save(&self) -> io::Result<()> {
        save_at(self, &Self::path(), true)
    }
}

/// Directory for persistent application data. Portable mode requires an explicit
/// `portable.flag` beside the executable; installing under Program Files is safe.
pub fn data_dir() -> PathBuf {
    resolve_paths()
        .destination
        .parent()
        .expect("configuration path always has a parent")
        .to_path_buf()
}

/// Callers create this directory when actually writing diagnostics.
pub fn diagnostics_dir() -> PathBuf {
    data_dir().join("diagnostics")
}

#[derive(Debug, PartialEq, Eq)]
struct ConfigPaths {
    destination: PathBuf,
    legacy: Option<PathBuf>,
}

/// Pure path policy, separated from the filesystem and process environment.
fn select_paths(
    executable_dir: Option<&Path>,
    application_dir: &Path,
    portable_flag: bool,
) -> ConfigPaths {
    if let Some(directory) = executable_dir.filter(|_| portable_flag) {
        return ConfigPaths {
            destination: directory.join(CONFIG_FILE),
            legacy: None,
        };
    }
    let destination = application_dir.join(CONFIG_FILE);
    let legacy = executable_dir
        .map(|directory| directory.join(CONFIG_FILE))
        .filter(|legacy| *legacy != destination);
    ConfigPaths {
        destination,
        legacy,
    }
}

fn resolve_paths() -> ConfigPaths {
    let executable_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let portable_flag = executable_dir
        .as_ref()
        .is_some_and(|directory| directory.join("portable.flag").is_file());
    // Known folders are authoritative. If unavailable, an absolute LOCALAPPDATA
    // environment value is still preferable to the (possibly protected) install
    // directory. The final temporary-directory fallback may not survive cleanup.
    let application_dir = known_local_app_data()
        .or_else(|| {
            std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
        })
        .unwrap_or_else(std::env::temp_dir)
        .join(APP_DIRECTORY);
    select_paths(executable_dir.as_deref(), &application_dir, portable_flag)
}

fn decode(bytes: &[u8]) -> Option<Config> {
    let mut value: Config = serde_json::from_slice(bytes).ok()?;
    value.normalize();
    Some(value)
}

fn read_config(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CONFIG_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "configuration file exceeds 64 KiB",
        ));
    }
    Ok(bytes)
}

fn load_from_paths(paths: &ConfigPaths) -> Config {
    match read_config(&paths.destination) {
        Ok(bytes) => decode(&bytes).unwrap_or_default(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let legacy = paths
                .legacy
                .as_ref()
                .and_then(|path| read_config(path).ok())
                .and_then(|bytes| decode(&bytes));
            if let Some(value) = legacy {
                // Import a valid old portable config once, leaving the source intact.
                // Refuse replacement here so another instance cannot lose a newly
                // created installed config between the read and the migration.
                if save_at(&value, &paths.destination, false).is_err() {
                    if let Ok(bytes) = read_config(&paths.destination) {
                        return decode(&bytes).unwrap_or_default();
                    }
                }
                value
            } else {
                Config::default()
            }
        }
        // Existing but malformed/inaccessible installed settings must never cause
        // an older executable-adjacent configuration to override the user's data.
        Err(_) => Config::default(),
    }
}

fn save_at(value: &Config, destination: &Path, replace_existing: bool) -> io::Result<()> {
    let bytes = serde_json::to_vec_pretty(value)?;
    atomic_write(destination, &bytes, replace_existing)
}

struct PendingFile {
    path: PathBuf,
    remove_on_drop: bool,
}
impl Drop for PendingFile {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn atomic_write(destination: &Path, bytes: &[u8], replace_existing: bool) -> io::Result<()> {
    let parent = destination
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "configuration path has no parent",
            )
        })?;
    fs::create_dir_all(parent)?;
    // Canonicalizing an existing parent also supplies an extended-length Windows
    // path for MoveFileExW. Both names are on the same volume and in the same dir.
    let parent = fs::canonicalize(parent)?;
    let destination = parent.join(destination.file_name().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "configuration path has no filename",
        )
    })?);
    for _ in 0..32 {
        let nonce = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{CONFIG_FILE}.{}.{nonce}.tmp", std::process::id()));
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        };
        let mut pending = PendingFile {
            path: temporary,
            remove_on_drop: true,
        };
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            drop(file);
            return Err(error);
        }
        // Windows rename needs the writer closed first. A failed rename preserves
        // the complete old JSON and the cleanup guard removes only our temp file.
        drop(file);
        replace_file(&pending.path, &destination, replace_existing)?;
        pending.remove_on_drop = false;
        return Ok(());
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique configuration temp file",
    ))
}

#[cfg(windows)]
fn known_local_app_data() -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::FOLDERID_LocalAppData;

    struct FolderAllocation(*mut u16);
    impl Drop for FolderAllocation {
        fn drop(&mut self) {
            unsafe {
                CoTaskMemFree(Some(self.0.cast()));
            }
        }
    }
    let mut allocation = FolderAllocation(std::ptr::null_mut());
    // The raw ABI lets us free an allocated result on failure too, as required
    // by SHGetKnownFolderPath's contract.
    let result = unsafe {
        SHGetKnownFolderPath(
            &FOLDERID_LocalAppData,
            0,
            std::ptr::null_mut(),
            &mut allocation.0,
        )
    };
    if result < 0 || allocation.0.is_null() {
        return None;
    }
    let mut length = 0;
    while length < 32768 && unsafe { *allocation.0.add(length) } != 0 {
        length += 1;
    }
    if length == 0 || length == 32768 {
        return None;
    }
    let wide = unsafe { std::slice::from_raw_parts(allocation.0, length) };
    let path = PathBuf::from(OsString::from_wide(wide));
    path.is_absolute().then_some(path)
}

#[cfg(not(windows))]
fn known_local_app_data() -> Option<PathBuf> {
    None
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path, replace_existing: bool) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    const MOVEFILE_REPLACE_EXISTING: u32 = 1;
    const MOVEFILE_WRITE_THROUGH: u32 = 8;
    let flags = MOVEFILE_WRITE_THROUGH
        | if replace_existing {
            MOVEFILE_REPLACE_EXISTING
        } else {
            0
        };
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path, replace_existing: bool) -> io::Result<()> {
    if replace_existing {
        fs::rename(source, destination)
    } else {
        fs::hard_link(source, destination).and_then(|_| fs::remove_file(source))
    }
}

#[cfg(windows)]
#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn MoveFileExW(source: *const u16, destination: *const u16, flags: u32) -> i32;
}

#[cfg(windows)]
#[link(name = "shell32", kind = "raw-dylib")]
unsafe extern "system" {
    fn SHGetKnownFolderPath(
        folder: *const windows::core::GUID,
        flags: u32,
        token: *mut std::ffi::c_void,
        path: *mut *mut u16,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDirectory(PathBuf);
    impl TestDirectory {
        fn new() -> Self {
            for _ in 0..32 {
                let nonce = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "taskbar-monitor-config-test-{}-{nonce}",
                    std::process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("could not create test directory: {error}"),
                }
            }
            panic!("could not allocate test directory");
        }
    }
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            // Verify the absolute target is our uniquely created directory inside
            // the intended system temp root before recursive test cleanup.
            if let (Ok(root), Ok(temp)) = (
                fs::canonicalize(&self.0),
                fs::canonicalize(std::env::temp_dir()),
            ) {
                if root.starts_with(temp)
                    && root.file_name().is_some_and(|name| {
                        name.to_string_lossy()
                            .starts_with("taskbar-monitor-config-test-")
                    })
                {
                    let _ = fs::remove_dir_all(root);
                }
            }
        }
    }

    #[test]
    fn malformed_ranges_cannot_hide_or_collapse_widget() {
        let mut c = Config {
            theme: "unknown".into(),
            offset_dip: -80,
            column_dip: 0,
            visible: [false; 6],
            style: "unknown".into(),
        };
        c.normalize();
        assert_eq!(c.theme, "auto");
        assert_eq!(c.offset_dip, 0);
        assert!(c.width() >= 78);
        assert_eq!(c.visible.iter().filter(|v| **v).count(), 1);
    }

    #[test]
    fn path_selection_requires_explicit_portable_flag() {
        let install = Path::new("install");
        let app_data = Path::new("user-data").join(APP_DIRECTORY);
        let installed = select_paths(Some(install), &app_data, false);
        assert_eq!(installed.destination, app_data.join(CONFIG_FILE));
        assert_eq!(installed.legacy, Some(install.join(CONFIG_FILE)));
        let portable = select_paths(Some(install), &app_data, true);
        assert_eq!(portable.destination, install.join(CONFIG_FILE));
        assert_eq!(portable.legacy, None);
        let missing_executable = select_paths(None, &app_data, true);
        assert_eq!(missing_executable.destination, app_data.join(CONFIG_FILE));
        assert_eq!(missing_executable.legacy, None);
    }

    #[test]
    fn atomic_save_creates_parent_and_replaces_complete_json() {
        let directory = TestDirectory::new();
        let path = directory.0.join("settings").join(CONFIG_FILE);
        let mut value = Config::default();
        save_at(&value, &path, true).unwrap();
        value.theme = "light".into();
        value.offset_dip = 47;
        save_at(&value, &path, true).unwrap();
        let restored = decode(&read_config(&path).unwrap()).unwrap();
        assert_eq!(restored.theme, "light");
        assert_eq!(restored.offset_dip, 47);
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn old_adjacent_config_migrates_once_without_changing_the_source() {
        let directory = TestDirectory::new();
        let old_directory = directory.0.join("old");
        fs::create_dir(&old_directory).unwrap();
        let legacy = old_directory.join(CONFIG_FILE);
        let old_json = br#"{"theme":"dark","column_dip":70,"offset_dip":90}"#;
        fs::write(&legacy, old_json).unwrap();
        let paths = select_paths(Some(&old_directory), &directory.0.join("new"), false);
        let loaded = load_from_paths(&paths);
        assert_eq!(loaded.theme, "dark");
        assert_eq!(loaded.column_dip, 88);
        assert_eq!(loaded.offset_dip, 90);
        assert_eq!(fs::read(&legacy).unwrap(), old_json);
        assert_eq!(
            decode(&read_config(&paths.destination).unwrap())
                .unwrap()
                .column_dip,
            88
        );

        let installed = Config {
            theme: "light".into(),
            ..Config::default()
        };
        save_at(&installed, &paths.destination, true).unwrap();
        assert_eq!(load_from_paths(&paths).theme, "light");
    }

    #[test]
    fn existing_invalid_installed_json_is_never_overridden_from_legacy() {
        let directory = TestDirectory::new();
        let legacy = directory.0.join("old.json");
        let destination = directory.0.join(CONFIG_FILE);
        fs::write(&legacy, br#"{"theme":"dark"}"#).unwrap();
        fs::write(&destination, b"incomplete-json").unwrap();
        let paths = ConfigPaths {
            destination: destination.clone(),
            legacy: Some(legacy),
        };
        assert_eq!(load_from_paths(&paths).theme, "auto");
        assert_eq!(fs::read(&destination).unwrap(), b"incomplete-json");
    }

    #[cfg(windows)]
    #[test]
    fn denied_atomic_replacement_preserves_old_json_and_removes_temp_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let directory = TestDirectory::new();
        let path = directory.0.join(CONFIG_FILE);
        let old = Config {
            theme: "dark".into(),
            ..Config::default()
        };
        save_at(&old, &path, true).unwrap();
        // Permit readers but deliberately deny deletion/renaming while open.
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(1)
            .open(&path)
            .unwrap();
        let next = Config {
            theme: "light".into(),
            ..Config::default()
        };
        assert!(save_at(&next, &path, true).is_err());
        assert_eq!(decode(&read_config(&path).unwrap()).unwrap().theme, "dark");
        assert_eq!(fs::read_dir(&directory.0).unwrap().count(), 1);
        drop(lock);
    }
}
