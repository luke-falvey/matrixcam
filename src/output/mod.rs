//! Virtual camera outputs. Each platform has one backend behind the same trait.

#[cfg(target_os = "linux")]
mod v4l2;
#[cfg(windows)]
pub mod mf_vcam;

pub trait Sink {
    /// Human-readable device name, shown to the user.
    fn name(&self) -> String;
    /// Send one tightly packed RGB frame (width * height * 3 bytes).
    fn send(&mut self, rgb: &[u8]) -> Result<(), String>;
}

#[allow(unused_variables)]
pub fn open(width: usize, height: usize, fps: u32, device: Option<&str>) -> Result<Box<dyn Sink>, String> {
    #[cfg(target_os = "linux")]
    return v4l2::open(width, height, device);
    #[cfg(windows)]
    return mf_vcam::open(width, height, fps);
    #[cfg(not(any(target_os = "linux", windows)))]
    return Err("no virtual camera backend on this platform yet (see README); \
                use the preview window instead"
        .into());
}

/// BT.601 limited-range luma / chroma from 8-bit RGB.
#[inline]
#[cfg(target_os = "linux")]
fn yuv(r: u8, g: u8, b: u8) -> (u8, i32, i32) {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    let y = (66 * r + 129 * g + 25 * b + 128 + (16 << 8)) >> 8;
    let u = (-38 * r - 74 * g + 112 * b + 128 + (128 << 8)) >> 8;
    let v = (112 * r - 94 * g - 18 * b + 128 + (128 << 8)) >> 8;
    (y as u8, u, v)
}
