//! Linux: write YUYV frames to a v4l2loopback device.
use super::{yuv, Sink};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;

const VIDIOC_S_FMT: libc::c_ulong = 0xC0D0_5605; // _IOWR('V', 5, struct v4l2_format)
const V4L2_BUF_TYPE_VIDEO_OUTPUT: u32 = 2;
const V4L2_PIX_FMT_YUYV: u32 = u32::from_le_bytes(*b"YUYV");
const V4L2_FIELD_NONE: u32 = 1;
const V4L2_COLORSPACE_SRGB: u32 = 8;

#[repr(C)]
#[derive(Default)]
struct PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

/// struct v4l2_format on 64-bit: u32 type, 4 bytes padding, 200-byte union.
#[repr(C)]
struct Format {
    type_: u32,
    _pad: u32,
    pix: PixFormat,
    _rest: [u8; 200 - std::mem::size_of::<PixFormat>()],
}

struct V4l2Sink {
    file: File,
    path: String,
    width: usize,
    buf: Vec<u8>,
}

/// v4l2loopback devices live under /sys/devices/virtual/video4linux.
fn find_loopback() -> Option<String> {
    let mut names: Vec<String> = std::fs::read_dir("/sys/devices/virtual/video4linux")
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.starts_with("video"))
        .collect();
    names.sort();
    names.first().map(|n| format!("/dev/{n}"))
}

pub fn open(width: usize, height: usize, device: Option<&str>) -> Result<Box<dyn Sink>, String> {
    let path = match device {
        Some(d) => d.to_string(),
        None => find_loopback().ok_or(
            "no v4l2loopback device found. Load it with:\n    sudo modprobe v4l2loopback \
             devices=1 exclusive_caps=1 card_label=\"Matrix Cam\"",
        )?,
    };
    let file = OpenOptions::new().write(true).open(&path).map_err(|e| format!("cannot open {path}: {e}"))?;
    let mut fmt = Format {
        type_: V4L2_BUF_TYPE_VIDEO_OUTPUT,
        _pad: 0,
        pix: PixFormat {
            width: width as u32,
            height: height as u32,
            pixelformat: V4L2_PIX_FMT_YUYV,
            field: V4L2_FIELD_NONE,
            bytesperline: (width * 2) as u32,
            sizeimage: (width * height * 2) as u32,
            colorspace: V4L2_COLORSPACE_SRGB,
            ..Default::default()
        },
        _rest: [0; 200 - std::mem::size_of::<PixFormat>()],
    };
    // SAFETY: fmt is a correctly sized and initialised v4l2_format for this ioctl.
    if unsafe { libc::ioctl(file.as_raw_fd(), VIDIOC_S_FMT, &mut fmt) } < 0 {
        return Err(format!(
            "{path} rejected the video format ({}). Is it a v4l2loopback device?",
            std::io::Error::last_os_error()
        ));
    }
    Ok(Box::new(V4l2Sink { file, path, width, buf: vec![0; width * height * 2] }))
}

impl Sink for V4l2Sink {
    fn name(&self) -> String {
        self.path.clone()
    }

    fn send(&mut self, rgb: &[u8]) -> Result<(), String> {
        let _ = self.width;
        for (src, dst) in rgb.chunks_exact(6).zip(self.buf.chunks_exact_mut(4)) {
            let (y0, u0, v0) = yuv(src[0], src[1], src[2]);
            let (y1, u1, v1) = yuv(src[3], src[4], src[5]);
            dst[0] = y0;
            dst[1] = ((u0 + u1) / 2) as u8;
            dst[2] = y1;
            dst[3] = ((v0 + v1) / 2) as u8;
        }
        self.file.write_all(&self.buf).map_err(|e| format!("write to {} failed: {e}", self.path))
    }
}
