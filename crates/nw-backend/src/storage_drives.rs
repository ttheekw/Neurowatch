//! Drives a recording location can be chosen from, for remote configuration.
//!
//! The web portal never names a path: it picks one of the mounted drives this
//! scan reports, and recordings go to `<drive>/nw-storage`. Only real disk
//! partitions are listed (no pseudo, network, loop or boot filesystems).

use std::path::{Path, PathBuf};

use serde::Serialize;

/// Drives at least this large are offered by default; smaller ones only when
/// the operator asks to see every drive.
pub const RECOMMENDED_MIN_BYTES: u64 = 128_000_000_000;
/// Folder created at the top of the chosen drive for recordings.
pub const STORAGE_FOLDER: &str = "nw-storage";

#[cfg(any(not(windows), test))]
const DISK_FILESYSTEMS: &[&str] = &[
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "jfs", "vfat", "exfat", "ntfs", "ntfs3",
    "fuseblk",
];
#[cfg(any(not(windows), test))]
const SYSTEM_MOUNTS: &[&str] = &[
    "/boot",
    "/efi",
    "/snap",
    "/var/snap",
    "/proc",
    "/sys",
    "/dev",
];

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Drive {
    pub mount_point: String,
    pub device: String,
    pub filesystem: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// At least [`RECOMMENDED_MIN_BYTES`].
    pub recommended: bool,
    /// The current recording location is on this drive.
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Mount {
    pub device: String,
    pub mount_point: String,
    pub filesystem: String,
    pub read_only: bool,
}

/// `/proc/mounts` escapes spaces, tabs, newlines and backslashes as octal.
#[cfg(any(not(windows), test))]
fn unescape(field: &str) -> String {
    let mut out = String::with_capacity(field.len());
    let mut rest = field;
    while let Some(index) = rest.find('\\') {
        out.push_str(&rest[..index]);
        let code = rest.get(index + 1..index + 4);
        match code.and_then(|digits| u8::from_str_radix(digits, 8).ok()) {
            Some(byte) => {
                out.push(byte as char);
                rest = &rest[index + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[index + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(any(not(windows), test))]
pub(crate) fn parse_mounts(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let device = unescape(fields.next()?);
            let mount_point = unescape(fields.next()?);
            let filesystem = fields.next()?.to_owned();
            let read_only = fields.next()?.split(',').any(|option| option == "ro");
            Some(Mount {
                device,
                mount_point,
                filesystem,
                read_only,
            })
        })
        .collect()
}

#[cfg(any(not(windows), test))]
fn is_system_mount(mount_point: &str) -> bool {
    SYSTEM_MOUNTS
        .iter()
        .any(|prefix| mount_point == *prefix || mount_point.starts_with(&format!("{prefix}/")))
}

/// Writable disk partitions, one per device (a device mounted twice, e.g. by
/// a bind mount, is listed at its shortest mount point).
#[cfg(any(not(windows), test))]
pub(crate) fn disk_mounts(mounts: Vec<Mount>) -> Vec<Mount> {
    let mut kept: Vec<Mount> = Vec::new();
    for mount in mounts {
        if mount.read_only
            || !mount.device.starts_with("/dev/")
            || mount.device.starts_with("/dev/loop")
            || !DISK_FILESYSTEMS.contains(&mount.filesystem.as_str())
            || is_system_mount(&mount.mount_point)
        {
            continue;
        }
        match kept.iter_mut().find(|seen| seen.device == mount.device) {
            Some(seen) if mount.mount_point.len() < seen.mount_point.len() => *seen = mount,
            Some(_) => {}
            None => kept.push(mount),
        }
    }
    kept.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    kept
}

/// `(total, available)` bytes of the filesystem at `path`.
#[cfg(unix)]
// The statvfs field types are 32-bit on some platforms and 64-bit on others.
#[allow(clippy::useless_conversion)]
pub(crate) fn space(path: &str) -> Option<(u64, u64)> {
    let path = std::ffi::CString::new(path).ok()?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is a valid NUL-terminated string and `stat` is a valid
    // out-pointer that statvfs fully initializes when it returns 0.
    let stat = unsafe {
        if libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) != 0 {
            return None;
        }
        stat.assume_init()
    };
    let fragment = u64::from(stat.f_frsize);
    Some((
        u64::from(stat.f_blocks).saturating_mul(fragment),
        u64::from(stat.f_bavail).saturating_mul(fragment),
    ))
}

/// `(total, available)` bytes of the volume holding `path`.
#[cfg(windows)]
pub(crate) fn space(path: &str) -> Option<(u64, u64)> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let _error_mode = VolumeErrorMode::new()?;
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let mut available = 0_u64;
    let mut total = 0_u64;
    // SAFETY: `wide` is NUL-terminated and the out-pointers are valid u64s;
    // the free-bytes total is optional and may be null.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            &mut total,
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some((total, available))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn space(_path: &str) -> Option<(u64, u64)> {
    None
}

#[cfg(not(windows))]
fn mounted_disks() -> Result<Vec<Mount>, String> {
    std::fs::read_to_string("/proc/mounts")
        .map(|text| disk_mounts(parse_mounts(&text)))
        .map_err(|_| "listing drives is only supported on Linux".to_owned())
}

/// Win32 drive-type values are part of the API ABI; kept here so filtering is
/// also testable on Linux without linking Windows APIs.
#[cfg(any(windows, test))]
fn windows_disks(volumes: Vec<(u32, Mount)>) -> Vec<Mount> {
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_FIXED: u32 = 3;
    let mut mounts: Vec<_> = volumes
        .into_iter()
        .filter(|(kind, mount)| matches!(*kind, DRIVE_FIXED | DRIVE_REMOVABLE) && !mount.read_only)
        .map(|(_, mount)| mount)
        .collect();
    mounts.sort_by(|a, b| a.mount_point.cmp(&b.mount_point));
    mounts
}

/// Empty removable media must fail without opening a Windows dialog in the
/// background service. The error mode belongs to this thread, not the process.
#[cfg(windows)]
struct VolumeErrorMode(u32);

#[cfg(windows)]
impl VolumeErrorMode {
    fn new() -> Option<Self> {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            GetThreadErrorMode, SetThreadErrorMode, SEM_FAILCRITICALERRORS, SEM_NOOPENFILEERRORBOX,
        };

        let mut previous = 0;
        // SAFETY: these calls affect only the current thread; previous is a
        // writable output. Preserve existing flags and restore them on Drop.
        let ok = unsafe {
            SetThreadErrorMode(
                GetThreadErrorMode() | SEM_FAILCRITICALERRORS | SEM_NOOPENFILEERRORBOX,
                &mut previous,
            )
        };
        (ok != 0).then(|| Self(previous))
    }
}

#[cfg(windows)]
impl Drop for VolumeErrorMode {
    fn drop(&mut self) {
        // SAFETY: restoring the saved error mode on the same thread. The
        // optional previous-mode output is unused.
        unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::SetThreadErrorMode(
                self.0,
                std::ptr::null_mut(),
            );
        }
    }
}

#[cfg(windows)]
fn mounted_disks() -> Result<Vec<Mount>, String> {
    use windows_sys::Win32::Storage::FileSystem::{
        GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
    };

    let _error_mode = VolumeErrorMode::new()
        .ok_or_else(|| "cannot safely check removable drives without Windows dialogs".to_owned())?;
    // FILE_READ_ONLY_VOLUME from GetVolumeInformationW's filesystem flags.
    const FILE_READ_ONLY_VOLUME: u32 = 0x0008_0000;
    // SAFETY: a null buffer with length zero queries the required buffer size.
    let required = unsafe { GetLogicalDriveStringsW(0, std::ptr::null_mut()) };
    if required == 0 {
        return Err(format!(
            "cannot list drives: {}",
            std::io::Error::last_os_error()
        ));
    }
    let mut buffer = vec![0_u16; required as usize + 1];
    // SAFETY: buffer is writable for the advertised number of UTF-16 units.
    let written = unsafe { GetLogicalDriveStringsW(buffer.len() as u32, buffer.as_mut_ptr()) };
    if written == 0 || written as usize >= buffer.len() {
        return Err("cannot list drives: the drive list changed; try again".to_owned());
    }
    let mut volumes = Vec::new();
    for root in buffer[..written as usize].split(|unit| *unit == 0) {
        if root.is_empty() {
            continue;
        }
        let wide: Vec<u16> = root.iter().copied().chain(std::iter::once(0)).collect();
        // SAFETY: wide holds a NUL-terminated drive root.
        let kind = unsafe { GetDriveTypeW(wide.as_ptr()) };
        if !matches!(kind, 2 | 3) {
            continue; // Never probe CD-ROMs, network shares or RAM disks.
        }
        let mut filesystem = [0_u16; 261];
        let mut flags = 0_u32;
        // SAFETY: input is NUL-terminated; output buffers and their sizes are
        // valid. Unneeded volume label, serial and component length are null.
        let ok = unsafe {
            GetVolumeInformationW(
                wide.as_ptr(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut flags,
                filesystem.as_mut_ptr(),
                filesystem.len() as u32,
            )
        };
        if ok == 0 {
            continue; // Includes removable drives with no media inserted.
        }
        let end = filesystem
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(filesystem.len());
        let mount_point = String::from_utf16_lossy(root);
        volumes.push((
            kind,
            Mount {
                device: mount_point.trim_end_matches('\\').to_owned(),
                mount_point,
                filesystem: String::from_utf16_lossy(&filesystem[..end]),
                read_only: flags & FILE_READ_ONLY_VOLUME != 0,
            },
        ));
    }
    Ok(windows_disks(volumes))
}

/// Compare Windows paths without the host's std::path semantics. Require a
/// component boundary so C:\\recordings never matches C:\\recordings-old.
#[cfg(any(windows, test))]
fn windows_path_under(path: &str, mount: &str) -> bool {
    let path = path.replace('\\', "/").to_lowercase();
    let mount = mount.replace('\\', "/").to_lowercase();
    let mount = mount.trim_end_matches('/');
    (path == mount && !mount.ends_with(':')) || path.starts_with(&format!("{mount}/"))
}

#[cfg(any(windows, test))]
fn windows_drive_of<'a>(drives: &'a [Drive], path: &str) -> Option<&'a Drive> {
    drives
        .iter()
        .filter(|drive| windows_path_under(path, &drive.mount_point))
        .max_by_key(|drive| drive.mount_point.len())
}

/// The drive that holds `path`: the longest mount point it lies under.
fn drive_of<'a>(drives: &'a [Drive], path: &Path) -> Option<&'a Drive> {
    #[cfg(windows)]
    {
        windows_drive_of(drives, &path.to_string_lossy())
    }
    #[cfg(not(windows))]
    {
        drives
            .iter()
            .filter(|drive| path.starts_with(&drive.mount_point))
            .max_by_key(|drive| drive.mount_point.len())
    }
}

#[cfg(test)]
pub(crate) fn drives_from(
    mounts: Vec<Mount>,
    space: impl Fn(&str) -> Option<(u64, u64)>,
    current_storage: &Path,
) -> Vec<Drive> {
    drives_on(disk_mounts(mounts), space, current_storage)
}

fn drives_on(
    mounts: Vec<Mount>,
    space: impl Fn(&str) -> Option<(u64, u64)>,
    current_storage: &Path,
) -> Vec<Drive> {
    let mut drives: Vec<Drive> = mounts
        .into_iter()
        .filter_map(|mount| {
            let (total_bytes, available_bytes) = space(&mount.mount_point)?;
            Some(Drive {
                recommended: total_bytes >= RECOMMENDED_MIN_BYTES,
                mount_point: mount.mount_point,
                device: mount.device,
                filesystem: mount.filesystem,
                total_bytes,
                available_bytes,
                selected: false,
            })
        })
        .collect();
    if let Some(current) = drive_of(&drives, current_storage).map(|d| d.mount_point.clone()) {
        for drive in &mut drives {
            drive.selected = drive.mount_point == current;
        }
    }
    drives
}

/// Mounted drives a recording location can be put on. Unless `all`, only
/// drives of at least [`RECOMMENDED_MIN_BYTES`] (and the current one).
pub fn list(all: bool, current_storage: &Path) -> Result<Vec<Drive>, String> {
    let drives = drives_on(mounted_disks()?, space, current_storage);
    Ok(drives
        .into_iter()
        .filter(|drive| all || drive.recommended || drive.selected)
        .collect())
}

/// Create `<mount_point>/nw-storage` on a drive the scan reports and check it
/// can be written. Any other path is refused.
pub fn prepare(mount_point: &str) -> Result<PathBuf, String> {
    let drives = drives_on(mounted_disks()?, space, Path::new(""));
    prepare_on(&drives, mount_point)
}

pub(crate) fn prepare_on(drives: &[Drive], mount_point: &str) -> Result<PathBuf, String> {
    if !drives.iter().any(|drive| drive.mount_point == mount_point) {
        return Err("that drive is not available on this recorder".to_owned());
    }
    let folder = Path::new(mount_point).join(STORAGE_FOLDER);
    let denied = |error: std::io::Error| {
        if error.kind() == std::io::ErrorKind::PermissionDenied {
            format!(
                "cannot create {}: permission denied (give the user running NeuroWatch write access to this drive)",
                folder.display()
            )
        } else {
            format!("cannot use {}: {error}", folder.display())
        }
    };
    std::fs::create_dir_all(&folder).map_err(denied)?;
    let probe = folder.join(".nw-write-test");
    std::fs::write(&probe, b"ok").map_err(denied)?;
    let _ = std::fs::remove_file(&probe);
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNTS: &str = "\
sysfs /sys sysfs rw,nosuid 0 0
proc /proc proc rw 0 0
/dev/nvme0n1p2 / ext4 rw,relatime 0 0
/dev/nvme0n1p1 /boot/efi vfat rw 0 0
/dev/loop3 /snap/core/1 squashfs ro 0 0
tmpfs /run tmpfs rw 0 0
/dev/sdb1 /media/nw/Big\\040Disk ext4 rw 0 0
/dev/sdc1 /mnt/usb exfat rw 0 0
/dev/sdd1 /mnt/cdrom iso9660 ro 0 0
/dev/sdb1 /media/nw/Big\\040Disk/bind ext4 rw 0 0
server:/share /mnt/nas nfs4 rw 0 0
/dev/sde1 /mnt/readonly ext4 ro 0 0
";

    fn space(mount: &str) -> Option<(u64, u64)> {
        match mount {
            "/" => Some((500_000_000_000, 200_000_000_000)),
            "/media/nw/Big Disk" => Some((2_000_000_000_000, 1_500_000_000_000)),
            "/mnt/usb" => Some((64_000_000_000, 60_000_000_000)),
            _ => None,
        }
    }

    #[test]
    fn only_writable_disk_partitions_are_listed_once_each() {
        let drives = drives_from(parse_mounts(MOUNTS), space, Path::new("/home/nw/Videos"));
        let mounts: Vec<_> = drives.iter().map(|d| d.mount_point.as_str()).collect();
        assert_eq!(mounts, ["/", "/media/nw/Big Disk", "/mnt/usb"]);
        assert_eq!(
            drives.iter().map(|d| d.recommended).collect::<Vec<_>>(),
            [true, true, false]
        );
        // The current location (under /home on the root drive) is marked.
        assert_eq!(
            drives.iter().map(|d| d.selected).collect::<Vec<_>>(),
            [true, false, false]
        );
    }

    #[test]
    fn the_current_drive_is_the_longest_matching_mount() {
        let drives = drives_from(
            parse_mounts(MOUNTS),
            space,
            Path::new("/media/nw/Big Disk/nw-storage"),
        );
        let selected: Vec<_> = drives.iter().filter(|d| d.selected).collect();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].mount_point, "/media/nw/Big Disk");
    }

    #[test]
    fn only_a_listed_drive_can_be_chosen() {
        let dir = std::env::temp_dir().join(format!("nw-drive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mount = dir.to_string_lossy().into_owned();
        let drives = vec![Drive {
            mount_point: mount.clone(),
            device: "/dev/sdz1".into(),
            filesystem: "ext4".into(),
            total_bytes: 1,
            available_bytes: 1,
            recommended: false,
            selected: false,
        }];

        assert!(prepare_on(&drives, "/etc").is_err());
        assert!(prepare_on(&drives, &format!("{mount}/../..")).is_err());
        let folder = prepare_on(&drives, &mount).unwrap();
        assert_eq!(folder, dir.join(STORAGE_FOLDER));
        assert!(folder.is_dir());
        assert!(!folder.join(".nw-write-test").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn windows_lists_only_writable_fixed_and_removable_volumes() {
        let volume = |letter: &str, filesystem: &str, read_only| Mount {
            device: format!("{letter}:"),
            mount_point: format!("{letter}:\\"),
            filesystem: filesystem.to_owned(),
            read_only,
        };
        let mounts = windows_disks(vec![
            (2, volume("E", "exFAT", false)),
            (3, volume("C", "NTFS", false)),
            (3, volume("D", "ReFS", true)),
            (4, volume("N", "NTFS", false)), // network
            (5, volume("F", "UDF", false)),  // optical
            (6, volume("R", "NTFS", false)), // RAM disk
            (1, volume("X", "", false)),     // no media/root
        ]);
        let drives = drives_on(
            mounts,
            |path| match path {
                "C:\\" => Some((500_000_000_000, 120_000_000_000)),
                "E:\\" => Some((64_000_000_000, 40_000_000_000)),
                _ => None,
            },
            Path::new(""),
        );
        assert_eq!(
            drives
                .iter()
                .map(|d| d.mount_point.as_str())
                .collect::<Vec<_>>(),
            ["C:\\", "E:\\"]
        );
        assert_eq!(drives[0].device, "C:");
        assert_eq!(drives[0].filesystem, "NTFS");
        assert_eq!(drives[0].available_bytes, 120_000_000_000);
        assert!(drives[0].recommended);
        assert!(!drives[1].recommended);
        assert!(drives_on(
            windows_disks(vec![(3, volume("C", "NTFS", false))]),
            |_| None,
            Path::new("")
        )
        .is_empty());

        assert_eq!(
            windows_drive_of(&drives, "c:/Users/x/Videos"),
            Some(&drives[0])
        );
        assert_eq!(
            windows_drive_of(&drives, "e:\\nw-storage"),
            Some(&drives[1])
        );
        assert!(windows_drive_of(&drives, "Z:\\Videos").is_none());
    }

    #[test]
    fn windows_path_matching_accepts_either_slash_and_checks_component_boundaries() {
        assert!(windows_path_under("c:/Users/x/Videos", "C:\\"));
        assert!(windows_path_under("C:\\", "c:/"));
        assert!(windows_path_under(
            "d:\\Recordings\\Camera",
            "D:/recordings"
        ));
        assert!(!windows_path_under("D:/Recordings-old", "d:\\Recordings"));
        assert!(!windows_path_under("C:/Videos", "D:\\"));
        assert!(!windows_path_under("C:Videos", "C:\\"));
    }
}
