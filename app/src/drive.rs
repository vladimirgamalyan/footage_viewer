//! Which storage bus a clip's drive sits on, for the tester's log.
//!
//! See ADR-0016. A path alone does not say what it is stored on: the two logs
//! that motivated this both read `E:\Hong Kong Original\`, one from an internal
//! disk at ~150 MB/s and one from an external at ~40 MB/s, and only the tester's
//! chat message told the two apart. `GetDriveTypeW` would not have helped —
//! a USB hard disk reports `DRIVE_FIXED`, exactly like an internal one, because
//! the removable bit describes removable *media* (a card reader), not a
//! removable *drive*. The bus type is the field that separates them.

use std::path::{Component, Path, Prefix};

/// The drive letter `path` starts from, uppercase. `None` for a UNC path, a
/// relative path, or any non-Windows path shape.
pub fn letter_of(path: &Path) -> Option<char> {
    match path.components().next()? {
        Component::Prefix(p) => match p.kind() {
            Prefix::Disk(d) | Prefix::VerbatimDisk(d) => Some(d as char),
            _ => None,
        },
        _ => None,
    }
}

/// The storage bus behind drive `letter` — `"USB"`, `"SATA"`, `"NVMe"` — or
/// `None` when it cannot be determined: a volume that will not open, or a
/// platform without the query. A missing answer is never worth failing over;
/// the caller just logs nothing.
///
/// The `BusType*` constants matched below are Windows API names, which the
/// upper-case-globals lint reads as bindings that would shadow rather than
/// match. They are consts and do match; the lint is off for this function only.
#[cfg(windows)]
#[allow(non_upper_case_globals)]
pub fn bus_of(letter: char) -> Option<String> {
    use std::ffi::OsStr;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        BusType1394, BusTypeAta, BusTypeNvme, BusTypeRAID, BusTypeSas, BusTypeSata, BusTypeScsi,
        BusTypeSd, BusTypeUsb, BusTypeVirtual, CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Ioctl::{
        PropertyStandardQuery, StorageDeviceProperty, IOCTL_STORAGE_QUERY_PROPERTY,
        STORAGE_DEVICE_DESCRIPTOR, STORAGE_PROPERTY_QUERY,
    };
    use windows_sys::Win32::System::IO::DeviceIoControl;

    // The volume device (`\\.\E:`), not the file system root (`E:\`): this form
    // takes no trailing separator.
    let device: Vec<u16> = OsStr::new(&format!(r"\\.\{letter}:"))
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    // Access 0 asks for device metadata and nothing else, which a standard user
    // is granted; any read access here would need administrator rights.
    let handle = unsafe {
        CreateFileW(
            device.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return None;
    }

    let query = STORAGE_PROPERTY_QUERY {
        PropertyId: StorageDeviceProperty,
        QueryType: PropertyStandardQuery,
        AdditionalParameters: [0],
    };
    // The descriptor is variable-length — it ends in vendor and product strings
    // this does not read — and the driver truncates it to whatever the buffer
    // holds. The struct is the fixed header, `BusType` sits inside it, and using
    // the struct rather than a byte array keeps the alignment its fields need.
    let mut desc = STORAGE_DEVICE_DESCRIPTOR::default();
    let mut returned = 0u32;
    let ok = unsafe {
        DeviceIoControl(
            handle,
            IOCTL_STORAGE_QUERY_PROPERTY,
            &query as *const _ as *const core::ffi::c_void,
            size_of::<STORAGE_PROPERTY_QUERY>() as u32,
            &mut desc as *mut _ as *mut core::ffi::c_void,
            size_of::<STORAGE_DEVICE_DESCRIPTOR>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }

    Some(match desc.BusType {
        BusTypeUsb => "USB".to_owned(),
        BusTypeSata => "SATA".to_owned(),
        BusTypeNvme => "NVMe".to_owned(),
        BusTypeAta => "ATA".to_owned(),
        BusTypeRAID => "RAID".to_owned(),
        BusTypeSas => "SAS".to_owned(),
        BusTypeScsi => "SCSI".to_owned(),
        BusType1394 => "FireWire".to_owned(),
        BusTypeSd => "SD".to_owned(),
        BusTypeVirtual => "virtual".to_owned(),
        // Reported as a number rather than dropped: an unnamed bus still tells
        // two runs apart, which is the whole point of the line.
        other => format!("bus {other}"),
    })
}

#[cfg(not(windows))]
pub fn bus_of(_letter: char) -> Option<String> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letter_of_a_windows_path() {
        assert_eq!(letter_of(Path::new(r"E:\clips\a.MP4")), Some('E'));
    }

    #[test]
    fn a_unc_path_has_no_drive_letter() {
        assert_eq!(letter_of(Path::new(r"\\server\share\a.MP4")), None);
    }

    #[test]
    fn a_relative_path_has_no_drive_letter() {
        assert_eq!(letter_of(Path::new("clips/a.MP4")), None);
    }
}
