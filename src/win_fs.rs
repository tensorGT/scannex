//! Direct Win32 filesystem primitives.
//! Using FindFirstFileExW with FindExInfoBasic + LARGE_FETCH because std::fs::read_dir
//! leaves performance on the table when you're trying to eat 1TB in seconds.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;

use windows_sys::Win32::Foundation::{
    ERROR_ACCESS_DENIED, ERROR_NO_MORE_FILES, FILETIME, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_COMPRESSED, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_HIDDEN,
    FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_ATTRIBUTE_SPARSE_FILE, FILE_ATTRIBUTE_SYSTEM, FIND_FIRST_EX_LARGE_FETCH,
    FindClose, FindExInfoBasic, FindExSearchNameMatch, FindFirstFileExW, FindNextFileW,
    GetCompressedFileSizeW, GetDiskFreeSpaceExW, GetVolumeInformationW, WIN32_FIND_DATAW,
};

use crate::types::VolumeInfo;

pub const INVALID_FILE_SIZE_U32: u32 = 0xFFFF_FFFF;

/// Convert a Windows path into an extended-length UTF-16 buffer (`\\?\...`)
/// so deep Windows paths (>260 chars) don't crash or get skipped.
pub fn to_extended_utf16(path: &str, out: &mut Vec<u16>) {
    out.clear();
    if !path.starts_with(r"\\?\") {
        if let Some(stripped) = path.strip_prefix(r"\\") {
            out.extend(OsStr::new(r"\\?\UNC\").encode_wide());
            out.extend(OsStr::new(stripped).encode_wide());
        } else {
            out.extend(OsStr::new(r"\\?\").encode_wide());
            out.extend(OsStr::new(path).encode_wide());
        }
    } else {
        out.extend(OsStr::new(path).encode_wide());
    }
    // Strip trailing backslash unless it's a drive root like `\\?\C:\`
    if out.len() > 7 && out.last() == Some(&(b'\\' as u16)) {
        out.pop();
    }
    out.push(0);
}

/// Append `\*` onto a null-terminated UTF-16 directory path for FindFirstFileExW.
pub fn append_wildcard(dir_utf16_no_nul: &[u16], out: &mut Vec<u16>) {
    out.clear();
    out.extend_from_slice(dir_utf16_no_nul);
    if out.last() != Some(&(b'\\' as u16)) {
        out.push(b'\\' as u16);
    }
    out.push(b'*' as u16);
    out.push(0);
}

#[inline]
pub fn filetime_to_unix(ft: FILETIME) -> i64 {
    let ticks = ((ft.dwHighDateTime as u64) << 32) | (ft.dwLowDateTime as u64);
    if ticks < 116_444_736_000_000_000 {
        return 0;
    }
    ((ticks - 116_444_736_000_000_000) / 10_000_000) as i64
}

#[inline]
pub fn is_dot_or_dotdot(name_utf16: &[u16]) -> bool {
    (name_utf16.len() == 1 && name_utf16[0] == b'.' as u16)
        || (name_utf16.len() == 2 && name_utf16[0] == b'.' as u16 && name_utf16[1] == b'.' as u16)
}

#[inline]
pub fn utf16_slice_len(buf: &[u16]) -> usize {
    buf.iter().position(|&c| c == 0).unwrap_or(buf.len())
}

pub struct DirEnumResult {
    pub access_denied: bool,
}

/// Enumerate one directory using Win32 FindFirstFileExW(FindExInfoBasic, LARGE_FETCH).
/// Calls `on_entry` for every child (excluding `.` and `..`).
pub fn enumerate_dir<F>(search_pattern_utf16: &[u16], mut on_entry: F) -> DirEnumResult
where
    F: FnMut(&[u16], &WIN32_FIND_DATAW),
{
    let mut data: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    let handle: HANDLE = unsafe {
        FindFirstFileExW(
            search_pattern_utf16.as_ptr(),
            FindExInfoBasic,
            &mut data as *mut _ as *mut _,
            FindExSearchNameMatch,
            ptr::null_mut(),
            FIND_FIRST_EX_LARGE_FETCH,
        )
    };

    if handle == INVALID_HANDLE_VALUE {
        let err = unsafe { GetLastError() };
        return DirEnumResult {
            access_denied: err == ERROR_ACCESS_DENIED,
        };
    }

    loop {
        let name_len = utf16_slice_len(&data.cFileName);
        let name_slice = &data.cFileName[..name_len];
        if !is_dot_or_dotdot(name_slice) {
            on_entry(name_slice, &data);
        }

        let ok = unsafe { FindNextFileW(handle, &mut data) };
        if ok == 0 {
            let err = unsafe { GetLastError() };
            let _ = err == ERROR_NO_MORE_FILES;
            break;
        }
    }

    unsafe {
        FindClose(handle);
    }

    DirEnumResult {
        access_denied: false,
    }
}

/// In detailed mode, ask Windows for the actual compressed/allocated size on disk.
#[inline]
pub fn query_allocated_size(full_path_utf16_nul: &[u16], logical_fallback: u64) -> u64 {
    let mut high: u32 = 0;
    let low = unsafe { GetCompressedFileSizeW(full_path_utf16_nul.as_ptr(), &mut high) };
    if low == INVALID_FILE_SIZE_U32 {
        let err = unsafe { GetLastError() };
        if err != 0 {
            return logical_fallback;
        }
    }
    ((high as u64) << 32) | (low as u64)
}

#[inline]
pub fn is_dir(attrs: u32) -> bool {
    attrs & FILE_ATTRIBUTE_DIRECTORY != 0
}

#[inline]
pub fn is_reparse_point(attrs: u32) -> bool {
    attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[inline]
pub fn attr_flags() -> (u32, u32, u32, u32, u32, u32) {
    (
        FILE_ATTRIBUTE_READONLY,
        FILE_ATTRIBUTE_HIDDEN,
        FILE_ATTRIBUTE_SYSTEM,
        FILE_ATTRIBUTE_COMPRESSED,
        FILE_ATTRIBUTE_SPARSE_FILE,
        FILE_ATTRIBUTE_OFFLINE,
    )
}

pub fn logical_cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(2)
}

/// Grab volume stats (total/free space, filesystem like NTFS) for the target root.
pub fn query_volume_info(root_path: &str) -> Option<VolumeInfo> {
    let drive_root = if root_path.len() >= 2 && root_path.as_bytes()[1] == b':' {
        format!("{}\\", &root_path[..2])
    } else {
        return None;
    };

    let wide: Vec<u16> = OsStr::new(&drive_root).encode_wide().chain(Some(0)).collect();
    let mut free_avail: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;

    let ok_space = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_avail,
            &mut total,
            &mut total_free,
        )
    };
    if ok_space == 0 {
        return None;
    }

    let mut vol_name = [0u16; 260];
    let mut fs_name = [0u16; 64];
    let ok_info = unsafe {
        GetVolumeInformationW(
            wide.as_ptr(),
            vol_name.as_mut_ptr(),
            vol_name.len() as u32,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null_mut(),
            fs_name.as_mut_ptr(),
            fs_name.len() as u32,
        )
    };

    let (volume_name, fs_name_str) = if ok_info != 0 {
        let vn = String::from_utf16_lossy(&vol_name[..utf16_slice_len(&vol_name)]);
        let fn_ = String::from_utf16_lossy(&fs_name[..utf16_slice_len(&fs_name)]);
        (vn, fn_)
    } else {
        (String::new(), "NTFS".to_string())
    };

    Some(VolumeInfo {
        volume_name,
        fs_name: fs_name_str,
        total_bytes: total,
        free_bytes: total_free,
    })
}
