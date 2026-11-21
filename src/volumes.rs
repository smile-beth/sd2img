//! Hold volume locks while imaging the underlying physical disk.
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_FUNCTION, ERROR_MORE_DATA, ERROR_NOT_SUPPORTED, ERROR_NO_MORE_FILES,
    GENERIC_READ, GENERIC_WRITE, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FindFirstVolumeW, FindNextVolumeW, FindVolumeClose,
    GetVolumeNameForVolumeMountPointW, GetVolumePathNameW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS, OPEN_EXISTING,
};
use windows::Win32::System::Ioctl::{
    DISK_EXTENT, FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, VOLUME_DISK_EXTENTS,
};
use windows::Win32::System::IO::DeviceIoControl;

pub(crate) fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

pub(crate) fn decoded(value: &[u16]) -> String {
    String::from_utf16_lossy(&value[..value.iter().position(|&c| c == 0).unwrap_or(value.len())])
}

pub(crate) struct OwnedHandle(pub(crate) HANDLE);
impl OwnedHandle {
    pub(crate) fn into_raw(self) -> HANDLE {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _operation = crate::watchdog::operation("close native handle");
        // Closing the handle also releases FSCTL_LOCK_VOLUME, including on errors.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

fn open_volume(name: &str, access: u32) -> io::Result<OwnedHandle> {
    let _operation = crate::watchdog::operation("open volume");
    let path = wide(name.trim_end_matches('\\'));
    unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            access,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            Default::default(),
            None,
        )
    }
    .map(OwnedHandle)
    .map_err(io::Error::from)
}

fn control(handle: HANDLE, code: u32) -> io::Result<()> {
    let _operation = crate::watchdog::operation("lock or dismount volume");
    let mut returned = 0;
    unsafe { DeviceIoControl(handle, code, None, 0, None, 0, Some(&mut returned), None) }
        .map_err(win_error)
}

pub(crate) fn win_error(error: windows::core::Error) -> io::Error {
    let code = error.code().0 as u32;
    if code & 0xffff_0000 == 0x8007_0000 {
        io::Error::from_raw_os_error((code & 0xffff) as i32)
    } else {
        io::Error::other(error)
    }
}

fn lock_and_dismount(name: &str, mut request: impl FnMut(u32) -> io::Result<()>) -> io::Result<()> {
    request(FSCTL_LOCK_VOLUME).map_err(|error| io::Error::other(message!("Cannot lock source volume {name}. Close applications using the card and try again. No imaging will proceed: {error}", "対象ボリューム {name} をロックできません。カードを使用中のアプリを閉じて再実行してください。救出は停止します: {error}")))?;
    request(FSCTL_DISMOUNT_VOLUME).map_err(|error| {
        io::Error::other(message!(
            "Cannot dismount locked source volume {name}; imaging is blocked: {error}",
            "ロック済みボリューム {name} をアンマウントできないため、救出を停止します: {error}"
        ))
    })
}

fn extent_disks(bytes: &[u8]) -> io::Result<Vec<u32>> {
    let invalid = || {
        io::Error::new(
            io::ErrorKind::InvalidData,
            tr!(
                "Invalid volume disk extents",
                "ボリュームのディスク範囲情報が不正です"
            ),
        )
    };
    let count =
        u32::from_ne_bytes(bytes.get(..4).ok_or_else(invalid)?.try_into().unwrap()) as usize;
    let start = offset_of!(VOLUME_DISK_EXTENTS, Extents);
    let length = count
        .checked_mul(size_of::<DISK_EXTENT>())
        .and_then(|n| start.checked_add(n))
        .ok_or_else(invalid)?;
    if count == 0 || length > bytes.len() {
        return Err(invalid());
    }
    Ok((0..count)
        .map(|index| {
            let offset =
                start + index * size_of::<DISK_EXTENT>() + offset_of!(DISK_EXTENT, DiskNumber);
            u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap())
        })
        .collect())
}

fn disk_numbers(handle: HANDLE) -> io::Result<Vec<u32>> {
    let _operation = crate::watchdog::operation("query volume disk extents");
    // u64 backing storage provides the alignment required by VOLUME_DISK_EXTENTS.
    let mut buffer = vec![0u64; 16];
    loop {
        let mut returned = 0;
        let capacity = buffer.len() * size_of::<u64>();
        let result = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_VOLUME_GET_VOLUME_DISK_EXTENTS,
                None,
                0,
                Some(buffer.as_mut_ptr().cast()),
                capacity as u32,
                Some(&mut returned),
                None,
            )
        };
        match result {
            Ok(()) => {
                if returned as usize > capacity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        tr!(
                            "Invalid extent buffer length",
                            "ディスク範囲情報のバッファー長が不正です"
                        ),
                    ));
                }
                // The initialized backing storage remains alive while parsing these bytes.
                let bytes = unsafe {
                    std::slice::from_raw_parts(buffer.as_ptr().cast(), returned as usize)
                };
                return extent_disks(bytes);
            }
            Err(error)
                if error.code() == windows::core::HRESULT::from_win32(ERROR_MORE_DATA.0)
                    && capacity < 1024 * 1024 =>
            {
                buffer.resize(buffer.len() * 2, 0)
            }
            Err(error) => return Err(win_error(error)),
        }
    }
}

struct VolumeSearch(HANDLE);

impl Drop for VolumeSearch {
    fn drop(&mut self) {
        unsafe {
            let _ = FindVolumeClose(self.0);
        }
    }
}

fn volume_names() -> io::Result<Vec<String>> {
    let _operation = crate::watchdog::operation("enumerate volumes");
    let mut buffer = [0u16; 1024];
    let search = VolumeSearch(unsafe { FindFirstVolumeW(&mut buffer) }.map_err(io::Error::from)?);
    let mut names = vec![decoded(&buffer)];
    loop {
        match unsafe { FindNextVolumeW(search.0, &mut buffer) } {
            Ok(()) => names.push(decoded(&buffer)),
            Err(error)
                if error.code() == windows::core::HRESULT::from_win32(ERROR_NO_MORE_FILES.0) =>
            {
                break
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(names)
}

pub fn disk_number(path: &str) -> io::Result<u32> {
    path.strip_prefix(r"\\.\PhysicalDrive")
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                tr!("Invalid physical disk path", "物理ディスクのパスが不正です"),
            )
        })
}

/// Reject output on the source before acquiring any locks or creating files.
pub fn check_destination(output: &str, source: u32) -> io::Result<()> {
    let _operation = crate::watchdog::operation("validate output destination");
    if path_disks(output)?.contains(&source) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            tr!(
                "The image and map must be stored on a different physical disk from the source.",
                "イメージ・マップ・ログは元ディスクと異なる物理ディスクに保存してください。"
            ),
        ));
    }
    Ok(())
}

pub(crate) fn path_disks(output: &str) -> io::Result<Vec<u32>> {
    // Resolve an existing file too, so a symlink cannot hide a source-disk destination.
    let path = Path::new(output);
    let existing = if path.exists() {
        path
    } else {
        path.parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
    };
    let canonical = existing.canonicalize()?;
    let path: Vec<u16> = canonical.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut mount = vec![0u16; 32768];
    let mut volume = [0u16; 1024];
    unsafe {
        GetVolumePathNameW(PCWSTR(path.as_ptr()), &mut mount)?;
        GetVolumeNameForVolumeMountPointW(PCWSTR(mount.as_ptr()), &mut volume)?;
    }
    let handle = open_volume(&decoded(&volume), 0)?;
    disk_numbers(handle.0)
}

#[derive(Default)]
pub struct VolumeLocks {
    held: Vec<(String, OwnedHandle)>,
}

impl VolumeLocks {
    pub(crate) fn release(&mut self) {
        self.held.clear();
    }
    pub fn ensure(&mut self, source: u32) -> io::Result<()> {
        let _operation = crate::watchdog::operation("protect source volumes");
        // Removed devices invalidate old handles. Keep live locks without unlocking
        // and relocking on every read error; re-enumeration finds reinserted volumes.
        self.held.retain(|(_, handle)| {
            disk_numbers(handle.0).is_ok_and(|disks| disks.iter().all(|&disk| disk == source))
        });
        for name in volume_names()? {
            if self.held.iter().any(|(held, _)| held == &name) {
                continue;
            }
            let probe = open_volume(&name, 0).map_err(|error| {
                io::Error::other(message!(
                    "Cannot inspect volume {name}; imaging is blocked: {error}",
                    "ボリューム {name} を確認できないため、救出を停止します: {error}"
                ))
            })?;
            let disks = match disk_numbers(probe.0) {
                Ok(disks) => disks,
                // Optical/non-disk volumes do not implement this disk-specific query.
                Err(error)
                    if matches!(error.raw_os_error(), Some(code)
                    if code == ERROR_INVALID_FUNCTION.0 as i32 || code == ERROR_NOT_SUPPORTED.0 as i32) =>
                {
                    continue
                }
                Err(error) => {
                    return Err(io::Error::other(message!(
                        "Cannot identify the disk for volume {name}; imaging is blocked: {error}",
                        "ボリューム {name} のディスクを特定できないため、救出を停止します: {error}"
                    )))
                }
            };
            if !disks.contains(&source) {
                continue;
            }
            if disks.iter().any(|&disk| disk != source) {
                return Err(io::Error::other(
                    "Source volume spans multiple disks; refusing to lock it.",
                ));
            }
            drop(probe);
            let handle = open_volume(&name, GENERIC_READ.0 | GENERIC_WRITE.0)?;
            lock_and_dismount(&name, |code| control(handle.0, code))?;
            success!(
                "{}",
                message!(
                    "Locked and dismounted source volume: {name}",
                    "対象ボリュームをロック・アンマウントしました: {name}"
                )
            );
            self.held.push((name, handle));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_lock_never_forces_a_dismount() {
        let mut calls = Vec::new();
        assert!(lock_and_dismount("test volume", |code| {
            calls.push(code);
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Volume is in use",
            ))
        })
        .is_err());
        assert_eq!(calls, [FSCTL_LOCK_VOLUME]);
        calls.clear();
        lock_and_dismount("test volume", |code| {
            calls.push(code);
            Ok(())
        })
        .unwrap();
        assert_eq!(calls, [FSCTL_LOCK_VOLUME, FSCTL_DISMOUNT_VOLUME]);
        assert!(lock_and_dismount("test volume", |code| {
            if code == FSCTL_DISMOUNT_VOLUME {
                Err(io::Error::other("Dismount failed"))
            } else {
                Ok(())
            }
        })
        .is_err());
    }

    #[test]
    fn windows_errors_preserve_win32_error_codes() {
        let error =
            windows::core::Error::from(windows::core::HRESULT::from_win32(ERROR_NOT_SUPPORTED.0));
        assert_eq!(
            win_error(error).raw_os_error(),
            Some(ERROR_NOT_SUPPORTED.0 as i32)
        );
    }

    #[test]
    fn parses_all_extents_and_rejects_truncated_or_empty_data() {
        let start = offset_of!(VOLUME_DISK_EXTENTS, Extents);
        let mut data = vec![0; start + 2 * size_of::<DISK_EXTENT>()];
        data[..4].copy_from_slice(&2u32.to_ne_bytes());
        for (index, disk) in [3u32, 7].iter().enumerate() {
            let offset =
                start + index * size_of::<DISK_EXTENT>() + offset_of!(DISK_EXTENT, DiskNumber);
            data[offset..offset + 4].copy_from_slice(&disk.to_ne_bytes());
        }
        assert_eq!(extent_disks(&data).unwrap(), [3, 7]);
        assert!(extent_disks(&data[..data.len() - 1]).is_err());
        assert!(extent_disks(&[]).is_err());
        data[..4].copy_from_slice(&0u32.to_ne_bytes());
        assert!(extent_disks(&data).is_err());
    }

    #[test]
    fn physical_disk_path_is_strict() {
        assert_eq!(disk_number(r"\\.\PhysicalDrive12").unwrap(), 12);
        assert!(disk_number("E:").is_err());
        assert!(disk_number(r"\\.\PhysicalDrive-1").is_err());
    }
}
