//! Native disk discovery and UAC elevation; no shell subprocesses.
use crate::volumes::{decoded, path_disks, wide, win_error, OwnedHandle};
use std::ffi::OsStr;
use std::io;
use std::mem::{offset_of, size_of};
use std::os::windows::ffi::OsStrExt;
use windows::core::PCWSTR;
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{
    ERROR_INSUFFICIENT_BUFFER, ERROR_MORE_DATA, ERROR_NO_MORE_ITEMS, HANDLE, WAIT_OBJECT_0,
};
use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
use windows::Win32::Storage::FileSystem::{
    BusTypeMmc, BusTypeSd, BusTypeUsb, CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, PARTITION_SYSTEM_GUID,
};
use windows::Win32::System::Ioctl::*;
use windows::Win32::System::SystemInformation::GetWindowsDirectoryW;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetExitCodeProcess, OpenProcessToken, WaitForSingleObject, INFINITE,
};
use windows::Win32::System::IO::DeviceIoControl;
use windows::Win32::UI::Shell::{
    ShellExecuteExW, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

struct DeviceList(HDEVINFO);
impl Drop for DeviceList {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

fn interfaces() -> io::Result<Vec<(Vec<u16>, u32)>> {
    let _operation = crate::watchdog::operation("enumerate disk interfaces");
    let devices = DeviceList(
        unsafe {
            SetupDiGetClassDevsW(
                Some(&GUID_DEVINTERFACE_DISK),
                PCWSTR::null(),
                None,
                DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
            )
        }
        .map_err(win_error)?,
    );
    let mut paths = Vec::new();
    for index in 0.. {
        let mut interface = SP_DEVICE_INTERFACE_DATA {
            cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
            ..Default::default()
        };
        match unsafe {
            SetupDiEnumDeviceInterfaces(
                devices.0,
                None,
                &GUID_DEVINTERFACE_DISK,
                index,
                &mut interface,
            )
        } {
            Ok(()) => {}
            Err(error)
                if error.code() == windows::core::HRESULT::from_win32(ERROR_NO_MORE_ITEMS.0) =>
            {
                break
            }
            Err(error) => return Err(win_error(error)),
        }
        let mut required = 0;
        let result = unsafe {
            SetupDiGetDeviceInterfaceDetailW(
                devices.0,
                &interface,
                None,
                0,
                Some(&mut required),
                None,
            )
        };
        if let Err(error) = result {
            if error.code() != windows::core::HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0) {
                return Err(win_error(error));
            }
        }
        if required < size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32
            || required > 1024 * 1024
        {
            return Err(io::Error::other(tr!(
                "Invalid disk interface path size",
                "ディスクインターフェースのパス長が不正です"
            )));
        }
        let mut buffer = vec![0u64; (required as usize).div_ceil(8)];
        let detail = buffer
            .as_mut_ptr()
            .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
        let mut devinfo = SP_DEVINFO_DATA {
            cbSize: size_of::<SP_DEVINFO_DATA>() as u32,
            ..Default::default()
        };
        unsafe {
            (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            SetupDiGetDeviceInterfaceDetailW(
                devices.0,
                &interface,
                Some(detail),
                required,
                None,
                Some(&mut devinfo),
            )
            .map_err(win_error)?;
            let count =
                (required as usize - offset_of!(SP_DEVICE_INTERFACE_DETAIL_DATA_W, DevicePath)) / 2;
            let path = std::slice::from_raw_parts(
                std::ptr::addr_of!((*detail).DevicePath).cast::<u16>(),
                count,
            );
            paths.push((path.to_vec(), devinfo.DevInst));
        }
    }
    Ok(paths)
}

fn query(
    handle: HANDLE,
    code: u32,
    input: Option<(*const core::ffi::c_void, u32)>,
) -> io::Result<Vec<u8>> {
    let _operation = crate::watchdog::operation("query disk metadata");
    let mut buffer = vec![0u64; 128];
    loop {
        let capacity = buffer.len() * 8;
        let mut returned = 0;
        let result = unsafe {
            DeviceIoControl(
                handle,
                code,
                input.map(|p| p.0),
                input.map_or(0, |p| p.1),
                Some(buffer.as_mut_ptr().cast()),
                capacity as u32,
                Some(&mut returned),
                None,
            )
        };
        match result {
            Ok(()) if returned as usize <= capacity => {
                return Ok(unsafe {
                    std::slice::from_raw_parts(buffer.as_ptr().cast::<u8>(), returned as usize)
                }
                .to_vec());
            }
            Ok(()) => {
                return Err(io::Error::other(tr!(
                    "Invalid disk query response size",
                    "ディスク照会の応答サイズが不正です"
                )))
            }
            Err(error)
                if (error.code() == windows::core::HRESULT::from_win32(ERROR_MORE_DATA.0)
                    || error.code()
                        == windows::core::HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0))
                    && capacity < 1024 * 1024 =>
            {
                buffer.resize(buffer.len() * 2, 0)
            }
            Err(error) => return Err(win_error(error)),
        }
    }
}

fn read_struct<T: Copy>(bytes: &[u8], offset: usize) -> io::Result<T> {
    let end = offset.checked_add(size_of::<T>()).ok_or_else(|| {
        io::Error::other(tr!(
            "Invalid disk response offset",
            "ディスク応答の位置が不正です"
        ))
    })?;
    let data = bytes.get(offset..end).ok_or_else(|| {
        io::Error::other(tr!(
            "Truncated disk response",
            "ディスク応答が不足しています"
        ))
    })?;
    // Windows output structures contain integer fields/unions, and may be unaligned here.
    Ok(unsafe { data.as_ptr().cast::<T>().read_unaligned() })
}

fn has_boot_partition(bytes: &[u8]) -> io::Result<bool> {
    let count = read_struct::<u32>(
        bytes,
        offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionCount),
    )? as usize;
    let start = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
    if count > bytes.len().saturating_sub(start) / size_of::<PARTITION_INFORMATION_EX>() {
        return Err(io::Error::other(tr!(
            "Truncated partition layout",
            "パーティション構成のデータが不足しています"
        )));
    }
    for index in 0..count {
        let entry: PARTITION_INFORMATION_EX =
            read_struct(bytes, start + index * size_of::<PARTITION_INFORMATION_EX>())?;
        // Read only the union member selected by PartitionStyle.
        if unsafe {
            (entry.PartitionStyle == PARTITION_STYLE_GPT
                && entry.Anonymous.Gpt.PartitionType == PARTITION_SYSTEM_GUID)
                || (entry.PartitionStyle == PARTITION_STYLE_MBR
                    && entry.Anonymous.Mbr.BootIndicator.0 != 0)
        } {
            return Ok(true);
        }
    }
    Ok(false)
}

pub fn auto_device_path(verbose: bool) -> io::Result<String> {
    let _operation = crate::watchdog::operation("automatic disk discovery");
    let mut directory = vec![0u16; 32768];
    let length = unsafe { GetWindowsDirectoryW(Some(&mut directory)) } as usize;
    if length == 0 || length >= directory.len() {
        return Err(io::Error::other(tr!(
            "Cannot identify the Windows system disk",
            "Windowsのシステムディスクを特定できません"
        )));
    }
    let system_disks = path_disks(&decoded(&directory))?;
    let mut candidates = Vec::new();
    for (path, _) in interfaces()? {
        if crate::control::stopping() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                tr!("Disk discovery stopped", "ディスクの検出を中断しました"),
            ));
        }
        let handle = match unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        } {
            Ok(handle) => OwnedHandle(handle),
            Err(_) => continue,
        };
        let property = STORAGE_PROPERTY_QUERY {
            PropertyId: StorageDeviceProperty,
            QueryType: PropertyStandardQuery,
            ..Default::default()
        };
        let descriptor = match query(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((
                &property as *const STORAGE_PROPERTY_QUERY as *const _,
                size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            )),
        ) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        let descriptor: STORAGE_DEVICE_DESCRIPTOR = match read_struct(&descriptor, 0) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if ![BusTypeSd, BusTypeMmc, BusTypeUsb].contains(&descriptor.BusType) {
            continue;
        }
        let number: STORAGE_DEVICE_NUMBER =
            match query(handle.0, IOCTL_STORAGE_GET_DEVICE_NUMBER, None)
                .and_then(|bytes| read_struct(&bytes, 0))
            {
                Ok(value) => value,
                Err(_) => continue,
            };
        if system_disks.contains(&number.DeviceNumber) {
            continue;
        }
        // Conservatively exclude EFI/active boot partitions. If the layout cannot
        // be verified, require explicit disk selection rather than guessing.
        match query(handle.0, IOCTL_DISK_GET_DRIVE_LAYOUT_EX, None)
            .and_then(|bytes| has_boot_partition(&bytes))
        {
            Ok(false) => candidates.push(number.DeviceNumber),
            _ => continue,
        }
    }
    candidates.sort_unstable();
    candidates.dedup();
    match candidates.as_slice() {
        [number] => {
            if verbose {
                info!(
                    "{}",
                    message!(
                        "Automatically selected physical disk {number}",
                        "物理ディスク {number} を自動選択しました"
                    )
                );
            }
            Ok(format!(r"\\.\PhysicalDrive{number}"))
        }
        [] => Err(io::Error::new(
            io::ErrorKind::NotFound,
            tr!(
                "No eligible SD/MMC/USB disks found. Specify the source with -d.",
                "対象のSD/MMC/USBディスクがありません。-dで指定してください。"
            ),
        )),
        _ => {
            if verbose {
                warning_stderr!(
                    "{}",
                    message!(
                        "Multiple candidate disks found. Run again with -d to select one:",
                        "候補が複数あります。-dで指定して再実行してください:"
                    )
                );
                for number in candidates {
                    warning_stderr!("  -d {number}");
                }
            }
            Err(io::Error::other(tr!(
                "Cannot uniquely identify the source disk",
                "対象ディスクを一意に特定できません"
            )))
        }
    }
}

fn instance_id(devinst: u32) -> io::Result<String> {
    let mut length = 0;
    let code = unsafe { CM_Get_Device_ID_Size(&mut length, devinst, 0) };
    if code != CR_SUCCESS || length > 32768 {
        return Err(io::Error::other(message!(
            "Cannot query device instance ID: CM code {}",
            "デバイス識別子を取得できません: CMコード {}",
            code.0
        )));
    }
    let mut id = vec![0u16; length as usize + 1];
    let code = unsafe { CM_Get_Device_IDW(devinst, &mut id, 0) };
    if code != CR_SUCCESS {
        return Err(io::Error::other(message!(
            "Cannot read device instance ID: CM code {}",
            "デバイス識別子を読み取れません: CMコード {}",
            code.0
        )));
    }
    Ok(decoded(&id))
}

fn interface_disk(path: &[u16]) -> io::Result<u32> {
    let handle = OwnedHandle(
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .map_err(win_error)?,
    );
    let number: STORAGE_DEVICE_NUMBER =
        read_struct(&query(handle.0, IOCTL_STORAGE_GET_DEVICE_NUMBER, None)?, 0)?;
    Ok(number.DeviceNumber)
}

pub(crate) fn device_instance_for_disk(number: u32) -> io::Result<String> {
    let _operation = crate::watchdog::operation("identify reset target");
    for (path, devinst) in interfaces()? {
        if interface_disk(&path).ok() == Some(number) {
            return instance_id(devinst);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        tr!(
            "Source disk device instance was not found",
            "対象ディスクのデバイス識別子が見つかりません"
        ),
    ))
}

pub(crate) fn disk_for_instance(id: &str) -> io::Result<String> {
    let _operation = crate::watchdog::operation("rediscover reset target");
    for (path, devinst) in interfaces()? {
        if instance_id(devinst).is_ok_and(|candidate| candidate.eq_ignore_ascii_case(id)) {
            return Ok(format!(r"\\.\PhysicalDrive{}", interface_disk(&path)?));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        tr!(
            "Source disk has not reappeared after reset",
            "リセット後に対象ディスクが再検出されていません"
        ),
    ))
}

fn quote_argument(argument: &OsStr) -> Vec<u16> {
    let mut result = vec![b'"' as u16];
    let mut slashes = 0;
    for unit in argument.encode_wide() {
        if unit == b'\\' as u16 {
            slashes += 1;
            continue;
        }
        result.extend(std::iter::repeat_n(
            b'\\' as u16,
            if unit == b'"' as u16 {
                2 * slashes + 1
            } else {
                slashes
            },
        ));
        result.push(unit);
        slashes = 0;
    }
    result.extend(std::iter::repeat_n(b'\\' as u16, 2 * slashes));
    result.push(b'"' as u16);
    result
}

pub fn ensure_elevated() -> io::Result<()> {
    let mut token = HANDLE::default();
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) }.map_err(win_error)?;
    let token = OwnedHandle(token);
    let mut elevation = TOKEN_ELEVATION::default();
    let mut returned = 0;
    unsafe {
        GetTokenInformation(
            token.0,
            TokenElevation,
            Some((&mut elevation as *mut TOKEN_ELEVATION).cast()),
            size_of::<TOKEN_ELEVATION>() as u32,
            &mut returned,
        )
    }
    .map_err(win_error)?;
    if elevation.TokenIsElevated != 0 {
        return Ok(());
    }
    let verb = wide("runas");
    let executable: Vec<u16> = std::env::current_exe()?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let directory: Vec<u16> = std::env::current_dir()?
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let mut arguments = Vec::new();
    for argument in std::env::args_os().skip(1) {
        if !arguments.is_empty() {
            arguments.push(b' ' as u16);
        }
        arguments.extend(quote_argument(&argument));
    }
    arguments.push(0);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(executable.as_ptr()),
        lpParameters: PCWSTR(arguments.as_ptr()),
        lpDirectory: PCWSTR(directory.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    info!(
        "{}",
        message!(
            "sd2img is requesting administrator privileges. Approve the UAC prompt to continue.",
            "sd2imgが管理者権限を要求します。UACを許可してください。"
        )
    );
    unsafe { ShellExecuteExW(&mut info) }.map_err(win_error)?;
    if info.hProcess.is_invalid() {
        return Err(io::Error::other(tr!(
            "No elevated process handle returned",
            "昇格したプロセスのハンドルを取得できませんでした"
        )));
    }
    let process = OwnedHandle(info.hProcess);
    if unsafe { WaitForSingleObject(process.0, INFINITE) } != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error());
    }
    let mut code = 1;
    unsafe { GetExitCodeProcess(process.0, &mut code) }.map_err(win_error)?;
    std::process::exit(code as i32);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::UI::Shell::CommandLineToArgvW;

    #[test]
    fn elevation_arguments_round_trip_through_windows_parser() {
        let values = [
            "",
            "card image.img",
            "E:\\",
            "quote\"inside",
            "日本語\\",
            "x\\\\\"y",
            "$();`&",
        ];
        let mut command = wide("sd2img.exe");
        command.pop();
        for value in values {
            command.push(32);
            command.extend(quote_argument(OsStr::new(value)));
        }
        command.push(0);
        let mut count = 0;
        let args = unsafe { CommandLineToArgvW(PCWSTR(command.as_ptr()), &mut count) };
        assert!(!args.is_null());
        assert_eq!(count as usize, values.len() + 1);
        for (index, expected) in values.iter().enumerate() {
            let actual = unsafe { (*args.add(index + 1)).to_string().unwrap() };
            assert_eq!(actual, *expected);
        }
        unsafe {
            let _ = LocalFree(HLOCAL(args.cast()));
        }
    }

    #[test]
    fn boot_partition_detection_checks_style_and_bounds() {
        let start = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionEntry);
        let mut data = vec![0u8; start + size_of::<PARTITION_INFORMATION_EX>()];
        let count = offset_of!(DRIVE_LAYOUT_INFORMATION_EX, PartitionCount);
        data[count..count + 4].copy_from_slice(&1u32.to_ne_bytes());
        let mut entry = PARTITION_INFORMATION_EX {
            PartitionStyle: PARTITION_STYLE_GPT,
            ..Default::default()
        };
        entry.Anonymous.Gpt.PartitionType = PARTITION_SYSTEM_GUID;
        unsafe {
            data.as_mut_ptr()
                .add(start)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(entry);
        }
        assert!(has_boot_partition(&data).unwrap());
        assert!(has_boot_partition(&data[..start]).is_err());
        entry.PartitionStyle = PARTITION_STYLE_MBR;
        entry.Anonymous.Mbr = PARTITION_INFORMATION_MBR::default();
        unsafe {
            data.as_mut_ptr()
                .add(start)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(entry);
        }
        assert!(!has_boot_partition(&data).unwrap());
        entry.Anonymous.Mbr.BootIndicator.0 = 1;
        unsafe {
            data.as_mut_ptr()
                .add(start)
                .cast::<PARTITION_INFORMATION_EX>()
                .write_unaligned(entry);
        }
        assert!(has_boot_partition(&data).unwrap());
    }
}
