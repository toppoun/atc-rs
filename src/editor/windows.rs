use super::{
    ResolvedEditor, WindowsProcess, select_windows_ancestor, windows_environment_brand,
    windows_integrated_editor,
};
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io;
use std::mem::size_of;
use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _, OwnedHandle};
use std::path::PathBuf;
use windows_sys::Win32::Foundation::{
    ERROR_NO_MORE_FILES, FILETIME, INVALID_HANDLE_VALUE, LocalFree,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows_sys::Win32::UI::Shell::CommandLineToArgvW;

const MAX_ANCESTORS: usize = 64;
const MAX_WINDOWS_PATH_CODE_UNITS: usize = 32_768;

pub(super) fn parse_command_line(value: &OsStr) -> Result<Vec<OsString>, &'static str> {
    let wide = value.encode_wide().collect::<Vec<_>>();
    let start = wide
        .iter()
        .position(|unit| !is_ascii_command_whitespace(*unit))
        .unwrap_or(wide.len());
    let end = wide
        .iter()
        .rposition(|unit| !is_ascii_command_whitespace(*unit))
        .map_or(start, |index| index + 1);
    let mut wide = wide[start..end].to_vec();
    if !quotes_are_balanced(&wide) {
        return Err("the quoting is malformed");
    }
    wide.push(0);
    let mut count = 0_i32;
    // SAFETY: `wide` is nonempty, NUL-terminated, and remains alive for the call; `count` is a
    // valid writable out-parameter.
    let arguments = unsafe { CommandLineToArgvW(wide.as_ptr(), &raw mut count) };
    if arguments.is_null() {
        return Err("Windows could not parse the command line");
    }

    // SAFETY: a successful CommandLineToArgvW call returns `count` valid pointers to
    // NUL-terminated strings in one allocation that remains live until LocalFree below.
    let words = unsafe {
        std::slice::from_raw_parts(arguments, count as usize)
            .iter()
            .map(|argument| {
                let mut length = 0;
                while *argument.add(length) != 0 {
                    length += 1;
                }
                OsString::from_wide(std::slice::from_raw_parts(*argument, length))
            })
            .collect()
    };
    // SAFETY: `arguments` is the allocation returned by CommandLineToArgvW and is freed exactly
    // once after all borrowed pointers have been copied into owned OsStrings.
    unsafe {
        LocalFree(arguments.cast());
    }
    Ok(words)
}

fn is_ascii_command_whitespace(unit: u16) -> bool {
    matches!(unit, 0x09..=0x0d | 0x20)
}

fn quotes_are_balanced(value: &[u16]) -> bool {
    let quote = u16::from(b'"');
    let backslash = u16::from(b'\\');
    let mut quoted = false;
    let mut preceding_backslashes = 0;
    for &unit in value {
        if unit == quote && preceding_backslashes % 2 == 0 {
            quoted = !quoted;
        }
        preceding_backslashes = if unit == backslash {
            preceding_backslashes + 1
        } else {
            0
        };
    }
    !quoted
}

pub(super) fn detect_integrated_editor() -> Option<ResolvedEditor> {
    if let Ok(processes) = ancestor_processes()
        && let Some(editor) = select_windows_ancestor(&processes)
    {
        return Some(editor);
    }

    let term_program = std::env::var_os("TERM_PROGRAM");
    let evidence = [
        std::env::var_os("VSCODE_GIT_ASKPASS_NODE"),
        std::env::var_os("VSCODE_GIT_ASKPASS_MAIN"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    let brand = windows_environment_brand(term_program.as_deref(), &evidence)?;
    let executable = evidence.into_iter().map(PathBuf::from).find(|path| {
        path.is_absolute()
            && path.is_file()
            && super::classify_integrated_executable(path.as_os_str()) == Some(brand)
    });
    Some(windows_integrated_editor(brand, executable))
}

#[derive(Clone)]
struct SnapshotEntry {
    parent_process_id: u32,
    executable_name: OsString,
}

#[derive(Clone)]
struct InspectedProcess {
    creation_time: u64,
    executable_path: Option<PathBuf>,
}

fn ancestor_processes() -> io::Result<Vec<WindowsProcess>> {
    // SAFETY: TH32CS_SNAPPROCESS ignores the process-id argument and returns either an owned
    // snapshot handle or INVALID_HANDLE_VALUE.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: INVALID_HANDLE_VALUE was rejected above, and ownership is transferred exactly once
    // so OwnedHandle closes the snapshot on every return path.
    let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
    let entries = snapshot_entries(&snapshot)?;
    Ok(collect_ancestor_processes(
        &entries,
        std::process::id(),
        inspect_process,
    ))
}

fn collect_ancestor_processes(
    entries: &HashMap<u32, SnapshotEntry>,
    mut process_id: u32,
    mut inspect: impl FnMut(u32) -> Option<InspectedProcess>,
) -> Vec<WindowsProcess> {
    let Some(mut child) = inspect(process_id) else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut ancestors = Vec::new();

    for _ in 0..MAX_ANCESTORS {
        if !seen.insert(process_id) {
            break;
        }
        let Some(current) = entries.get(&process_id) else {
            break;
        };
        process_id = current.parent_process_id;
        if process_id == 0 {
            break;
        }
        let Some(parent) = entries.get(&process_id) else {
            break;
        };
        let Some(inspected_parent) = inspect(process_id) else {
            break;
        };
        // A real parent must predate its child. If the snapshot's stored parent PID now names a
        // newer process, the original parent exited and Windows reused the PID; do not traverse
        // into that unrelated process tree.
        if inspected_parent.creation_time > child.creation_time {
            break;
        }
        ancestors.push(WindowsProcess {
            executable_name: parent.executable_name.clone(),
            executable_path: inspected_parent.executable_path.clone(),
        });
        child = inspected_parent;
    }
    ancestors
}

fn snapshot_entries(snapshot: &OwnedHandle) -> io::Result<HashMap<u32, SnapshotEntry>> {
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..PROCESSENTRY32W::default()
    };
    // SAFETY: the snapshot handle is live, `entry` has the required dwSize, and the pointer is a
    // valid writable PROCESSENTRY32W for the duration of the call.
    if unsafe { Process32FirstW(snapshot.as_raw_handle(), &raw mut entry) } == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut entries = HashMap::new();
    loop {
        entries.insert(
            entry.th32ProcessID,
            SnapshotEntry {
                parent_process_id: entry.th32ParentProcessID,
                executable_name: os_string_from_nul_terminated(&entry.szExeFile),
            },
        );
        // SAFETY: the same live snapshot and initialized writable entry used by Process32FirstW
        // remain valid throughout enumeration.
        if unsafe { Process32NextW(snapshot.as_raw_handle(), &raw mut entry) } == 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                break;
            }
            return Err(error);
        }
    }
    Ok(entries)
}

fn inspect_process(process_id: u32) -> Option<InspectedProcess> {
    // SAFETY: OpenProcess receives a PID value and requests query-only access without inheriting
    // the returned handle.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }
    // SAFETY: null was rejected above, and ownership is transferred exactly once so OwnedHandle
    // closes the process handle on every return path.
    let process = unsafe { OwnedHandle::from_raw_handle(process) };
    let creation_time = query_process_creation_time(&process)?;
    let executable_path = query_process_image_path(&process);
    Some(InspectedProcess {
        creation_time,
        executable_path,
    })
}

fn query_process_creation_time(process: &OwnedHandle) -> Option<u64> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: `process` is a live query handle and all four FILETIME out-pointers are valid and
    // writable for the call.
    if unsafe {
        GetProcessTimes(
            process.as_raw_handle(),
            &raw mut creation,
            &raw mut exit,
            &raw mut kernel,
            &raw mut user,
        )
    } == 0
    {
        return None;
    }
    Some((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
}

fn query_process_image_path(process: &OwnedHandle) -> Option<PathBuf> {
    let mut buffer = vec![0_u16; MAX_WINDOWS_PATH_CODE_UNITS];
    let mut length = buffer.len() as u32;
    // SAFETY: `process` is a live query handle, the buffer has `length` writable UTF-16 code
    // units, and `length` is a valid in/out parameter.
    if unsafe {
        QueryFullProcessImageNameW(
            process.as_raw_handle(),
            0,
            buffer.as_mut_ptr(),
            &raw mut length,
        )
    } == 0
    {
        return None;
    }
    buffer.truncate(length as usize);
    Some(PathBuf::from(OsString::from_wide(&buffer)))
}

fn os_string_from_nul_terminated(value: &[u16]) -> OsString {
    let length = value
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(value.len());
    OsString::from_wide(&value[..length])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_validation_understands_escaped_quotes() {
        let balanced = OsStr::new(r#""C:\Editor\editor.exe" "a\\\"b""#)
            .encode_wide()
            .collect::<Vec<_>>();
        assert!(quotes_are_balanced(&balanced));
        assert!(!quotes_are_balanced(
            &OsStr::new("\"unfinished").encode_wide().collect::<Vec<_>>()
        ));
    }

    #[test]
    fn parser_ignores_surrounding_ascii_whitespace() {
        assert_eq!(
            parse_command_line(OsStr::new("  nvim -f\t")).unwrap(),
            ["nvim", "-f"]
        );
    }

    #[test]
    fn parser_preserves_unicode_spaces_quotes_and_windows_backslashes() {
        assert_eq!(
            parse_command_line(OsStr::new(
                r#"  "C:\編集\editor.exe" "two words" "say \"hello\"" "C:\dir with spaces\\"  "#,
            ))
            .unwrap(),
            [
                r"C:\編集\editor.exe",
                "two words",
                r#"say "hello""#,
                r"C:\dir with spaces\",
            ]
        );
    }

    #[test]
    fn ancestor_collection_stops_at_a_reused_parent_pid() {
        let entries = HashMap::from([
            (
                10,
                SnapshotEntry {
                    parent_process_id: 20,
                    executable_name: "atc.exe".into(),
                },
            ),
            (
                20,
                SnapshotEntry {
                    parent_process_id: 30,
                    executable_name: "Code.exe".into(),
                },
            ),
        ]);

        let ancestors = collect_ancestor_processes(&entries, 10, |process_id| {
            Some(InspectedProcess {
                creation_time: match process_id {
                    10 => 100,
                    // A genuine parent cannot have been created after its child.
                    20 => 200,
                    _ => return None,
                },
                executable_path: None,
            })
        });

        assert!(ancestors.is_empty());
    }
}
