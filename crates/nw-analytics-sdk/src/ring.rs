//! A latest-picture, read-only capability. The node owns the writable mapping;
//! a plugin receives only a read-only descriptor. Readers never block writers.
use memmap2::{Mmap, MmapMut, MmapOptions};
pub use nw_analytics_protocol::decoded::{RawFormat, RawLayout};
use std::fs::File;
#[cfg(not(target_os = "linux"))]
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{fence, AtomicU64, Ordering};

const MAGIC: &[u8; 8] = b"NWPIC001";
pub const HEADER: usize = 128;
pub const MAX_PICTURE: usize = 32 * 1024 * 1024;
const GENERATION: usize = 24;

#[derive(Debug, thiserror::Error)]
pub enum RingError {
    #[error("picture layout or shared-memory descriptor is invalid")]
    Invalid,
    #[error("shared picture: {0}")]
    Io(#[from] std::io::Error),
    #[error("could not generate a private picture capability")]
    Random,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureIdentity {
    pub seq: u64,
    pub time_us: i64,
    pub pts_ns: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Picture {
    pub layout: RawLayout,
    pub identity: PictureIdentity,
    pub data: Vec<u8>,
}

pub fn plane_sizes(layout: RawLayout) -> Result<[(usize, usize); 2], RingError> {
    if layout.width == 0 || layout.height == 0 || layout.width > 4096 || layout.height > 4096 {
        return Err(RingError::Invalid);
    }
    let (w, h) = (layout.width as usize, layout.height as usize);
    let planes = match layout.format {
        RawFormat::Rgb24 => [(w * 3, h), (0, 0)],
        RawFormat::Gray8 => [(w, h), (0, 0)],
        RawFormat::Nv12 => [(w, h), (w.div_ceil(2) * 2, h.div_ceil(2))],
    };
    let bytes: usize = planes.iter().map(|(stride, rows)| stride * rows).sum();
    if bytes > MAX_PICTURE || layout.color.is_some_and(|color| color & !0x73 != 0) {
        return Err(RingError::Invalid);
    }
    Ok(planes)
}

fn picture_bytes(layout: RawLayout) -> Result<usize, RingError> {
    Ok(plane_sizes(layout)?
        .iter()
        .map(|(stride, rows)| stride * rows)
        .sum())
}

fn token(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn private_dir(directory: &Path) -> Result<(), RingError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        match std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
        {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
        let metadata = std::fs::symlink_metadata(directory)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "picture directory must be private",
            )
            .into());
        }
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(directory)?;
    Ok(())
}

pub struct RingWriter {
    map: MmapMut,
    readonly: File,
    id: String,
    layout: RawLayout,
    bytes: usize,
}

impl RingWriter {
    pub fn create(directory: &Path, layout: RawLayout) -> Result<Self, RingError> {
        let size = picture_bytes(layout)?;
        private_dir(directory)?;
        let mut id = [0u8; 16];
        getrandom::getrandom(&mut id).map_err(|_| RingError::Random)?;
        #[cfg(target_os = "linux")]
        let file = {
            use std::os::fd::FromRawFd;
            let descriptor = unsafe {
                libc::memfd_create(
                    b"nw-analytics-picture\0".as_ptr().cast(),
                    libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
                )
            };
            if descriptor < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(descriptor) };
            if unsafe { libc::fchmod(descriptor, 0o600) } < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            file
        };
        #[cfg(not(target_os = "linux"))]
        let path = directory.join(format!("picture-{}", token(&id)));
        #[cfg(not(target_os = "linux"))]
        let file = {
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                options.share_mode(1 | 2 | 4);
                // Delete-on-close is established before mapping the file. Windows
                // cannot unlink a mapped file using the Unix remove-after-open pattern.
                options.access_mode(0x8000_0000 | 0x4000_0000 | 0x0001_0000);
                options.custom_flags(0x0400_0000 | 0x0000_0100);
            }
            options.open(&path)?
        };
        let result = (|| {
            file.set_len((HEADER + size) as u64)?;
            #[cfg(target_os = "linux")]
            let readonly = {
                use std::os::fd::AsRawFd;
                let descriptor = file.as_raw_fd();
                if unsafe {
                    libc::fcntl(
                        descriptor,
                        libc::F_ADD_SEALS,
                        libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL,
                    )
                } < 0
                {
                    return Err(std::io::Error::last_os_error().into());
                }
                File::open(format!("/proc/self/fd/{descriptor}"))?
            };
            #[cfg(not(target_os = "linux"))]
            let readonly = {
                let mut read = OpenOptions::new();
                read.read(true);
                #[cfg(windows)]
                {
                    use std::os::windows::fs::OpenOptionsExt;
                    read.share_mode(1 | 2 | 4);
                }
                read.open(&path)?
            };
            // The file's size was checked/set above; only this writer can resize it.
            let mut map = unsafe { MmapOptions::new().len(HEADER + size).map_mut(&file) }?;
            map[..8].copy_from_slice(MAGIC);
            map[8..24].copy_from_slice(&id);
            map[56..60].copy_from_slice(&layout.width.to_le_bytes());
            map[60..64].copy_from_slice(&layout.height.to_le_bytes());
            map[64..68].copy_from_slice(
                &(match layout.format {
                    RawFormat::Rgb24 => 1u32,
                    RawFormat::Nv12 => 2,
                    RawFormat::Gray8 => 3,
                })
                .to_le_bytes(),
            );
            map[68..72].copy_from_slice(&(size as u32).to_le_bytes());
            map[72..76].copy_from_slice(&layout.color.unwrap_or(0).to_le_bytes());
            Ok(Self {
                map,
                readonly,
                id: token(&id),
                layout,
                bytes: HEADER + size,
            })
        })();
        #[cfg(not(any(windows, target_os = "linux")))]
        std::fs::remove_file(&path)?;
        result
    }
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn bytes(&self) -> u64 {
        self.bytes as u64
    }
    pub fn layout(&self) -> RawLayout {
        self.layout
    }
    pub fn descriptor(&self) -> Result<File, RingError> {
        Ok(self.readonly.try_clone()?)
    }

    /// Copy only existing planes; strip row padding without any conversion.
    pub fn write(
        &mut self,
        identity: PictureIdentity,
        planes: [&[u8]; 2],
        strides: [usize; 2],
    ) -> Result<(), RingError> {
        let sizes = plane_sizes(self.layout)?;
        for (index, (width, height)) in sizes.iter().enumerate() {
            if *height > 0
                && (strides[index] < *width
                    || (height - 1)
                        .checked_mul(strides[index])
                        .and_then(|size| size.checked_add(*width))
                        .map_or(true, |size| size > planes[index].len()))
            {
                return Err(RingError::Invalid);
            }
        }
        // mmap is page-aligned and GENERATION is aligned to AtomicU64.
        let sequence = unsafe { self.map.as_mut_ptr().add(GENERATION).cast::<AtomicU64>() };
        let previous = u64::from_le(unsafe { (*sequence).load(Ordering::Relaxed) });
        unsafe { (*sequence).swap(previous.wrapping_add(1).to_le(), Ordering::AcqRel) };
        self.map[32..40].copy_from_slice(&identity.seq.to_le_bytes());
        self.map[40..48].copy_from_slice(&identity.time_us.to_le_bytes());
        self.map[48..56].copy_from_slice(&identity.pts_ns.unwrap_or(u64::MAX).to_le_bytes());
        let mut at = HEADER;
        for (index, (width, height)) in sizes.into_iter().enumerate() {
            for row in 0..height {
                self.map[at..at + width].copy_from_slice(
                    &planes[index][row * strides[index]..row * strides[index] + width],
                );
                at += width;
            }
        }
        unsafe { (*sequence).store(previous.wrapping_add(2).to_le(), Ordering::Release) };
        Ok(())
    }
}

fn word(map: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([map[at], map[at + 1], map[at + 2], map[at + 3]])
}
fn wide(map: &[u8], at: usize) -> u64 {
    u64::from_le_bytes([
        map[at],
        map[at + 1],
        map[at + 2],
        map[at + 3],
        map[at + 4],
        map[at + 5],
        map[at + 6],
        map[at + 7],
    ])
}

pub fn descriptor_id(file: &File) -> Result<String, RingError> {
    use std::io::{Read, Seek, SeekFrom};
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.len() < HEADER as u64
        || metadata.len() > (HEADER + MAX_PICTURE) as u64
    {
        return Err(RingError::Invalid);
    }
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(0))?;
    let mut header = [0u8; 24];
    file.read_exact(&mut header)?;
    if &header[..8] != MAGIC {
        return Err(RingError::Invalid);
    }
    Ok(token(&header[8..24]))
}

pub struct RingReader {
    map: Mmap,
    layout: RawLayout,
    last: u64,
}
impl RingReader {
    pub fn open(file: File, id: &str, bytes: u64) -> Result<Self, RingError> {
        if descriptor_id(&file)? != id || file.metadata()?.len() != bytes {
            return Err(RingError::Invalid);
        }
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 || flags & libc::O_ACCMODE != libc::O_RDONLY {
                return Err(RingError::Invalid);
            }
        }
        // Descriptor is read-only, regular, bounded, and has a full immutable header.
        let map = unsafe { MmapOptions::new().map(&file) }?;
        let layout = RawLayout {
            width: word(&map, 56),
            height: word(&map, 60),
            format: match word(&map, 64) {
                1 => RawFormat::Rgb24,
                2 => RawFormat::Nv12,
                3 => RawFormat::Gray8,
                _ => return Err(RingError::Invalid),
            },
            color: match word(&map, 72) {
                0 => None,
                color => Some(color),
            },
        };
        let size = picture_bytes(layout)?;
        if map.len() != HEADER + size || word(&map, 68) as usize != size {
            return Err(RingError::Invalid);
        }
        Ok(Self {
            map,
            layout,
            last: 0,
        })
    }
    pub fn layout(&self) -> RawLayout {
        self.layout
    }
    pub fn latest(&mut self) -> Option<Picture> {
        let sequence = unsafe { &*self.map.as_ptr().add(GENERATION).cast::<AtomicU64>() };
        let before = u64::from_le(sequence.load(Ordering::Acquire));
        if before == 0 || before == self.last || before & 1 != 0 {
            return None;
        }
        let pts = wide(&self.map, 48);
        let identity = PictureIdentity {
            seq: wide(&self.map, 32),
            time_us: wide(&self.map, 40) as i64,
            pts_ns: (pts != u64::MAX).then_some(pts),
        };
        let data = self.map[HEADER..].to_vec();
        fence(Ordering::Acquire);
        if u64::from_le(sequence.load(Ordering::Acquire)) != before {
            return None;
        }
        self.last = before;
        Some(Picture {
            layout: self.layout,
            identity,
            data,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directory(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("nw-sdk-{name}-{}", std::process::id()))
    }
    fn identity() -> PictureIdentity {
        PictureIdentity {
            seq: 1u64 << 40,
            time_us: 1_700_000_000_123_456,
            pts_ns: Some(123_000),
        }
    }

    #[test]
    fn existing_rgb_planes_are_copied_without_padding_or_conversion() {
        let dir = directory("rgb");
        let layout = RawLayout {
            width: 2,
            height: 2,
            format: RawFormat::Rgb24,
            color: None,
        };
        let mut writer = RingWriter::create(&dir, layout).unwrap();
        let mut reader =
            RingReader::open(writer.descriptor().unwrap(), writer.id(), writer.bytes()).unwrap();
        assert!(reader.latest().is_none());
        writer
            .write(
                identity(),
                [&[1, 2, 3, 4, 5, 6, 99, 99, 7, 8, 9, 10, 11, 12], &[]],
                [8, 0],
            )
            .unwrap();
        let frame = reader.latest().unwrap();
        assert_eq!(frame.identity, identity());
        assert_eq!(frame.layout, layout);
        assert_eq!(frame.data, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        assert!(reader.latest().is_none());
        assert!(writer.write(identity(), [&[0; 3], &[]], [6, 0]).is_err());
        assert!(
            reader.latest().is_none(),
            "invalid writes never publish a torn picture"
        );
        drop(writer);
        assert_eq!(frame.data.len(), 12, "the reader owns the copied picture");
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn nv12_odd_sizes_preserve_existing_y_and_uv_planes_and_color() {
        let dir = directory("nv12");
        let layout = RawLayout {
            width: 3,
            height: 3,
            format: RawFormat::Nv12,
            color: Some(2),
        };
        let mut writer = RingWriter::create(&dir, layout).unwrap();
        let mut reader =
            RingReader::open(writer.descriptor().unwrap(), writer.id(), writer.bytes()).unwrap();
        writer
            .write(
                identity(),
                [
                    &[1, 2, 3, 99, 4, 5, 6, 99, 7, 8, 9],
                    &[10, 11, 12, 13, 14, 15, 16, 17],
                ],
                [4, 4],
            )
            .unwrap();
        let frame = reader.latest().unwrap();
        assert_eq!(frame.layout, layout);
        assert_eq!(frame.data, (1u8..=17).collect::<Vec<_>>());
        assert!(RingReader::open(writer.descriptor().unwrap(), "wrong", writer.bytes()).is_err());
        assert!(RingReader::open(
            writer.descriptor().unwrap(),
            writer.id(),
            writer.bytes() + 1
        )
        .is_err());
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn oversized_or_empty_pictures_are_refused_before_allocating_shared_memory() {
        for (width, height) in [(0, 1), (1, 0), (4097, 1), (4096, 4096)] {
            assert!(plane_sizes(RawLayout {
                width,
                height,
                format: RawFormat::Rgb24,
                color: None
            })
            .is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_public_directory_is_refused_without_changing_its_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = directory("public");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(RingWriter::create(
            &dir,
            RawLayout {
                width: 1,
                height: 1,
                format: RawFormat::Gray8,
                color: None
            }
        )
        .is_err());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o777
        );
        std::fs::remove_dir(dir).unwrap();
    }
}
