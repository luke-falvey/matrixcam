//! Media Foundation virtual camera source for matrixcam (Windows 11).
//!
//! Windows' camera service ("Frame Server") loads this DLL as an in-process COM
//! server when matrixcam.exe calls `MFCreateVirtualCamera`. It exposes one NV12
//! video stream whose frames are read from the file mapping that matrixcam.exe
//! writes (see the `matrixcam-shared` crate).
//!
//! The structure follows Microsoft's SimpleMediaSource virtual camera sample.
#![cfg(windows)]
#![allow(non_snake_case, clippy::not_unsafe_ptr_arg_deref)]

use matrixcam_shared::{frame_size, Shm, CLSID_U128};
use std::ffi::c_void;
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use windows::core::{implement, w, ComObject, Error, IUnknown, Interface, Ref, Result, BOOL, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CLASS_E_CLASSNOTAVAILABLE, CLASS_E_NOAGGREGATION, E_INVALIDARG, E_POINTER, E_UNEXPECTED, S_FALSE, S_OK,
};
use windows::Win32::Media::KernelStreaming::{IKsControl, IKsControl_Impl, KSCAMERAPROFILE_Legacy, KSIDENTIFIER, PINNAME_VIDEO_CAPTURE};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{IClassFactory, IClassFactory_Impl};

const CLSID_MATRIXCAM: GUID = GUID::from_u128(CLSID_U128);
const GUID_NULL: GUID = GUID::zeroed();
/// HRESULT_FROM_WIN32(ERROR_SET_NOT_FOUND): "this KS property set is not supported".
const E_SET_NOT_FOUND: HRESULT = HRESULT(0x8007_0492_u32 as i32);

// ---------------------------------------------------------------- logging --

/// Appends to %ProgramData%\matrixcam\vcam.log, but only if a file named
/// `debug` exists next to it. Used to diagnose problems inside the service.
fn log(msg: std::fmt::Arguments) {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    let dir = matrixcam_shared::data_dir();
    if !*ENABLED.get_or_init(|| dir.join("debug").exists()) {
        return;
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("vcam.log")) {
        let _ = writeln!(f, "[pid {}] {}", std::process::id(), msg);
    }
}

macro_rules! log {
    ($($arg:tt)*) => { log(format_args!($($arg)*)) };
}

/// Logs a failed result on its way out, so COM callers' errors are visible.
fn traced<T>(what: &str, r: Result<T>) -> Result<T> {
    if let Err(e) = &r {
        log!("{what} failed: {e}");
    }
    r
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn pack(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

// ------------------------------------------------------------ DLL exports --

#[no_mangle]
pub extern "system" fn DllGetClassObject(rclsid: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> HRESULT {
    if rclsid.is_null() || riid.is_null() || ppv.is_null() {
        return E_POINTER;
    }
    // SAFETY: pointers were checked for null; COM guarantees they are valid.
    unsafe {
        *ppv = std::ptr::null_mut();
        if *rclsid != CLSID_MATRIXCAM {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        log!("DllGetClassObject in {:?}", std::env::current_exe().unwrap_or_default());
        let factory: IClassFactory = ClassFactory.into();
        factory.query(riid, ppv)
    }
}

/// The DLL stays loaded for the life of the host process; that is simplest and
/// safe (the host is a long-lived service).
#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    S_FALSE
}

#[implement(IClassFactory)]
struct ClassFactory;

impl IClassFactory_Impl for ClassFactory_Impl {
    fn CreateInstance(&self, outer: Ref<IUnknown>, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if riid.is_null() || ppv.is_null() {
            return Err(E_POINTER.into());
        }
        if outer.is_some() {
            return Err(CLASS_E_NOAGGREGATION.into());
        }
        let activator: IMFActivate = traced("Activator::new", Activator::new())?.into();
        // SAFETY: riid/ppv checked above.
        unsafe { activator.query(riid, ppv).ok() }
    }

    fn LockServer(&self, _lock: BOOL) -> Result<()> {
        Ok(())
    }
}

// -------------------------------------------------------------- activator --

/// The object Frame Server asks for: an IMFActivate that hands out the source.
#[implement(IMFActivate)]
struct Activator {
    attrs: IMFAttributes,
    source: Mutex<Option<IMFMediaSourceEx>>,
}

impl Activator {
    fn new() -> Result<Self> {
        // SAFETY: plain Media Foundation calls with valid arguments.
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 4)?;
            let attrs = attrs.ok_or(Error::from(E_UNEXPECTED))?;
            attrs.SetUINT32(&MF_VIRTUALCAMERA_PROVIDE_ASSOCIATED_CAMERA_SOURCES, 1)?;
            attrs.SetGUID(&MFT_TRANSFORM_CLSID_Attribute, &CLSID_MATRIXCAM)?;
            Ok(Activator { attrs, source: Mutex::new(None) })
        }
    }
}

impl IMFActivate_Impl for Activator_Impl {
    fn ActivateObject(&self, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if riid.is_null() || ppv.is_null() {
            return Err(E_POINTER.into());
        }
        let mut source = lock(&self.source);
        if source.is_none() {
            *source = Some(traced("MediaSource::create", MediaSource::create(&self.attrs))?);
        }
        log!("ActivateObject");
        // SAFETY: riid/ppv checked above.
        unsafe { source.as_ref().unwrap().query(riid, ppv).ok() }
    }

    fn ShutdownObject(&self) -> Result<()> {
        Ok(())
    }

    fn DetachObject(&self) -> Result<()> {
        *lock(&self.source) = None;
        Ok(())
    }
}

/// IMFActivate inherits IMFAttributes; forward everything to a real store.
impl IMFAttributes_Impl for Activator_Impl {
    fn GetItem(&self, k: *const GUID, v: *mut PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.GetItem(k, (!v.is_null()).then_some(v)) }
    }
    fn GetItemType(&self, k: *const GUID) -> Result<MF_ATTRIBUTE_TYPE> {
        unsafe { self.attrs.GetItemType(k) }
    }
    fn CompareItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<BOOL> {
        unsafe { self.attrs.CompareItem(k, v) }
    }
    fn Compare(&self, theirs: Ref<IMFAttributes>, kind: MF_ATTRIBUTES_MATCH_TYPE) -> Result<BOOL> {
        unsafe { self.attrs.Compare(theirs.as_ref(), kind) }
    }
    fn GetUINT32(&self, k: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetUINT32(k) }
    }
    fn GetUINT64(&self, k: *const GUID) -> Result<u64> {
        unsafe { self.attrs.GetUINT64(k) }
    }
    fn GetDouble(&self, k: *const GUID) -> Result<f64> {
        unsafe { self.attrs.GetDouble(k) }
    }
    fn GetGUID(&self, k: *const GUID) -> Result<GUID> {
        unsafe { self.attrs.GetGUID(k) }
    }
    fn GetStringLength(&self, k: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetStringLength(k) }
    }
    fn GetString(&self, k: *const GUID, value: PWSTR, size: u32, len: *mut u32) -> Result<()> {
        unsafe { (Interface::vtable(&self.attrs).GetString)(Interface::as_raw(&self.attrs), k, value, size, len).ok() }
    }
    fn GetAllocatedString(&self, k: *const GUID, value: *mut PWSTR, len: *mut u32) -> Result<()> {
        unsafe { self.attrs.GetAllocatedString(k, value, len) }
    }
    fn GetBlobSize(&self, k: *const GUID) -> Result<u32> {
        unsafe { self.attrs.GetBlobSize(k) }
    }
    fn GetBlob(&self, k: *const GUID, buf: *mut u8, size: u32, len: *mut u32) -> Result<()> {
        unsafe { (Interface::vtable(&self.attrs).GetBlob)(Interface::as_raw(&self.attrs), k, buf, size, len).ok() }
    }
    fn GetAllocatedBlob(&self, k: *const GUID, buf: *mut *mut u8, size: *mut u32) -> Result<()> {
        unsafe { self.attrs.GetAllocatedBlob(k, buf, size) }
    }
    fn GetUnknown(&self, k: *const GUID, riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        unsafe { (Interface::vtable(&self.attrs).GetUnknown)(Interface::as_raw(&self.attrs), k, riid, ppv).ok() }
    }
    fn SetItem(&self, k: *const GUID, v: *const PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.SetItem(k, v) }
    }
    fn DeleteItem(&self, k: *const GUID) -> Result<()> {
        unsafe { self.attrs.DeleteItem(k) }
    }
    fn DeleteAllItems(&self) -> Result<()> {
        unsafe { self.attrs.DeleteAllItems() }
    }
    fn SetUINT32(&self, k: *const GUID, v: u32) -> Result<()> {
        unsafe { self.attrs.SetUINT32(k, v) }
    }
    fn SetUINT64(&self, k: *const GUID, v: u64) -> Result<()> {
        unsafe { self.attrs.SetUINT64(k, v) }
    }
    fn SetDouble(&self, k: *const GUID, v: f64) -> Result<()> {
        unsafe { self.attrs.SetDouble(k, v) }
    }
    fn SetGUID(&self, k: *const GUID, v: *const GUID) -> Result<()> {
        unsafe { self.attrs.SetGUID(k, v) }
    }
    fn SetString(&self, k: *const GUID, v: &PCWSTR) -> Result<()> {
        unsafe { self.attrs.SetString(k, *v) }
    }
    fn SetBlob(&self, k: *const GUID, buf: *const u8, size: u32) -> Result<()> {
        unsafe { (Interface::vtable(&self.attrs).SetBlob)(Interface::as_raw(&self.attrs), k, buf, size).ok() }
    }
    fn SetUnknown(&self, k: *const GUID, v: Ref<IUnknown>) -> Result<()> {
        unsafe { self.attrs.SetUnknown(k, v.as_ref()) }
    }
    fn LockStore(&self) -> Result<()> {
        unsafe { self.attrs.LockStore() }
    }
    fn UnlockStore(&self) -> Result<()> {
        unsafe { self.attrs.UnlockStore() }
    }
    fn GetCount(&self) -> Result<u32> {
        unsafe { self.attrs.GetCount() }
    }
    fn GetItemByIndex(&self, i: u32, k: *mut GUID, v: *mut PROPVARIANT) -> Result<()> {
        unsafe { self.attrs.GetItemByIndex(i, k, (!v.is_null()).then_some(v)) }
    }
    fn CopyAllItems(&self, dest: Ref<IMFAttributes>) -> Result<()> {
        unsafe { self.attrs.CopyAllItems(dest.as_ref()) }
    }
}

// ----------------------------------------------------------- media source --

struct SourceInner {
    queue: Option<IMFMediaEventQueue>,
    descriptor: Option<IMFPresentationDescriptor>,
    stream: Option<ComObject<MediaStream>>,
    attrs: Option<IMFAttributes>,
}

#[implement(IMFMediaSourceEx, IMFGetService, IKsControl, IMFSampleAllocatorControl)]
struct MediaSource {
    inner: Mutex<SourceInner>,
}

impl MediaSource {
    fn create(activator_attrs: &IMFAttributes) -> Result<IMFMediaSourceEx> {
        // SAFETY: plain Media Foundation calls with valid arguments.
        unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 8)?;
            let attrs = attrs.ok_or(Error::from(E_UNEXPECTED))?;
            activator_attrs.CopyAllItems(&attrs)?;

            // Frame Server requires a sensor profile collection on camera sources.
            let profiles = MFCreateSensorProfileCollection()?;
            let profile = MFCreateSensorProfile(&KSCAMERAPROFILE_Legacy, 0, PCWSTR::null())?;
            profile.AddProfileFilter(0, w!("((RES==;FRT<=30,1;SUT==))"))?;
            profiles.AddProfile(&profile)?;
            attrs.SetUnknown(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, &profiles)?;

            let source = ComObject::new(MediaSource {
                inner: Mutex::new(SourceInner {
                    queue: Some(MFCreateEventQueue()?),
                    descriptor: None,
                    stream: None,
                    attrs: Some(attrs),
                }),
            });
            let source_ex: IMFMediaSourceEx = source.to_interface();
            let stream = MediaStream::create(source_ex.clone().into())?;
            let stream_desc = lock(&stream.inner).descriptor.clone();
            let descriptor = MFCreatePresentationDescriptor(Some(&[stream_desc]))?;
            {
                let mut inner = lock(&source.inner);
                inner.descriptor = Some(descriptor);
                inner.stream = Some(stream);
            }
            Ok(source_ex)
        }
    }

    fn queue(&self) -> Result<IMFMediaEventQueue> {
        lock(&self.inner).queue.clone().ok_or(MF_E_SHUTDOWN.into())
    }
}

impl IMFMediaEventGenerator_Impl for MediaSource_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        // The queue is fetched first so the (possibly blocking) call runs unlocked.
        unsafe { self.queue()?.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        unsafe { self.queue()?.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.queue()?.EndGetEvent(result.as_ref()) }
    }
    fn QueueEvent(&self, met: u32, ext: *const GUID, status: HRESULT, value: *const PROPVARIANT) -> Result<()> {
        unsafe { self.queue()?.QueueEventParamVar(met, ext, status, value) }
    }
}

impl IMFMediaSource_Impl for MediaSource_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        let inner = lock(&self.inner);
        unsafe { inner.descriptor.as_ref().ok_or(Error::from(MF_E_SHUTDOWN))?.Clone() }
    }

    fn Start(&self, descriptor: Ref<IMFPresentationDescriptor>, time_format: *const GUID, start: *const PROPVARIANT) -> Result<()> {
        let requested = descriptor.as_ref().ok_or(Error::from(E_INVALIDARG))?;
        if start.is_null() {
            return Err(E_INVALIDARG.into());
        }
        // SAFETY: time_format is null or a valid GUID per the COM contract.
        if !time_format.is_null() && unsafe { *time_format } != GUID_NULL {
            return Err(MF_E_UNSUPPORTED_TIME_FORMAT.into());
        }
        let inner = lock(&self.inner);
        let (Some(queue), Some(ours), Some(stream)) = (&inner.queue, &inner.descriptor, &inner.stream) else {
            return Err(MF_E_SHUTDOWN.into());
        };
        log!("Source::Start");
        traced("Source::Start", unsafe {
            (|| -> Result<()> {
                let mut selected = BOOL(0);
                let mut requested_desc = None;
                requested.GetStreamDescriptorByIndex(0, &mut selected, &mut requested_desc)?;
                let requested_desc = requested_desc.ok_or(Error::from(E_UNEXPECTED))?;
                let mut was_selected = BOOL(0);
                let mut our_desc = None;
                ours.GetStreamDescriptorByIndex(0, &mut was_selected, &mut our_desc)?;

                if selected.as_bool() {
                    ours.SelectStream(0)?;
                    let media_type = requested_desc.GetMediaTypeHandler()?.GetCurrentMediaType()?;
                    let event = if was_selected.as_bool() { MEUpdatedStream } else { MENewStream };
                    let unknown: IUnknown = stream.to_interface();
                    queue.QueueEventParamUnk(event.0 as u32, &GUID_NULL, S_OK, &unknown)?;
                    stream.start(Some(&media_type), true)?;
                } else if was_selected.as_bool() {
                    ours.DeselectStream(0)?;
                    stream.stop(false)?;
                }
                let now = PROPVARIANT::from(MFGetSystemTime());
                queue.QueueEventParamVar(MESourceStarted.0 as u32, &GUID_NULL, S_OK, &now)
            })()
        })
    }

    fn Stop(&self) -> Result<()> {
        let inner = lock(&self.inner);
        let (Some(queue), Some(ours), Some(stream)) = (&inner.queue, &inner.descriptor, &inner.stream) else {
            return Err(MF_E_SHUTDOWN.into());
        };
        log!("Source::Stop");
        unsafe {
            stream.stop(true)?;
            ours.DeselectStream(0)?;
            let now = PROPVARIANT::from(MFGetSystemTime());
            queue.QueueEventParamVar(MESourceStopped.0 as u32, &GUID_NULL, S_OK, &now)
        }
    }

    fn Pause(&self) -> Result<()> {
        Err(MF_E_INVALID_STATE_TRANSITION.into())
    }

    fn Shutdown(&self) -> Result<()> {
        let mut inner = lock(&self.inner);
        log!("Source::Shutdown");
        let queue = inner.queue.take().ok_or(Error::from(MF_E_SHUTDOWN))?;
        unsafe {
            let _ = queue.Shutdown();
        }
        if let Some(stream) = inner.stream.take() {
            stream.shutdown();
        }
        inner.descriptor = None;
        inner.attrs = None;
        Ok(())
    }
}

impl IMFMediaSourceEx_Impl for MediaSource_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        lock(&self.inner).attrs.clone().ok_or(MF_E_SHUTDOWN.into())
    }

    fn GetStreamAttributes(&self, stream_id: u32) -> Result<IMFAttributes> {
        if stream_id != 0 {
            return Err(MF_E_NOT_FOUND.into());
        }
        let inner = lock(&self.inner);
        Ok(inner.stream.as_ref().ok_or(Error::from(MF_E_SHUTDOWN))?.attrs.clone())
    }

    fn SetD3DManager(&self, _manager: Ref<IUnknown>) -> Result<()> {
        Ok(()) // frames are produced on the CPU
    }
}

impl IMFGetService_Impl for MediaSource_Impl {
    fn GetService(&self, _service: *const GUID, _riid: *const GUID, ppv: *mut *mut c_void) -> Result<()> {
        if !ppv.is_null() {
            // SAFETY: checked for null.
            unsafe { *ppv = std::ptr::null_mut() };
        }
        Err(MF_E_UNSUPPORTED_SERVICE.into())
    }
}

impl IMFSampleAllocatorControl_Impl for MediaSource_Impl {
    fn SetDefaultAllocator(&self, stream_id: u32, allocator: Ref<IUnknown>) -> Result<()> {
        if stream_id != 0 {
            return Err(MF_E_NOT_FOUND.into());
        }
        let allocator: IMFVideoSampleAllocator = allocator.as_ref().ok_or(Error::from(E_POINTER))?.cast()?;
        let inner = lock(&self.inner);
        let stream = inner.stream.as_ref().ok_or(Error::from(MF_E_SHUTDOWN))?;
        lock(&stream.inner).allocator = Some(allocator);
        Ok(())
    }

    fn GetAllocatorUsage(&self, stream_id: u32, input_id: *mut u32, usage: *mut MFSampleAllocatorUsage) -> Result<()> {
        if input_id.is_null() || usage.is_null() {
            return Err(E_POINTER.into());
        }
        // SAFETY: checked for null.
        unsafe {
            *input_id = stream_id;
            *usage = MFSampleAllocatorUsage_UsesProvidedAllocator;
        }
        Ok(())
    }
}

/// The camera exposes no driver properties (brightness, zoom, ...).
impl IKsControl_Impl for MediaSource_Impl {
    fn KsProperty(&self, _p: *const KSIDENTIFIER, _pl: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(E_SET_NOT_FOUND.into())
    }
    fn KsMethod(&self, _m: *const KSIDENTIFIER, _ml: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(E_SET_NOT_FOUND.into())
    }
    fn KsEvent(&self, _e: *const KSIDENTIFIER, _el: u32, _d: *mut c_void, _dl: u32, _r: *mut u32) -> Result<()> {
        Err(E_SET_NOT_FOUND.into())
    }
}

// ----------------------------------------------------------- media stream --

struct StreamInner {
    queue: Option<IMFMediaEventQueue>,
    descriptor: Option<IMFStreamDescriptor>,
    parent: Option<IMFMediaSource>,
    allocator: Option<IMFVideoSampleAllocator>,
    media_type: Option<IMFMediaType>,
    state: MF_STREAM_STATE,
    shm: Option<Shm>,
    last_seq: u32,
    last_sent: Instant,
    last_open_attempt: Instant,
}

#[implement(IMFMediaStream2)]
struct MediaStream {
    inner: Mutex<StreamInner>,
    attrs: IMFAttributes,
    width: usize,
    height: usize,
    fps: u32,
}

impl MediaStream {
    fn create(parent: IMFMediaSource) -> Result<ComObject<MediaStream>> {
        // The running matrixcam.exe decides the format; fall back to 720p30.
        let shm = Shm::open().ok();
        let (width, height, fps) = shm.as_ref().map_or((1280, 720, 30), |s| (s.width, s.height, s.fps.clamp(1, 120)));
        log!("Stream::create {width}x{height}@{fps} (frame file open: {})", shm.is_some());

        // SAFETY: plain Media Foundation calls with valid arguments.
        unsafe {
            let media_type = MFCreateMediaType()?;
            media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            media_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            media_type.SetUINT64(&MF_MT_FRAME_SIZE, pack(width as u32, height as u32))?;
            media_type.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            media_type.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            media_type.SetUINT32(&MF_MT_AVG_BITRATE, (frame_size(width, height) * 8 * fps as usize) as u32)?;

            let set_stream_attributes = |a: &IMFAttributes| -> Result<()> {
                a.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
                a.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
                a.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
                a.SetUINT32(&MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MFFrameSourceTypes_Color.0 as u32)
            };
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 8)?;
            let attrs = attrs.ok_or(Error::from(E_UNEXPECTED))?;
            set_stream_attributes(&attrs)?;

            let descriptor = MFCreateStreamDescriptor(0, &[Some(media_type.clone())])?;
            descriptor.GetMediaTypeHandler()?.SetCurrentMediaType(&media_type)?;
            set_stream_attributes(&descriptor.cast()?)?;

            let now = Instant::now();
            Ok(ComObject::new(MediaStream {
                inner: Mutex::new(StreamInner {
                    queue: Some(MFCreateEventQueue()?),
                    descriptor: Some(descriptor),
                    parent: Some(parent),
                    allocator: None,
                    media_type: Some(media_type),
                    state: MF_STREAM_STATE_STOPPED,
                    shm,
                    last_seq: 0,
                    last_sent: now,
                    last_open_attempt: now,
                }),
                attrs,
                width,
                height,
                fps,
            }))
        }
    }

    fn queue(&self) -> Result<IMFMediaEventQueue> {
        lock(&self.inner).queue.clone().ok_or(MF_E_SHUTDOWN.into())
    }

    fn start(&self, media_type: Option<&IMFMediaType>, send_event: bool) -> Result<()> {
        let mut inner = lock(&self.inner);
        self.start_locked(&mut inner, media_type, send_event)
    }

    fn start_locked(&self, inner: &mut StreamInner, media_type: Option<&IMFMediaType>, send_event: bool) -> Result<()> {
        let queue = inner.queue.clone().ok_or(Error::from(MF_E_SHUTDOWN))?;
        if let Some(t) = media_type {
            inner.media_type = Some(t.clone());
        }
        log!("Stream::start (allocator provided: {})", inner.allocator.is_some());
        // SAFETY: plain Media Foundation calls with valid arguments.
        unsafe {
            if inner.allocator.is_none() {
                let mut raw = std::ptr::null_mut();
                MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocator::IID, &mut raw)?;
                inner.allocator = Some(IMFVideoSampleAllocator::from_raw(raw));
            }
            if inner.state != MF_STREAM_STATE_RUNNING {
                let allocator = inner.allocator.as_ref().unwrap();
                let _ = allocator.UninitializeSampleAllocator();
                allocator.InitializeSampleAllocator(10, inner.media_type.as_ref())?;
            }
            if send_event {
                queue.QueueEventParamVar(MEStreamStarted.0 as u32, &GUID_NULL, S_OK, std::ptr::null())?;
            }
        }
        inner.state = MF_STREAM_STATE_RUNNING;
        Ok(())
    }

    fn stop(&self, send_event: bool) -> Result<()> {
        let mut inner = lock(&self.inner);
        self.stop_locked(&mut inner, send_event)
    }

    fn stop_locked(&self, inner: &mut StreamInner, send_event: bool) -> Result<()> {
        let queue = inner.queue.clone().ok_or(Error::from(MF_E_SHUTDOWN))?;
        log!("Stream::stop");
        inner.state = MF_STREAM_STATE_STOPPED;
        // SAFETY: plain Media Foundation calls with valid arguments.
        unsafe {
            if let Some(allocator) = &inner.allocator {
                let _ = allocator.UninitializeSampleAllocator();
            }
            if send_event {
                queue.QueueEventParamVar(MEStreamStopped.0 as u32, &GUID_NULL, S_OK, std::ptr::null())?;
            }
        }
        Ok(())
    }

    fn shutdown(&self) {
        let mut inner = lock(&self.inner);
        if let Some(queue) = inner.queue.take() {
            // SAFETY: valid interface pointer.
            unsafe {
                let _ = queue.Shutdown();
            }
        }
        inner.descriptor = None;
        inner.parent = None; // breaks the source <-> stream reference cycle
        inner.allocator = None;
        inner.state = MF_STREAM_STATE_STOPPED;
    }

    /// Blocks briefly so samples leave at the camera's frame rate, preferring to
    /// hand out a frame as soon as matrixcam.exe publishes a new one.
    fn wait_for_frame(&self, inner: &mut StreamInner) {
        let interval = Duration::from_secs_f64(1.0 / self.fps as f64);
        if inner.shm.is_none() && inner.last_open_attempt.elapsed() > Duration::from_secs(1) {
            inner.last_open_attempt = Instant::now();
            inner.shm = Shm::open().ok().filter(|s| s.width == self.width && s.height == self.height);
        }
        loop {
            let since = inner.last_sent.elapsed();
            let fresh = inner.shm.as_ref().is_some_and(|s| s.seq() != inner.last_seq);
            if (fresh && since >= interval / 2) || since >= interval * 2 || (inner.shm.is_none() && since >= interval) {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        inner.last_sent = Instant::now();
    }

    /// Copies the latest frame (or black, if matrixcam.exe is not running) into `buffer`.
    fn fill(&self, inner: &mut StreamInner, buffer: &IMFMediaBuffer) -> Result<()> {
        let (w, h) = (self.width, self.height);
        let frame = inner.shm.as_ref().filter(|s| s.width == w && s.height == h).map(|s| {
            inner.last_seq = s.seq();
            s.latest()
        });
        // Writes `rows` rows of `w` bytes each to dst, honouring the destination pitch.
        let copy_plane = |dst: *mut u8, pitch: isize, rows: usize, src: Option<&[u8]>, blank: u8| {
            for y in 0..rows {
                // SAFETY: callers pass a destination with at least `rows` rows of `pitch` bytes.
                unsafe {
                    let line = std::slice::from_raw_parts_mut(dst.offset(y as isize * pitch), w);
                    match src {
                        Some(s) => line.copy_from_slice(&s[y * w..(y + 1) * w]),
                        None => line.fill(blank),
                    }
                }
            }
        };
        let (luma, chroma) = match frame {
            Some(f) => (Some(&f[..w * h]), Some(&f[w * h..])),
            None => (None, None),
        };
        // SAFETY: buffers are locked before being written and unlocked afterwards;
        // sizes are validated against what Media Foundation reports.
        unsafe {
            if let Ok(buffer2d) = buffer.cast::<IMF2DBuffer2>() {
                let (mut scan0, mut pitch, mut start, mut len) = (std::ptr::null_mut(), 0i32, std::ptr::null_mut(), 0u32);
                buffer2d.Lock2DSize(MF2DBuffer_LockFlags_Write, &mut scan0, &mut pitch, &mut start, &mut len)?;
                let pitch_bytes = pitch as isize;
                let ok = pitch_bytes >= w as isize && len as usize >= pitch as usize * h * 3 / 2;
                if ok {
                    copy_plane(scan0, pitch_bytes, h, luma, 16);
                    copy_plane(scan0.offset(pitch_bytes * h as isize), pitch_bytes, h / 2, chroma, 128);
                }
                buffer2d.Unlock2D()?;
                return if ok { Ok(()) } else { Err(E_UNEXPECTED.into()) };
            }
            let (mut data, mut max_len) = (std::ptr::null_mut(), 0u32);
            buffer.Lock(&mut data, Some(&mut max_len), None)?;
            let ok = max_len as usize >= frame_size(w, h);
            if ok {
                copy_plane(data, w as isize, h, luma, 16);
                copy_plane(data.add(w * h), w as isize, h / 2, chroma, 128);
            }
            buffer.Unlock()?;
            if !ok {
                return Err(E_UNEXPECTED.into());
            }
            buffer.SetCurrentLength(frame_size(w, h) as u32)
        }
    }
}

impl IMFMediaEventGenerator_Impl for MediaStream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        unsafe { self.queue()?.GetEvent(flags.0) }
    }
    fn BeginGetEvent(&self, callback: Ref<IMFAsyncCallback>, state: Ref<IUnknown>) -> Result<()> {
        unsafe { self.queue()?.BeginGetEvent(callback.as_ref(), state.as_ref()) }
    }
    fn EndGetEvent(&self, result: Ref<IMFAsyncResult>) -> Result<IMFMediaEvent> {
        unsafe { self.queue()?.EndGetEvent(result.as_ref()) }
    }
    fn QueueEvent(&self, met: u32, ext: *const GUID, status: HRESULT, value: *const PROPVARIANT) -> Result<()> {
        unsafe { self.queue()?.QueueEventParamVar(met, ext, status, value) }
    }
}

impl IMFMediaStream_Impl for MediaStream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        lock(&self.inner).parent.clone().ok_or(MF_E_SHUTDOWN.into())
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        lock(&self.inner).descriptor.clone().ok_or(MF_E_SHUTDOWN.into())
    }

    fn RequestSample(&self, token: Ref<IUnknown>) -> Result<()> {
        let mut inner = lock(&self.inner);
        let queue = inner.queue.clone().ok_or(Error::from(MF_E_SHUTDOWN))?;
        if inner.state != MF_STREAM_STATE_RUNNING {
            return Err(MF_E_INVALIDREQUEST.into());
        }
        let allocator = inner.allocator.clone().ok_or(Error::from(E_UNEXPECTED))?;
        self.wait_for_frame(&mut inner);
        traced("RequestSample", unsafe {
            (|| -> Result<()> {
                let sample = allocator.AllocateSample()?;
                self.fill(&mut inner, &sample.GetBufferByIndex(0)?)?;
                sample.SetSampleTime(MFGetSystemTime())?;
                sample.SetSampleDuration(10_000_000 / self.fps as i64)?;
                if let Some(token) = token.as_ref() {
                    sample.SetUnknown(&MFSampleExtension_Token, token)?;
                }
                queue.QueueEventParamUnk(MEMediaSample.0 as u32, &GUID_NULL, S_OK, &sample)
            })()
        })
    }
}

impl IMFMediaStream2_Impl for MediaStream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        let mut inner = lock(&self.inner);
        log!("Stream::SetStreamState {} -> {}", inner.state.0, value.0);
        if inner.state == value {
            return Ok(());
        }
        match value {
            MF_STREAM_STATE_PAUSED if inner.state == MF_STREAM_STATE_RUNNING => {
                inner.state = MF_STREAM_STATE_PAUSED;
                Ok(())
            }
            MF_STREAM_STATE_RUNNING => self.start_locked(&mut inner, None, false),
            MF_STREAM_STATE_STOPPED => self.stop_locked(&mut inner, false),
            _ => Err(MF_E_INVALID_STATE_TRANSITION.into()),
        }
    }

    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        Ok(lock(&self.inner).state)
    }
}
