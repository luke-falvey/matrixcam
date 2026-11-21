//! Frame transport between matrixcam.exe (user session) and the virtual camera
//! DLL, which Windows loads inside its camera service (a different session and
//! account). Both sides memory-map the same file under %ProgramData%; named
//! shared memory is not used because an unprivileged process cannot create it
//! in the global namespace the service would need.
//!
//! Layout: 64-byte header, then two NV12 frame slots. The writer fills the slot
//! the reader is not using and then bumps `seq`; slot `seq & 1` is the latest.
use std::path::PathBuf;

/// COM class id of the virtual camera media source.
pub const CLSID_U128: u128 = 0x908c69b4_52de_4ed7_a0cb_93979460aff1;
pub const CLSID_STR: &str = "{908C69B4-52DE-4ED7-A0CB-93979460AFF1}";
pub const CAMERA_NAME: &str = "Matrix Cam";

pub const MAGIC: u32 = 0x4D58_434D;
pub const VERSION: u32 = 1;
pub const HEADER_SIZE: usize = 64;

pub fn data_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into())).join("matrixcam")
}

pub fn frames_path() -> PathBuf {
    data_dir().join("frames.bin")
}

pub const fn frame_size(width: usize, height: usize) -> usize {
    width * height * 3 / 2
}

/// BT.601 limited-range RGB -> NV12. `dst` must hold `frame_size(width, height)` bytes.
pub fn rgb_to_nv12(rgb: &[u8], width: usize, height: usize, dst: &mut [u8]) {
    let (luma, chroma) = dst.split_at_mut(width * height);
    for y in 0..height {
        let row = &rgb[y * width * 3..(y + 1) * width * 3];
        for x in 0..width {
            let (r, g, b) = (row[x * 3] as i32, row[x * 3 + 1] as i32, row[x * 3 + 2] as i32);
            luma[y * width + x] = ((66 * r + 129 * g + 25 * b + 128 + (16 << 8)) >> 8) as u8;
            if y % 2 == 0 && x % 2 == 0 {
                let o = (y / 2) * width + x;
                chroma[o] = ((-38 * r - 74 * g + 112 * b + 128 + (128 << 8)) >> 8) as u8;
                chroma[o + 1] = ((112 * r - 94 * g - 18 * b + 128 + (128 << 8)) >> 8) as u8;
            }
        }
    }
}

#[cfg(windows)]
pub use win::Shm;

#[cfg(windows)]
mod win {
    use super::*;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::sync::atomic::{AtomicU32, Ordering};
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::System::Memory::{
        CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS,
        PAGE_READONLY, PAGE_READWRITE,
    };

    const SHARE_ALL: u32 = 7; // FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE

    #[repr(C)]
    struct Header {
        magic: u32,
        version: u32,
        width: u32,
        height: u32,
        fps: u32,
        seq: AtomicU32,
    }

    pub struct Shm {
        _file: File,
        mapping: HANDLE,
        ptr: *mut u8,
        pub width: usize,
        pub height: usize,
        pub fps: u32,
    }

    // The mapping is plain memory; access is coordinated through `seq`.
    unsafe impl Send for Shm {}

    impl Shm {
        fn map(file: File, len: usize, write: bool) -> io::Result<(File, HANDLE, *mut u8)> {
            let (protect, access) = if write { (PAGE_READWRITE, FILE_MAP_WRITE) } else { (PAGE_READONLY, FILE_MAP_READ) };
            // SAFETY: the file handle is valid for the duration of the calls; results are checked.
            unsafe {
                let mapping = CreateFileMappingW(HANDLE(file.as_raw_handle()), None, protect, 0, 0, None)
                    .map_err(|e| io::Error::other(e.to_string()))?;
                let view = MapViewOfFile(mapping, access, 0, 0, len);
                if view.Value.is_null() {
                    let err = io::Error::last_os_error();
                    let _ = CloseHandle(mapping);
                    return Err(err);
                }
                Ok((file, mapping, view.Value as *mut u8))
            }
        }

        /// Writer side: create (or reuse) the frame file for the given format.
        pub fn create(width: usize, height: usize, fps: u32) -> io::Result<Shm> {
            std::fs::create_dir_all(data_dir())?;
            let len = HEADER_SIZE + 2 * frame_size(width, height);
            let file = OpenOptions::new().read(true).write(true).create(true).share_mode(SHARE_ALL).open(frames_path())?;
            file.set_len(len as u64)?;
            let (file, mapping, ptr) = Self::map(file, len, true)?;
            // SAFETY: the view is at least HEADER_SIZE bytes and page-aligned.
            unsafe {
                let h = &mut *(ptr as *mut Header);
                h.magic = MAGIC;
                h.version = VERSION;
                h.width = width as u32;
                h.height = height as u32;
                h.fps = fps;
            }
            Ok(Shm { _file: file, mapping, ptr, width, height, fps })
        }

        /// Reader side: open the frame file written by a running matrixcam.
        pub fn open() -> io::Result<Shm> {
            let file = OpenOptions::new().read(true).share_mode(SHARE_ALL).open(frames_path())?;
            let file_len = file.metadata()?.len() as usize;
            if file_len < HEADER_SIZE {
                return Err(io::Error::other("frame file too small"));
            }
            let (file, mapping, ptr) = Self::map(file, file_len, false)?;
            // SAFETY: the view covers the header.
            let (magic, version, width, height, fps) = unsafe {
                let h = &*(ptr as *const Header);
                (h.magic, h.version, h.width as usize, h.height as usize, h.fps)
            };
            let shm = Shm { _file: file, mapping, ptr, width, height, fps };
            let sane = magic == MAGIC && version == VERSION && (64..=8192).contains(&width) && (64..=8192).contains(&height);
            if !sane || file_len < HEADER_SIZE + 2 * frame_size(width, height) {
                return Err(io::Error::other("frame file has an unexpected format"));
            }
            Ok(shm)
        }

        fn header(&self) -> &Header {
            // SAFETY: ptr points at a mapped, aligned Header for the lifetime of self.
            unsafe { &*(self.ptr as *const Header) }
        }

        fn slot(&self, index: u32) -> *mut u8 {
            // SAFETY: both slots lie inside the mapping (checked in create/open).
            unsafe { self.ptr.add(HEADER_SIZE + (index as usize & 1) * frame_size(self.width, self.height)) }
        }

        /// Number of frames published so far.
        pub fn seq(&self) -> u32 {
            self.header().seq.load(Ordering::Acquire)
        }

        /// Writer: fill the next slot through `fill`, then publish it.
        pub fn write_frame(&mut self, fill: impl FnOnce(&mut [u8])) {
            let next = self.seq().wrapping_add(1);
            // SAFETY: writer-only slot; the reader uses the other one until seq changes.
            fill(unsafe { std::slice::from_raw_parts_mut(self.slot(next), frame_size(self.width, self.height)) });
            self.header().seq.store(next, Ordering::Release);
        }

        /// Reader: the most recently published NV12 frame.
        pub fn latest(&self) -> &[u8] {
            // SAFETY: slot is inside the mapping; a torn read can only happen if the
            // writer laps us twice during one copy, which costs one glitched frame.
            unsafe { std::slice::from_raw_parts(self.slot(self.seq()), frame_size(self.width, self.height)) }
        }
    }

    impl Drop for Shm {
        fn drop(&mut self) {
            // SAFETY: ptr/mapping came from MapViewOfFile/CreateFileMappingW and are released once.
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.ptr as *mut _ });
                let _ = CloseHandle(self.mapping);
            }
        }
    }
}
