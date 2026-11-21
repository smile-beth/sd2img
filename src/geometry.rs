use crate::{
    sector::SectorSize,
    volumes::{wide, win_error, OwnedHandle},
};
use std::{io, mem::size_of};
use windows::core::PCWSTR;
use windows::Win32::Foundation::GENERIC_READ;

const QUERY_ACCESS: u32 = GENERIC_READ.0;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::{
    Ioctl::{
        PropertyStandardQuery, StorageAccessAlignmentProperty, DISK_GEOMETRY_EX,
        IOCTL_DISK_GET_DRIVE_GEOMETRY_EX, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    },
    IO::DeviceIoControl,
};

pub(crate) fn inspect(path: &str) -> io::Result<(SectorSize, u64)> {
    let _operation = crate::watchdog::operation("query logical sector size");
    let display_path = path;
    let path = wide(path);
    // Metadata IOCTLs use a synchronous handle, separate from overlapped data I/O.
    let handle = OwnedHandle(
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                QUERY_ACCESS,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .map_err(|error| query_error("CreateFileW(metadata)", display_path, error))?,
    );
    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageAccessAlignmentProperty,
        QueryType: PropertyStandardQuery,
        ..Default::default()
    };
    let mut alignment = STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR::default();
    let mut returned = 0;
    let result = unsafe {
        DeviceIoControl(
            handle.0,
            IOCTL_STORAGE_QUERY_PROPERTY,
            Some((&query as *const STORAGE_PROPERTY_QUERY).cast()),
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            Some((&mut alignment as *mut STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR).cast()),
            size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>() as u32,
            Some(&mut returned),
            None,
        )
    };
    let logical = if result.is_ok()
        && returned as usize >= size_of::<STORAGE_ACCESS_ALIGNMENT_DESCRIPTOR>()
    {
        SectorSize::new(alignment.BytesPerLogicalSector as usize).ok()
    } else {
        None
    };
    let sector = if let Some(sector) = logical {
        sector
    } else {
        let mut buffer = vec![0u64; 512];
        unsafe {
            DeviceIoControl(
                handle.0,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                None,
                0,
                Some(buffer.as_mut_ptr().cast()),
                (buffer.len() * 8) as u32,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|error| query_error("IOCTL_DISK_GET_DRIVE_GEOMETRY_EX", display_path, error))?;
        if (returned as usize) < size_of::<windows::Win32::System::Ioctl::DISK_GEOMETRY>() {
            return Err(io::Error::other(tr!(
                "Truncated disk geometry response",
                "ディスク構成の応答が不足しています"
            )));
        }
        let geometry = unsafe { &*buffer.as_ptr().cast::<DISK_GEOMETRY_EX>() };
        SectorSize::new(geometry.Geometry.BytesPerSector as usize)?
    };
    let mut length = windows::Win32::System::Ioctl::GET_LENGTH_INFORMATION::default();
    unsafe {
        DeviceIoControl(
            handle.0,
            windows::Win32::System::Ioctl::IOCTL_DISK_GET_LENGTH_INFO,
            None,
            0,
            Some(
                (&mut length as *mut windows::Win32::System::Ioctl::GET_LENGTH_INFORMATION).cast(),
            ),
            size_of::<windows::Win32::System::Ioctl::GET_LENGTH_INFORMATION>() as u32,
            Some(&mut returned),
            None,
        )
    }
    .map_err(|error| query_error("IOCTL_DISK_GET_LENGTH_INFO", display_path, error))?;
    if length.Length <= 0 || !(length.Length as u64).is_multiple_of(sector.bytes() as u64) {
        return Err(io::Error::other(tr!(
            "Invalid sector-aligned disk size",
            "ディスク容量がセクタ境界に一致しません"
        )));
    }
    Ok((sector, length.Length as u64))
}

fn query_error(api: &str, path: &str, error: windows::core::Error) -> io::Error {
    let error = win_error(error);
    io::Error::new(
        error.kind(),
        message!(
            "{api} failed for {path}: {error}",
            "{api} に失敗しました（対象: {path}）: {error}"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn length_query_handle_grants_the_access_required_by_the_ioctl() {
        use windows::Win32::System::Ioctl::IOCTL_DISK_GET_LENGTH_INFO;
        // CTL_CODE bits 14..15 encode FILE_READ_ACCESS / FILE_WRITE_ACCESS.
        assert_eq!((IOCTL_DISK_GET_LENGTH_INFO >> 14) & 3, 1);
        assert_ne!(QUERY_ACCESS & GENERIC_READ.0, 0);
        assert_eq!(
            QUERY_ACCESS & windows::Win32::Foundation::GENERIC_WRITE.0,
            0
        );
    }
}
