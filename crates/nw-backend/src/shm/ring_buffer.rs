//! Shared memory ring buffer writer (Rust backend side).
//!
//! Transport per platform:
//!   Unix:    file-backed mapping at `/dev/shm/neurowatch_{key}`
//!   Windows: named pagefile-backed section `Local\neurowatch_{key}`
//!
//! Global header layout (64 bytes):
//!   [0..4]    write_idx: u32 (atomic, little-endian)
//!   [4..8]    UI preview limit in milli-FPS (atomic u32 LE; 0 = unlimited)
//!   [8..12]   reader capability (YUV1 enables NV12)
//!   [12..64]  reserved
//!
//! Per-slot layout (SLOT_SIZE bytes each):
//!   [0..4]    width: u32 LE
//!   [4..8]    height: u32 LE
//!   [8..12]   data_len: u32 LE
//!   [12..]    RGB24 bytes, or NV12 descriptor + packed Y/UV planes
//! NV12 sets data_len bit 31; remaining bits are the payload length.
//! Descriptor: bits 0..1 matrix (1=601, 2=709, 3=2020), bit 4 full range,
//! bits 5/6 horizontal/vertical cosited chroma; all other bits are reserved.

use nw_core::camera::CameraId;
use nw_core::frame::shm::{
    HEADER_BYTES, MAX_FRAME_BYTES, NV12_CAPABILITY, NV12_SLOT_FLAG, PREVIEW_FORMAT_OFFSET,
    PREVIEW_RATE_OFFSET, SLOT_COUNT, SLOT_HEADER_BYTES, SLOT_SIZE, TOTAL_BYTES,
};
use std::sync::atomic::{AtomicU32, Ordering};

/// Kernel object name the UI opens on Windows (`Local\neurowatch_{key}`).
#[cfg(any(windows, test))]
pub fn windows_mapping_name(key: &str) -> String {
    let prefix = nw_core::frame::shm::SHM_PREFIX.trim_start_matches('/');
    format!("Local\\{prefix}{key}")
}

pub struct ShmRingBufferWriter {
    mapping: platform::Mapping,
    cam_id: CameraId,
}

impl ShmRingBufferWriter {
    pub fn create(cam_id: &CameraId) -> std::io::Result<Self> {
        let mapping = platform::Mapping::create(cam_id.as_str())?;
        tracing::debug!(cam_id = %cam_id, name = %mapping.name(), bytes = TOTAL_BYTES, "SHM created");
        Ok(Self {
            mapping,
            cam_id: cam_id.clone(),
        })
    }

    #[cfg(test)]
    pub(crate) fn request_preview_rate(&mut self, rate: u32) {
        let bytes = self.mapping.bytes_mut();
        let pointer = unsafe { bytes.as_mut_ptr().add(PREVIEW_RATE_OFFSET) as *mut AtomicU32 };
        unsafe { (*pointer).store(rate.to_le(), Ordering::Release) };
    }

    /// The wall's reader updates this optional limit when its visibility changes.
    pub fn preview_rate_millifps(&mut self) -> u32 {
        let bytes = self.mapping.bytes_mut();
        // The mapping and offset are aligned for AtomicU32, like write_idx.
        let pointer = unsafe { bytes.as_ptr().add(PREVIEW_RATE_OFFSET) as *const AtomicU32 };
        let rate = u32::from_le(unsafe { (*pointer).load(Ordering::Acquire) });
        if rate <= 120_000 {
            rate
        } else {
            0
        }
    }

    #[cfg(test)]
    pub(crate) fn request_nv12(&mut self, enabled: bool) {
        self.mapping.bytes_mut()[PREVIEW_FORMAT_OFFSET..PREVIEW_FORMAT_OFFSET + 4]
            .copy_from_slice(&(if enabled { NV12_CAPABILITY } else { 0 }).to_le_bytes());
    }

    pub fn accepts_nv12(&mut self) -> bool {
        let bytes = self.mapping.bytes_mut();
        let pointer = unsafe { bytes.as_ptr().add(PREVIEW_FORMAT_OFFSET) as *const AtomicU32 };
        u32::from_le(unsafe { (*pointer).load(Ordering::Acquire) }) == NV12_CAPABILITY
    }

    pub fn write_nv12_frame(
        &mut self,
        width: u32,
        height: u32,
        strides: [usize; 2],
        planes: [&[u8]; 2],
        descriptor: u32,
    ) -> bool {
        if width == 0 || height == 0 || width > 4096 || height > 4096 {
            return false;
        }
        let sizes = [
            (width as usize, height as usize),
            (
                (width as usize).div_ceil(2) * 2,
                (height as usize).div_ceil(2),
            ),
        ];
        let Some(length) = sizes.iter().try_fold(4usize, |sum, (row, rows)| {
            row.checked_mul(*rows)
                .and_then(|length| sum.checked_add(length))
        }) else {
            return false;
        };
        if width == 0
            || height == 0
            || length > MAX_FRAME_BYTES
            || sizes.iter().enumerate().any(|(i, (row, rows))| {
                strides[i] < *row
                    || rows
                        .saturating_sub(1)
                        .checked_mul(strides[i])
                        .and_then(|n| n.checked_add(*row))
                        .map_or(true, |n| n > planes[i].len())
            })
        {
            return false;
        }
        let bytes = self.mapping.bytes_mut();
        let index = unsafe { (*(bytes.as_ptr() as *const AtomicU32)).load(Ordering::Acquire) };
        let next = (index as usize + 1) % SLOT_COUNT;
        let slot = HEADER_BYTES + next * SLOT_SIZE;
        bytes[slot..slot + 4].copy_from_slice(&width.to_le_bytes());
        bytes[slot + 4..slot + 8].copy_from_slice(&height.to_le_bytes());
        bytes[slot + 8..slot + 12].copy_from_slice(&(length as u32 | NV12_SLOT_FLAG).to_le_bytes());
        let mut offset = slot + SLOT_HEADER_BYTES;
        bytes[offset..offset + 4].copy_from_slice(&descriptor.to_le_bytes());
        offset += 4;
        for (i, (row, rows)) in sizes.into_iter().enumerate() {
            if strides[i] == row {
                let size = row * rows;
                bytes[offset..offset + size].copy_from_slice(&planes[i][..size]);
                offset += size;
            } else {
                for y in 0..rows {
                    bytes[offset..offset + row]
                        .copy_from_slice(&planes[i][y * strides[i]..y * strides[i] + row]);
                    offset += row;
                }
            }
        }
        unsafe { (*(bytes.as_mut_ptr() as *mut AtomicU32)).store(next as u32, Ordering::Release) };
        true
    }

    /// Write tightly packed RGB24 into the next slot, stripping source padding.
    /// Returns false without publishing a slot when the frame layout is invalid.
    pub fn write_frame(
        &mut self,
        width: u32,
        height: u32,
        stride: usize,
        frame_data: &[u8],
    ) -> bool {
        let row_bytes = (width as usize).checked_mul(3);
        let data_len = row_bytes.and_then(|row| row.checked_mul(height as usize));
        let source_len = (height as usize)
            .checked_sub(1)
            .and_then(|rows| rows.checked_mul(stride))
            .and_then(|offset| row_bytes.and_then(|row| offset.checked_add(row)));
        let (Some(row_bytes), Some(data_len), Some(source_len)) = (row_bytes, data_len, source_len)
        else {
            return false;
        };
        if row_bytes == 0
            || stride < row_bytes
            || data_len > MAX_FRAME_BYTES
            || source_len > frame_data.len()
        {
            tracing::warn!(cam_id = %self.cam_id, width, height, stride, "Invalid RGB frame layout; frame dropped");
            return false;
        }

        let buf = self.mapping.bytes_mut();
        let write_idx_ptr = buf.as_ptr() as *const AtomicU32;
        let current = unsafe { (*write_idx_ptr).load(Ordering::Acquire) } as usize;
        let next_slot = (current + 1) % SLOT_COUNT;
        let slot_offset = HEADER_BYTES + next_slot * SLOT_SIZE;

        buf[slot_offset..slot_offset + 4].copy_from_slice(&width.to_le_bytes());
        buf[slot_offset + 4..slot_offset + 8].copy_from_slice(&height.to_le_bytes());
        buf[slot_offset + 8..slot_offset + 12].copy_from_slice(&(data_len as u32).to_le_bytes());

        // SHM and Qt use width * 3 bytes per row. GStreamer can align RGB
        // rows (854 pixels => 2564 bytes, versus 2562 packed) or supply a
        // hardware-specific stride. Copy straight into SHM without allocating.
        let data_offset = slot_offset + SLOT_HEADER_BYTES;
        let payload = &mut buf[data_offset..data_offset + data_len];
        if stride == row_bytes {
            payload.copy_from_slice(&frame_data[..data_len]);
        } else {
            for (row, destination) in payload.chunks_exact_mut(row_bytes).enumerate() {
                let source = row * stride;
                destination.copy_from_slice(&frame_data[source..source + row_bytes]);
            }
        }

        // Release ensures payload visibility before publishing the slot index.
        unsafe {
            (*(buf.as_mut_ptr() as *mut AtomicU32)).store(next_slot as u32, Ordering::Release)
        };
        true
    }
}

#[cfg(unix)]
mod platform {
    use memmap2::MmapMut;
    use nw_core::frame::shm::{SHM_PREFIX, TOTAL_BYTES};
    use std::os::unix::fs::MetadataExt;

    pub struct Mapping {
        mmap: MmapMut,
        path: String,
        /// Inode of the SHM file this writer owns.  Used in Drop to avoid
        /// unlinking a file that was already replaced by a newer supervisor.
        inode: u64,
    }

    impl Mapping {
        pub fn create(key: &str) -> std::io::Result<Self> {
            let path = format!("/dev/shm{SHM_PREFIX}{key}");

            // Unlink the old file *before* creating a new one.  This is critical:
            // if a previous supervisor still has the file mmap'd and we truncate it
            // in-place (via OpenOptions::truncate), accessing pages beyond offset 0
            // from the old mmap causes SIGBUS.  By unlinking first the old inode
            // stays alive (held by the old mmap) while we create a fresh inode at
            // the same path — both supervisors get independent mappings with no
            // truncation involved.
            let _ = std::fs::remove_file(&path);

            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            file.set_len(TOTAL_BYTES as u64)?;

            let inode = file.metadata()?.ino();
            let mmap = unsafe { MmapMut::map_mut(&file)? };
            Ok(Self { mmap, path, inode })
        }

        pub fn name(&self) -> &str {
            &self.path
        }

        pub fn bytes_mut(&mut self) -> &mut [u8] {
            &mut self.mmap
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // Only unlink if the path still refers to *our* inode.  A replacement
            // supervisor may have already unlinked the old inode and created a new
            // file at the same path; removing that file would break its frame
            // delivery and re-trigger the exact race we are trying to prevent.
            match std::fs::metadata(&self.path) {
                Ok(meta) if meta.ino() == self.inode => {
                    let _ = std::fs::remove_file(&self.path);
                    tracing::debug!(path = %self.path, "SHM removed (owned inode)");
                }
                Ok(_) => {
                    tracing::debug!(path = %self.path, "SHM not removed (replacement supervisor owns path)");
                }
                Err(_) => {
                    // Path already gone — nothing to do.
                }
            }
        }
    }
}

#[cfg(windows)]
mod platform {
    use nw_core::frame::shm::TOTAL_BYTES;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
        MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
    };

    /// Named, pagefile-backed section.  The kernel reference-counts it, so a
    /// restarted supervisor (or a UI reader still holding a handle) shares the
    /// same memory and it is released once every handle closes — no file to
    /// replace or unlink.
    pub struct Mapping {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
        name: String,
    }

    // The handle and view are process-wide kernel resources, not thread-bound.
    unsafe impl Send for Mapping {}

    impl Mapping {
        pub fn create(key: &str) -> std::io::Result<Self> {
            let name = super::windows_mapping_name(key);
            let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let size = TOTAL_BYTES as u64;
            let handle = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    std::ptr::null(),
                    PAGE_READWRITE,
                    (size >> 32) as u32,
                    size as u32,
                    wide.as_ptr(),
                )
            };
            if handle.is_null() {
                return Err(std::io::Error::last_os_error());
            }
            let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, TOTAL_BYTES) };
            if view.Value.is_null() {
                let error = std::io::Error::last_os_error();
                unsafe { CloseHandle(handle) };
                return Err(error);
            }
            Ok(Self { handle, view, name })
        }

        pub fn name(&self) -> &str {
            &self.name
        }

        pub fn bytes_mut(&mut self) -> &mut [u8] {
            unsafe { std::slice::from_raw_parts_mut(self.view.Value as *mut u8, TOTAL_BYTES) }
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nw_core::frame::shm::SHM_PREFIX;

    #[test]
    fn nv12_packs_odd_padded_planes_and_preserves_rgb_compatibility() {
        let mut writer =
            ShmRingBufferWriter::create(&format!("test-nv12-{}", uuid::Uuid::new_v4())).unwrap();
        assert!(!writer.accepts_nv12());
        writer.request_nv12(true);
        assert!(writer.accepts_nv12());
        assert!(writer.write_nv12_frame(
            3,
            3,
            [4, 6],
            [
                &[1, 2, 3, 99, 4, 5, 6, 99, 7, 8, 9],
                &[10, 11, 12, 13, 99, 99, 14, 15, 16, 17]
            ],
            34
        ));
        let bytes = writer.mapping.bytes_mut();
        let slot = HEADER_BYTES + SLOT_SIZE;
        assert_eq!(
            u32::from_le_bytes(bytes[slot + 8..slot + 12].try_into().unwrap()),
            NV12_SLOT_FLAG | 21
        );
        assert_eq!(
            &bytes[slot + 12..slot + 33],
            &[34, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
        );
        for (w, h, strides) in [
            (0, 3, [4, 4]),
            (3, 0, [4, 4]),
            (u32::MAX, u32::MAX, [usize::MAX; 2]),
            (3, 3, [2, 4]),
            (3, 3, [4, 2]),
        ] {
            assert!(!writer.write_nv12_frame(w, h, strides, [&[0; 16], &[0; 16]], 34));
        }
        assert!(!writer.write_nv12_frame(3, 3, [4, 6], [&[0; 10], &[0; 10]], 34));
        assert_eq!(
            u32::from_le_bytes(writer.mapping.bytes_mut()[..4].try_into().unwrap()),
            1
        );
        writer.request_nv12(false);
        assert!(!writer.accepts_nv12());
        assert!(writer.write_frame(1, 1, 3, &[1, 2, 3]));
        let slot = HEADER_BYTES + 2 * SLOT_SIZE;
        assert_eq!(
            u32::from_le_bytes(
                writer.mapping.bytes_mut()[slot + 8..slot + 12]
                    .try_into()
                    .unwrap()
            ),
            3
        );
    }

    #[test]
    fn windows_mapping_name_matches_ui_reader() {
        assert_eq!(windows_mapping_name("cam-1"), "Local\\neurowatch_cam-1");
        assert_eq!(windows_mapping_name("pb_abc"), "Local\\neurowatch_pb_abc");
    }

    #[test]
    fn preview_request_is_optional_and_does_not_change_frame_index() {
        let key = format!("test-preview-request-{}", uuid::Uuid::new_v4());
        let mut writer = ShmRingBufferWriter::create(&key).unwrap();
        assert_eq!(writer.preview_rate_millifps(), 0);
        assert!(writer.write_frame(1, 1, 3, &[1, 2, 3]));
        for (request, expected) in [(2_000, 2_000), (15_000, 15_000), (0, 0), (u32::MAX, 0)] {
            writer.request_preview_rate(request);
            assert_eq!(writer.preview_rate_millifps(), expected);
            assert_eq!(
                u32::from_le_bytes(writer.mapping.bytes_mut()[..4].try_into().unwrap()),
                1
            );
        }
    }

    #[test]
    fn rgb_rows_are_packed_without_alignment_or_hardware_padding() {
        for (width, height, stride) in [(1, 2, 4), (854, 480, 2564), (1920, 2, 5760), (3, 3, 64)] {
            let row_bytes = width as usize * 3;
            // The last row need not have trailing padding in the mapped plane.
            let mut source = vec![0xfd; (height as usize - 1) * stride + row_bytes];
            let mut expected = Vec::with_capacity(row_bytes * height as usize);
            for y in 0..height as usize {
                let row = &mut source[y * stride..y * stride + row_bytes];
                for (x, byte) in row.iter_mut().enumerate() {
                    *byte = ((y + x) % 251) as u8;
                }
                expected.extend_from_slice(row);
            }
            let key = format!("test-rgb-{}", uuid::Uuid::new_v4());
            let mut writer = ShmRingBufferWriter::create(&key).expect("create ring");
            assert!(writer.write_frame(width, height, stride, &source));
            let ring = writer.mapping.bytes_mut();
            let u32_at =
                |offset: usize| u32::from_le_bytes(ring[offset..offset + 4].try_into().unwrap());
            assert_eq!(u32_at(0), 1);
            let slot = HEADER_BYTES + SLOT_SIZE;
            assert_eq!(
                (u32_at(slot), u32_at(slot + 4), u32_at(slot + 8)),
                (width, height, expected.len() as u32),
            );
            assert_eq!(
                &ring[slot + SLOT_HEADER_BYTES..slot + SLOT_HEADER_BYTES + expected.len()],
                expected.as_slice(),
                "RGB row layout must survive {width}x{height}, stride {stride}",
            );
        }
    }

    #[test]
    fn invalid_rgb_layouts_do_not_publish_or_overwrite_a_frame() {
        let key = format!("test-rgb-invalid-{}", uuid::Uuid::new_v4());
        let mut writer = ShmRingBufferWriter::create(&key).expect("create ring");
        let valid = [1, 2, 3, 4, 5, 6];
        assert!(writer.write_frame(2, 1, 6, &valid));
        for (width, height, stride, bytes) in [
            (0, 1, 0, &[][..]),
            (1, 0, 3, &[][..]),
            (2, 2, 5, &[0; 12][..]),
            (2, 2, 8, &[0; 13][..]),
            (3840, 2161, 11520, &[][..]),
            (1, 3, usize::MAX, &[][..]),
            (u32::MAX, u32::MAX, usize::MAX, &[][..]),
        ] {
            assert!(!writer.write_frame(width, height, stride, bytes));
        }
        let ring = writer.mapping.bytes_mut();
        assert_eq!(u32::from_le_bytes(ring[..4].try_into().unwrap()), 1);
        let start = HEADER_BYTES + SLOT_SIZE + SLOT_HEADER_BYTES;
        assert_eq!(&ring[start..start + valid.len()], &valid);
    }

    #[cfg(unix)]
    #[test]
    fn writes_frame_layout_and_removes_owned_file() {
        let key = CameraId::from(format!("test-ring-{}", std::process::id()));
        let path = format!("/dev/shm{SHM_PREFIX}{key}");
        let mut writer = ShmRingBufferWriter::create(&key).expect("create ring");
        assert!(writer.write_frame(2, 1, 6, &[1, 2, 3, 4, 5, 6]));

        let file = std::fs::File::open(&path).expect("open ring");
        let mmap = unsafe { memmap2::Mmap::map(&file) }.expect("map ring");
        assert_eq!(mmap.len(), TOTAL_BYTES);
        let u32_at = |offset: usize| {
            u32::from_le_bytes(mmap[offset..offset + 4].try_into().expect("4 bytes"))
        };
        assert_eq!(u32_at(0), 1);
        let slot = HEADER_BYTES + SLOT_SIZE;
        assert_eq!(
            (u32_at(slot), u32_at(slot + 4), u32_at(slot + 8)),
            (2, 1, 6)
        );
        assert_eq!(
            &mmap[slot + SLOT_HEADER_BYTES..slot + SLOT_HEADER_BYTES + 6],
            &[1, 2, 3, 4, 5, 6]
        );

        drop(writer);
        assert!(std::fs::metadata(&path).is_err());
    }
}
