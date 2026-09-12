//! Per-user logon registration, shared with the installer. This describes the
//! documented Run value, not Windows' separate Startup-app approval state.
//! Never modify the undocumented StartupApproved data or register diagnostics args.

use std::{io, os::windows::ffi::OsStrExt, path::Path};
use windows::{
    Win32::{
        Foundation::{ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, WIN32_ERROR},
        System::Registry::*,
    },
    core::{PCWSTR, w},
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("TaskbarMonitor");
// Microsoft documents a maximum 260-character command, excluding its terminator:
// https://learn.microsoft.com/windows/win32/setupapi/run-and-runonce-registry-keys
const MAX_COMMAND_UNITS: usize = 260;
const MAX_VALUE_BYTES: usize = (MAX_COMMAND_UNITS + 1) * 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupState {
    Disabled,
    Enabled,
    /// This value belongs to another location or command. Do not overwrite it
    /// when a portable copy happens to be running alongside an installed copy.
    OtherRegistration,
}

/// Read on menu opening, not on a timer. The registry is the source of truth;
/// no startup preference is cached in widget.json.
pub fn read() -> io::Result<StartupState> {
    let command = current_command()?;
    read_at(RUN_KEY, &command)
}

/// Called only for an explicit user toggle. Removing an absent registration is
/// harmless. A malformed or foreign registration is preserved and reported.
/// A post-write query confirms the requested registration state was persisted.
pub fn set_enabled(enabled: bool) -> io::Result<()> {
    let command = current_command()?;
    set_at(RUN_KEY, &command, enabled)
}

fn current_command() -> io::Result<Vec<u16>> {
    executable_command(&std::env::current_exe()?)
}

fn executable_command(executable: &Path) -> io::Result<Vec<u16>> {
    if !executable.is_absolute() || executable.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Windows startup requires an absolute executable path",
        ));
    }
    let path: Vec<u16> = executable.as_os_str().encode_wide().collect();
    if path.iter().any(|unit| *unit == 0 || *unit == b'"' as u16)
        || String::from_utf16(&path).is_err()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The executable path cannot be represented by a Windows startup command",
        ));
    }
    if path.len() + 2 > MAX_COMMAND_UNITS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "The quoted executable path exceeds the 260-character Windows startup limit",
        ));
    }
    // Quoting is mandatory even when today's path contains no spaces. A Run
    // value invokes the executable directly, with no shell and no copied args.
    let mut command = Vec::with_capacity(path.len() + 3);
    command.push(b'"' as u16);
    command.extend(path);
    command.push(b'"' as u16);
    command.push(0);
    Ok(command)
}

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn check(status: WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status.0 as i32))
    }
}

fn open_key(path: PCWSTR, access: REG_SAM_FLAGS) -> io::Result<Option<Key>> {
    let mut handle = HKEY::default();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path, None, access, &mut handle) };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    check(status)?;
    Ok(Some(Key(handle)))
}

fn create_key(path: PCWSTR) -> io::Result<Key> {
    let mut handle = HKEY::default();
    check(unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path,
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_QUERY_VALUE | KEY_SET_VALUE,
            None,
            &mut handle,
            None,
        )
    })?;
    Ok(Key(handle))
}

fn read_at(path: PCWSTR, expected: &[u16]) -> io::Result<StartupState> {
    match open_key(path, KEY_QUERY_VALUE)? {
        Some(key) => query(&key, expected),
        None => Ok(StartupState::Disabled),
    }
}

fn query(key: &Key, expected: &[u16]) -> io::Result<StartupState> {
    // One bounded native read: values that grow concurrently cannot cause an
    // unbounded allocation. RegQueryValueEx preserves malformed terminators so
    // validation can reject them instead of silently repairing stored data.
    let mut bytes = [0u8; MAX_VALUE_BYTES];
    let mut byte_count = bytes.len() as u32;
    let mut kind = REG_VALUE_TYPE::default();
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            VALUE_NAME,
            None,
            Some(&mut kind),
            Some(bytes.as_mut_ptr()),
            Some(&mut byte_count),
        )
    };
    if status == ERROR_FILE_NOT_FOUND {
        return Ok(StartupState::Disabled);
    }
    if status == ERROR_MORE_DATA || byte_count as usize > bytes.len() {
        return Err(invalid_registration(
            "Windows startup registration is too large",
        ));
    }
    check(status)?;
    classify(kind, &bytes[..byte_count as usize], expected)
}

fn invalid_registration(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn classify(kind: REG_VALUE_TYPE, bytes: &[u8], expected: &[u16]) -> io::Result<StartupState> {
    if kind != REG_SZ || bytes.len() < 2 || bytes.len() % 2 != 0 || bytes.len() > MAX_VALUE_BYTES {
        return Err(invalid_registration(
            "Invalid Windows startup registry value",
        ));
    }
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    if units.last() != Some(&0)
        || units[..units.len() - 1].contains(&0)
        || String::from_utf16(&units[..units.len() - 1]).is_err()
    {
        return Err(invalid_registration(
            "Invalid Windows startup registry string",
        ));
    }
    // Conservatively accept ASCII case changes only. Do not normalize arbitrary
    // Unicode, expand variables, parse arguments, or resolve another path as ours.
    let ascii_fold = |unit: u16| {
        if (b'A' as u16..=b'Z' as u16).contains(&unit) {
            unit + 32
        } else {
            unit
        }
    };
    if units.len() == expected.len()
        && units
            .iter()
            .zip(expected)
            .all(|(left, right)| ascii_fold(*left) == ascii_fold(*right))
    {
        Ok(StartupState::Enabled)
    } else {
        Ok(StartupState::OtherRegistration)
    }
}

fn encode_bytes(units: &[u16]) -> Vec<u8> {
    units.iter().flat_map(|unit| unit.to_le_bytes()).collect()
}

fn set_at(path: PCWSTR, expected: &[u16], enabled: bool) -> io::Result<()> {
    let key = match open_key(path, KEY_QUERY_VALUE | KEY_SET_VALUE)? {
        Some(key) => key,
        None if !enabled => return Ok(()),
        None => create_key(path)?,
    };
    // Recheck immediately before mutation; an open menu may hold stale state.
    // Registry APIs do not provide a compare-and-swap operation for a Run value.
    let before = query(&key, expected)?;
    if before == StartupState::OtherRegistration {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Windows startup is registered to another location or command",
        ));
    }
    let wanted = if enabled {
        StartupState::Enabled
    } else {
        StartupState::Disabled
    };
    if before == wanted {
        return Ok(());
    }
    if enabled {
        check(unsafe {
            RegSetValueExW(
                key.0,
                VALUE_NAME,
                None,
                REG_SZ,
                Some(&encode_bytes(expected)),
            )
        })?;
    } else {
        let status = unsafe { RegDeleteValueW(key.0, VALUE_NAME) };
        if status != ERROR_FILE_NOT_FOUND {
            check(status)?;
        }
    }
    if query(&key, expected)? != wanted {
        return Err(io::Error::other(
            "Windows startup registration changed during the update",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        os::windows::ffi::OsStringExt,
        sync::atomic::{AtomicU64, Ordering},
    };

    fn command() -> Vec<u16> {
        executable_command(Path::new(r"C:\사용자\Test Folder\taskbar-monitor.exe")).unwrap()
    }

    #[test]
    fn executable_path_is_quoted_without_process_arguments() {
        let value = command();
        assert_eq!(value.last(), Some(&0));
        assert_eq!(
            String::from_utf16(&value[..value.len() - 1]).unwrap(),
            "\"C:\\사용자\\Test Folder\\taskbar-monitor.exe\""
        );
        for path in [
            "relative.exe",
            r"C:relative.exe",
            "C:\\bad\"path.exe",
            "C:\\bad\0path.exe",
        ] {
            assert!(executable_command(Path::new(path)).is_err());
        }
    }

    #[test]
    fn command_limit_counts_utf16_units_and_quotes() {
        let exact = format!("C:\\{}", "x".repeat(255));
        assert_eq!(executable_command(Path::new(&exact)).unwrap().len(), 261);
        assert!(executable_command(Path::new(&(exact + "x"))).is_err());
        let astral = format!("C:\\{}", "🖥".repeat(128));
        assert!(executable_command(Path::new(&astral)).is_err());
        let invalid = OsString::from_wide(&[b'C' as u16, b':' as u16, b'\\' as u16, 0xd800]);
        assert!(executable_command(Path::new(&invalid)).is_err());
    }

    #[test]
    fn codec_rejects_malformed_registry_data() {
        let expected = command();
        for bytes in [
            vec![],
            vec![0],
            vec![0, 0, 0],
            encode_bytes(&[65]),
            encode_bytes(&[65, 0, 66, 0]),
            encode_bytes(&[0xd800, 0]),
            encode_bytes(&vec![65; MAX_COMMAND_UNITS + 2]),
        ] {
            assert!(classify(REG_SZ, &bytes, &expected).is_err());
        }
        for kind in [REG_BINARY, REG_EXPAND_SZ, REG_MULTI_SZ, REG_DWORD] {
            assert!(classify(kind, &encode_bytes(&expected), &expected).is_err());
        }
    }

    #[test]
    fn foreign_path_or_arguments_are_not_owned() {
        let expected = command();
        assert_eq!(
            classify(REG_SZ, &encode_bytes(&expected), &expected).unwrap(),
            StartupState::Enabled
        );
        let upper: Vec<u16> = String::from_utf16(&expected)
            .unwrap()
            .to_ascii_uppercase()
            .encode_utf16()
            .collect();
        assert_eq!(
            classify(REG_SZ, &encode_bytes(&upper), &expected).unwrap(),
            StartupState::Enabled
        );
        for text in [
            "\"C:\\Other\\taskbar-monitor.exe\"",
            "\"C:\\사용자\\Test Folder\\taskbar-monitor.exe\" --probe",
            "",
            "%LOCALAPPDATA%\\TaskbarMonitor\\taskbar-monitor.exe",
        ] {
            let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
            assert_eq!(
                classify(REG_SZ, &encode_bytes(&units), &expected).unwrap(),
                StartupState::OtherRegistration
            );
        }
    }

    struct TestKey {
        path: Vec<u16>,
    }

    impl TestKey {
        fn new() -> Self {
            static SEQUENCE: AtomicU64 = AtomicU64::new(0);
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let name = format!(
                r"Software\TaskbarMonitor\Tests\Startup-{}-{timestamp}-{}",
                std::process::id(),
                SEQUENCE.fetch_add(1, Ordering::Relaxed)
            );
            let path: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            assert!(
                open_key(PCWSTR(path.as_ptr()), KEY_QUERY_VALUE)
                    .unwrap()
                    .is_none()
            );
            Self { path }
        }
        fn path(&self) -> PCWSTR {
            PCWSTR(self.path.as_ptr())
        }
    }

    impl Drop for TestKey {
        fn drop(&mut self) {
            // Exact unique test leaf only; never recurse or touch Run/StartupApproved.
            let status = unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, self.path()) };
            if !std::thread::panicking() {
                assert!(status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND);
            }
        }
    }

    #[test]
    fn isolated_registry_roundtrip_preserves_foreign_and_malformed_values() {
        fn stored_value(key: &Key) -> (REG_VALUE_TYPE, Vec<u8>) {
            // Read what Windows actually persisted: some systems add a string
            // terminator at write time. Preservation must compare stored bytes,
            // not assume that an intentionally malformed write stays malformed.
            let mut bytes = vec![0u8; MAX_VALUE_BYTES + 8];
            let mut length = bytes.len() as u32;
            let mut kind = REG_VALUE_TYPE::default();
            check(unsafe {
                RegQueryValueExW(
                    key.0,
                    VALUE_NAME,
                    None,
                    Some(&mut kind),
                    Some(bytes.as_mut_ptr()),
                    Some(&mut length),
                )
            })
            .unwrap();
            bytes.truncate(length as usize);
            (kind, bytes)
        }

        let test = TestKey::new();
        let expected = command();
        assert_eq!(
            read_at(test.path(), &expected).unwrap(),
            StartupState::Disabled
        );
        set_at(test.path(), &expected, false).unwrap();
        assert!(open_key(test.path(), KEY_QUERY_VALUE).unwrap().is_none());
        set_at(test.path(), &expected, true).unwrap();
        set_at(test.path(), &expected, true).unwrap();
        assert_eq!(
            read_at(test.path(), &expected).unwrap(),
            StartupState::Enabled
        );
        set_at(test.path(), &expected, false).unwrap();
        set_at(test.path(), &expected, false).unwrap();
        assert_eq!(
            read_at(test.path(), &expected).unwrap(),
            StartupState::Disabled
        );

        let key = open_key(test.path(), KEY_QUERY_VALUE | KEY_SET_VALUE)
            .unwrap()
            .unwrap();
        let foreign = executable_command(Path::new(r"C:\Other\taskbar-monitor.exe")).unwrap();
        check(unsafe {
            RegSetValueExW(
                key.0,
                VALUE_NAME,
                None,
                REG_SZ,
                Some(&encode_bytes(&foreign)),
            )
        })
        .unwrap();
        for enabled in [false, true] {
            assert!(set_at(test.path(), &expected, enabled).is_err());
        }
        assert_eq!(
            read_at(test.path(), &foreign).unwrap(),
            StartupState::Enabled
        );

        for (kind, bytes) in [
            (REG_BINARY, vec![1, 2, 3, 4]),
            (REG_SZ, vec![65, 0]),
            (REG_SZ, vec![65; MAX_VALUE_BYTES + 2]),
        ] {
            check(unsafe { RegSetValueExW(key.0, VALUE_NAME, None, kind, Some(&bytes)) }).unwrap();
            let before = stored_value(&key);
            assert!(matches!(
                read_at(test.path(), &expected),
                Err(_) | Ok(StartupState::OtherRegistration)
            ));
            for enabled in [false, true] {
                assert!(set_at(test.path(), &expected, enabled).is_err());
                assert_eq!(stored_value(&key), before);
            }
        }
    }
}
