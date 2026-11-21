//! Windows: feed the "OBS Virtual Camera" DirectShow device through its
//! shared-memory queue (layout from OBS's shared-memory-queue.c).
use super::{yuv, Sink};
use std::time::Instant;
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS, FILE_MAP_READ,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

const STATE_STARTING: u32 = 1;
const STATE_READY: u32 = 2;
const STATE_STOPPING: u32 = 3;
const FRAME_HEADER_SIZE: usize = 32;

#[repr(C)]
struct QueueHeader {
    write_idx: u32,
    read_idx: u32,
    state: u32,
    offsets: [u32; 3],
    type_: u32,
    cx: u32,
    cy: u32,
    interval: u64, // frame duration in 100 ns units
    reserved: [u32; 8],
}

struct ObsSink {
    handle: HANDLE,
    header: *mut QueueHeader,
    width: usize,
    height: usize,
    start: Instant,
}

fn align32(n: usize) -> usize {
    (n + 31) & !31
}

pub fn open(width: usize, height: usize, fps: u32) -> Result<Box<dyn Sink>, String> {
    let name: Vec<u16> = "OBSVirtualCamVideo\0".encode_utf16().collect();
    let frame_size = width * height * 3 / 2;
    let mut size = align32(std::mem::size_of::<QueueHeader>());
    let mut offsets = [0u32; 3];
    for off in offsets.iter_mut() {
        *off = size as u32;
        size = align32(size + frame_size + FRAME_HEADER_SIZE);
    }
    // SAFETY: plain Win32 calls; every returned handle/pointer is checked before use.
    unsafe {
        let existing = OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr());
        if !existing.is_null() {
            CloseHandle(existing);
            return Err("the OBS Virtual Camera is already in use by another program (is OBS running?)".into());
        }
        let handle =
            CreateFileMappingW(INVALID_HANDLE_VALUE, std::ptr::null(), PAGE_READWRITE, 0, size as u32, name.as_ptr());
        if handle.is_null() {
            return Err("could not create the shared memory for the virtual camera".into());
        }
        let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, 0);
        if view.Value.is_null() {
            CloseHandle(handle);
            return Err("could not map the shared memory for the virtual camera".into());
        }
        let header = view.Value as *mut QueueHeader;
        header.write(QueueHeader {
            write_idx: 0,
            read_idx: 0,
            state: STATE_STARTING,
            offsets,
            type_: 0,
            cx: width as u32,
            cy: height as u32,
            interval: 10_000_000 / fps.max(1) as u64,
            reserved: [0; 8],
        });
        Ok(Box::new(ObsSink { handle, header, width, height, start: Instant::now() }))
    }
}

impl Sink for ObsSink {
    fn name(&self) -> String {
        "OBS Virtual Camera".into()
    }

    fn send(&mut self, rgb: &[u8]) -> Result<(), String> {
        let (w, h) = (self.width, self.height);
        // SAFETY: the mapping is large enough for three (header + NV12) frames
        // at the offsets we computed in `open`, and lives until Drop.
        unsafe {
            let hdr = &mut *self.header;
            let inc = hdr.write_idx.wrapping_add(1);
            std::ptr::write_volatile(&mut hdr.write_idx, inc);
            let base = (self.header as *mut u8).add(hdr.offsets[(inc % 3) as usize] as usize);
            (base as *mut u64).write_unaligned(self.start.elapsed().as_nanos() as u64);
            let frame = std::slice::from_raw_parts_mut(base.add(FRAME_HEADER_SIZE), w * h * 3 / 2);
            let (luma, chroma) = frame.split_at_mut(w * h);
            for y in 0..h {
                for x in 0..w {
                    let p = &rgb[(y * w + x) * 3..];
                    let (yy, u, v) = yuv(p[0], p[1], p[2]);
                    luma[y * w + x] = yy;
                    if y % 2 == 0 && x % 2 == 0 {
                        let o = (y / 2) * w + x;
                        chroma[o] = u as u8;
                        chroma[o + 1] = v as u8;
                    }
                }
            }
            std::ptr::write_volatile(&mut hdr.read_idx, inc);
            std::ptr::write_volatile(&mut hdr.state, STATE_READY);
        }
        Ok(())
    }
}

impl Drop for ObsSink {
    fn drop(&mut self) {
        // SAFETY: header/handle were created in `open` and are released exactly once.
        unsafe {
            std::ptr::write_volatile(&mut (*self.header).state, STATE_STOPPING);
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.header as *mut _ });
            CloseHandle(self.handle);
        }
    }
}
