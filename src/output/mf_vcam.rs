//! Windows 11: a native virtual camera via Media Foundation's
//! `MFCreateVirtualCamera`. No third-party driver is needed, but Windows loads
//! the camera's media source (our embedded DLL) inside its camera service, so
//! the DLL must be registered machine-wide once, which needs administrator rights.
use super::Sink;
use matrixcam_shared::{data_dir, rgb_to_nv12, Shm, CAMERA_NAME, CLSID_STR};
use std::ffi::c_void;
use std::path::PathBuf;
use std::process::Command;
use windows::core::{s, w, Interface, GUID, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Media::MediaFoundation::{
    IMFVirtualCamera, MFShutdown, MFStartup, MFVirtualCameraAccess_CurrentUser, MFVirtualCameraLifetime_Session,
    MFVirtualCameraType_SoftwareCameraSource, MFSTARTUP_FULL, MF_VERSION,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};

const DLL: &[u8] = include_bytes!(env!("MATRIXCAM_VCAM_DLL"));
const DLL_NAME: &str = "matrixcam_vcam.dll";

fn reg_key() -> String {
    format!("HKLM\\SOFTWARE\\Classes\\CLSID\\{CLSID_STR}")
}

/// The DLL lives under Program Files: readable by the camera service, and not
/// writable by ordinary users (it is loaded into a privileged service).
fn install_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into())).join("matrixcam")
}

fn run(program: &str, args: &[&str]) -> bool {
    Command::new(program).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

fn is_installed() -> bool {
    let dll = install_dir().join(DLL_NAME);
    std::fs::read(&dll).is_ok_and(|current| current == DLL)
        && Command::new("reg")
            .args(["query", &format!("{}\\InprocServer32", reg_key()), "/ve"])
            .output()
            .is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains(DLL_NAME))
}

/// Copies the DLL into place and registers it. Requires administrator rights.
pub fn install_driver() -> Result<(), String> {
    let dir = install_dir();
    let dll = dir.join(DLL_NAME);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    if std::fs::read(&dll).map_or(true, |current| current != DLL) {
        if std::fs::write(&dll, DLL).is_err() {
            // A loaded DLL cannot be overwritten, but it can be renamed out of the way.
            let _ = std::fs::rename(&dll, dir.join(format!("old-{}.dll", std::process::id())));
            std::fs::write(&dll, DLL).map_err(|e| format!("cannot write {}: {e}", dll.display()))?;
        }
    }
    let key = format!("{}\\InprocServer32", reg_key());
    let dll_str = dll.to_string_lossy();
    if !run("reg", &["add", &key, "/ve", "/d", &dll_str, "/f"])
        || !run("reg", &["add", &key, "/v", "ThreadingModel", "/d", "Both", "/f"])
    {
        return Err("could not write the registry (administrator rights are required)".into());
    }
    // Frames are handed to the camera service through a file here; let any user write it.
    let data = data_dir();
    std::fs::create_dir_all(&data).map_err(|e| format!("cannot create {}: {e}", data.display()))?;
    run("icacls", &[&data.to_string_lossy(), "/grant", "*S-1-5-32-545:(OI)(CI)M"]);
    Ok(())
}

pub fn uninstall_driver() -> Result<(), String> {
    if !run("reg", &["delete", &reg_key(), "/f"]) && run("reg", &["query", &reg_key()]) {
        return Err("could not remove the registry entry (administrator rights are required)".into());
    }
    let _ = std::fs::remove_dir_all(install_dir());
    let _ = std::fs::remove_dir_all(data_dir());
    Ok(())
}

/// Re-runs this exe elevated (UAC prompt) with one argument and waits for it.
pub fn run_elevated(arg: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let script = format!(
        "$p = Start-Process -FilePath '{}' -ArgumentList '{arg}' -Verb RunAs -Wait -PassThru; exit $p.ExitCode",
        exe.to_string_lossy().replace('\'', "''")
    );
    if run("powershell", &["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", &script]) {
        Ok(())
    } else {
        Err("the administrator step was cancelled or failed".into())
    }
}

fn ensure_installed() -> Result<(), String> {
    if is_installed() {
        return Ok(());
    }
    if install_driver().is_ok() && is_installed() {
        return Ok(()); // we were already running as administrator
    }
    println!("Installing the {CAMERA_NAME} camera component (one-time; Windows will ask for administrator approval)...");
    run_elevated("--install-driver")?;
    if is_installed() {
        Ok(())
    } else {
        Err("the camera component is still not installed".into())
    }
}

type CreateVirtualCamera = unsafe extern "system" fn(
    kind: i32,
    lifetime: i32,
    access: i32,
    friendly_name: PCWSTR,
    source_id: PCWSTR,
    categories: *const GUID,
    category_count: u32,
    camera: *mut *mut c_void,
) -> HRESULT;

struct MfSink {
    camera: IMFVirtualCamera,
    shm: Shm,
}

pub fn open(width: usize, height: usize, fps: u32) -> Result<Box<dyn Sink>, String> {
    // SAFETY: Win32 / Media Foundation calls with valid arguments; every result is checked.
    unsafe {
        // Looked up at run time so the exe still starts (with a clear message) on Windows 10.
        let create = LoadLibraryW(w!("mfsensorgroup.dll"))
            .ok()
            .and_then(|lib| GetProcAddress(lib, s!("MFCreateVirtualCamera")))
            .ok_or("this needs Windows 11 (the built-in virtual camera API is missing)")?;
        let create: CreateVirtualCamera = std::mem::transmute(create);

        ensure_installed()?;
        // The camera reads its format from this file, so it must exist before the camera does.
        let shm = Shm::create(width, height, fps)
            .map_err(|e| format!("cannot create the frame file ({e}); close apps that are using {CAMERA_NAME} and retry"))?;

        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_FULL).map_err(|e| format!("Media Foundation failed to start: {e}"))?;

        let mut raw = std::ptr::null_mut();
        create(
            MFVirtualCameraType_SoftwareCameraSource.0,
            MFVirtualCameraLifetime_Session.0,
            MFVirtualCameraAccess_CurrentUser.0,
            PCWSTR(HSTRING::from(CAMERA_NAME).as_ptr()),
            PCWSTR(HSTRING::from(CLSID_STR).as_ptr()),
            std::ptr::null(),
            0,
            &mut raw,
        )
        .ok()
        .map_err(|e| format!("Windows refused to create the virtual camera: {e}"))?;
        let camera = IMFVirtualCamera::from_raw(raw);
        camera.Start(None::<&windows::Win32::Media::MediaFoundation::IMFAsyncCallback>).map_err(|e| {
            format!(
                "the virtual camera did not start: {e}\n\
                 (Is the Windows camera turned off in Settings > Privacy & security > Camera?)"
            )
        })?;
        Ok(Box::new(MfSink { camera, shm }))
    }
}

impl Sink for MfSink {
    fn name(&self) -> String {
        CAMERA_NAME.into()
    }

    fn send(&mut self, rgb: &[u8]) -> Result<(), String> {
        let (w, h) = (self.shm.width, self.shm.height);
        self.shm.write_frame(|slot| rgb_to_nv12(rgb, w, h, slot));
        Ok(())
    }
}

impl Drop for MfSink {
    fn drop(&mut self) {
        // SAFETY: valid interface; Remove deletes the session-lifetime camera device.
        unsafe {
            let _ = self.camera.Remove();
            let _ = MFShutdown();
        }
    }
}
