//! matrixcam - turn your webcam into Matrix-style digital rain on a virtual camera.
mod effect;
mod output;

use clap::Parser;
use effect::MatrixEffect;
use nokhwa::pixel_format::RgbFormat;
use nokhwa::utils::{ApiBackend, CameraFormat, CameraIndex, FrameFormat, RequestedFormat, RequestedFormatType, Resolution};
use nokhwa::Camera;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// Webcam index
    #[arg(short, long, default_value_t = 0)]
    camera: u32,
    #[arg(long, default_value_t = 1280)]
    width: usize,
    #[arg(long, default_value_t = 720)]
    height: usize,
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// Glyph height in pixels; smaller = more detail
    #[arg(long, default_value_t = 15)]
    cell: usize,
    /// Flip the image horizontally
    #[arg(long)]
    mirror: bool,
    /// Also show a preview window
    #[arg(short, long)]
    preview: bool,
    /// Preview window only, do not open a virtual camera
    #[arg(long)]
    no_virtualcam: bool,
    /// Virtual camera device (Linux, e.g. /dev/video10)
    #[arg(long)]
    device: Option<String>,
    /// List webcams and exit
    #[arg(long)]
    list_cameras: bool,
    /// Install the virtual camera component and exit (needs administrator rights)
    #[cfg(windows)]
    #[arg(long)]
    install_driver: bool,
    /// Remove the virtual camera component and exit (needs administrator rights)
    #[cfg(windows)]
    #[arg(long)]
    uninstall_driver: bool,
    /// Use a still image instead of a webcam (for testing)
    #[arg(long, hide = true)]
    image: Option<String>,
    /// Render ~2 s, write a PNG and exit (for testing)
    #[arg(long, hide = true)]
    snapshot: Option<String>,
}

/// A frame's brightness, one byte per pixel, plus its size.
type Luma = (Vec<u8>, usize, usize);

/// Where frames come from: a live camera or a still image.
enum Source {
    Camera(Camera),
    Still(Vec<u8>, usize, usize),
}

impl Source {
    fn open(args: &Args) -> Result<Source, String> {
        if let Some(path) = &args.image {
            let img = image::open(path).map_err(|e| format!("cannot read {path}: {e}"))?.to_luma8();
            let (w, h) = (img.width() as usize, img.height() as usize);
            return Ok(Source::Still(img.into_raw(), w, h));
        }
        init_camera_permission()?;
        let want = CameraFormat::new(Resolution::new(args.width as u32, args.height as u32), FrameFormat::MJPEG, args.fps);
        let mut last_err = String::new();
        // Prefer the requested size; fall back to whatever the camera offers.
        for kind in [RequestedFormatType::Closest(want), RequestedFormatType::AbsoluteHighestFrameRate, RequestedFormatType::None] {
            match Camera::new(CameraIndex::Index(args.camera), RequestedFormat::new::<RgbFormat>(kind)) {
                Ok(mut cam) => match cam.open_stream() {
                    Ok(()) => return Ok(Source::Camera(cam)),
                    Err(e) => last_err = e.to_string(),
                },
                Err(e) => last_err = e.to_string(),
            }
        }
        Err(format!(
            "could not open camera {}: {last_err}\nIs another app (e.g. Teams) already using the webcam? Try --list-cameras.",
            args.camera
        ))
    }

    fn frame(&mut self) -> Result<Luma, String> {
        match self {
            Source::Still(data, w, h) => Ok((data.clone(), *w, *h)),
            Source::Camera(cam) => {
                let frame = cam.frame().map_err(|e| e.to_string())?;
                let res = frame.resolution();
                luma_from(frame.buffer(), frame.source_frame_format(), res.width() as usize, res.height() as usize)
            }
        }
    }
}

/// Extracts brightness from a raw camera frame. The effect only needs luma,
/// which the YUV formats carry directly.
fn luma_from(data: &[u8], format: FrameFormat, w: usize, h: usize) -> Result<Luma, String> {
    let short = || format!("camera sent a truncated {format:?} frame");
    let rgb_luma = |px: &[u8], r: usize, b: usize| ((px[r] as u32 * 77 + px[1] as u32 * 150 + px[b] as u32 * 29) >> 8) as u8;
    let luma = match format {
        FrameFormat::MJPEG => {
            let img = image::load_from_memory_with_format(data, image::ImageFormat::Jpeg)
                .map_err(|e| format!("bad JPEG frame from camera: {e}"))?
                .to_luma8();
            let (w, h) = (img.width() as usize, img.height() as usize);
            return Ok((img.into_raw(), w, h));
        }
        FrameFormat::YUYV => data.get(..w * h * 2).ok_or_else(short)?.iter().step_by(2).copied().collect(),
        FrameFormat::NV12 | FrameFormat::GRAY => data.get(..w * h).ok_or_else(short)?.to_vec(),
        FrameFormat::RAWRGB => data.get(..w * h * 3).ok_or_else(short)?.chunks_exact(3).map(|p| rgb_luma(p, 0, 2)).collect(),
        FrameFormat::RAWBGR => data.get(..w * h * 3).ok_or_else(short)?.chunks_exact(3).map(|p| rgb_luma(p, 2, 0)).collect(),
    };
    Ok((luma, w, h))
}

/// macOS asks the user for camera permission; elsewhere this is a no-op.
fn init_camera_permission() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let (tx, rx) = std::sync::mpsc::channel();
        nokhwa::nokhwa_initialize(move |granted| {
            let _ = tx.send(granted);
        });
        if !rx.recv().unwrap_or(false) {
            return Err("camera permission was denied (System Settings > Privacy & Security > Camera)".into());
        }
    }
    Ok(())
}

fn list_cameras() -> Result<(), String> {
    init_camera_permission()?;
    let cams = nokhwa::query(ApiBackend::Auto).map_err(|e| e.to_string())?;
    if cams.is_empty() {
        println!("  no cameras found");
    }
    for c in cams {
        println!("  camera {}: {}", c.index(), c.human_name());
    }
    Ok(())
}

fn mirror(luma: &mut [u8], w: usize) {
    luma.chunks_exact_mut(w).for_each(|row| row.reverse());
}

#[cfg(feature = "preview")]
struct Preview {
    window: minifb::Window,
    buf: Vec<u32>,
}

#[cfg(feature = "preview")]
impl Preview {
    fn new(w: usize, h: usize) -> Result<Self, String> {
        let window = minifb::Window::new("matrixcam", w, h, minifb::WindowOptions::default())
            .map_err(|e| format!("cannot open preview window: {e}"))?;
        Ok(Preview { window, buf: vec![0; w * h] })
    }

    /// Returns false once the window was closed or Esc / Q was pressed.
    fn show(&mut self, rgb: &[u8], w: usize, h: usize) -> bool {
        for (d, s) in self.buf.iter_mut().zip(rgb.chunks_exact(3)) {
            *d = (s[0] as u32) << 16 | (s[1] as u32) << 8 | s[2] as u32;
        }
        let _ = self.window.update_with_buffer(&self.buf, w, h);
        self.window.is_open() && !self.window.is_key_down(minifb::Key::Escape) && !self.window.is_key_down(minifb::Key::Q)
    }
}

fn run() -> Result<(), String> {
    let args = Args::parse();
    if args.list_cameras {
        return list_cameras();
    }
    #[cfg(windows)]
    {
        use output::mf_vcam::{install_driver, run_elevated, uninstall_driver};
        if args.install_driver {
            return install_driver().or_else(|_| run_elevated("--install-driver"));
        }
        if args.uninstall_driver {
            return uninstall_driver().or_else(|_| run_elevated("--uninstall-driver"));
        }
    }
    if args.width < 64 || args.height < 64 || args.width % 2 != 0 || args.height % 2 != 0 {
        return Err("--width and --height must be even and at least 64".into());
    }
    let (w, h) = (args.width, args.height);

    let mut source = Source::open(&args)?;

    let mut sink = None;
    if !args.no_virtualcam && args.snapshot.is_none() {
        match output::open(w, h, args.fps, args.device.as_deref()) {
            Ok(s) => {
                println!("Streaming to virtual camera: {}", s.name());
                println!("In Teams: Settings > Devices > Camera > pick that device.");
                sink = Some(s);
            }
            Err(e) => eprintln!("Could not start the virtual camera: {e}\nFalling back to a preview window.\n"),
        }
    }
    let want_preview = (args.preview || sink.is_none()) && args.snapshot.is_none();
    #[cfg(feature = "preview")]
    let mut preview = if want_preview { Some(Preview::new(w, h)?) } else { None };
    #[cfg(not(feature = "preview"))]
    if want_preview && sink.is_none() {
        return Err("no virtual camera and this build has no preview window".into());
    }

    let running = Arc::new(AtomicBool::new(true));
    let flag = running.clone();
    let _ = ctrlc::set_handler(move || flag.store(false, Ordering::SeqCst));
    println!("Press Ctrl+C to quit.");

    let mut effect = MatrixEffect::new(w, h, args.cell);
    let mut out = vec![0u8; w * h * 3];
    let frame_time = Duration::from_secs_f64(1.0 / args.fps.max(1) as f64);
    let paced = args.image.is_some(); // a still image has no camera clock to pace us
    let mut last = Instant::now();
    let (mut frames, mut misses) = (0u32, 0u32);

    while running.load(Ordering::SeqCst) {
        let (mut luma, sw, sh) = match source.frame() {
            Ok(f) => f,
            Err(e) => {
                misses += 1;
                if misses > 60 {
                    return Err(format!("lost the camera feed: {e}"));
                }
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        };
        misses = 0;
        frames += 1;
        if args.mirror {
            mirror(&mut luma, sw);
        }

        let now = Instant::now();
        let dt = if args.snapshot.is_some() { frame_time.as_secs_f32() } else { (now - last).as_secs_f32().min(0.25) };
        last = now;
        effect.render(&luma, sw, sh, dt, &mut out);

        if let Some(path) = &args.snapshot {
            if frames >= 60 {
                return image::save_buffer(path, &out, w as u32, h as u32, image::ColorType::Rgb8).map_err(|e| e.to_string());
            }
            continue;
        }
        if let Some(s) = sink.as_mut() {
            s.send(&out)?;
        }
        #[cfg(feature = "preview")]
        if let Some(p) = preview.as_mut() {
            if !p.show(&out, w, h) {
                break;
            }
        }
        if paced {
            std::thread::sleep(frame_time.saturating_sub(now.elapsed()));
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
