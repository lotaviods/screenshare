//! WASAPI audio capture: microphone and process-loopback system audio.
//!
//! Both sources are converted to 48 kHz stereo S16LE by WASAPI's
//! `AUTOCONVERTPCM` and handed to the FFmpeg Opus encoder. System audio uses
//! process loopback (Windows 10 build 20348 or later) and excludes the
//! Sharescreen process tree, so the call's own audio is never captured.

use std::sync::mpsc;

use windows::core::{
    implement, IUnknown, IUnknownImpl, Interface, Ref, Result as WinResult, HSTRING,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, ActivateAudioInterfaceAsync, EDataFlow,
    IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler,
    IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient,
    IAudioRenderClient, IMMDevice, IMMDeviceCollection, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_LOOPBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
    AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, DEVICE_STATE_ACTIVE,
    PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX, WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::StructuredStorage::{PropVariantClear, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};
use windows::Win32::System::Variant::{VT_BLOB, VT_LPWSTR};

use sharescreen_core::{Device, DeviceKind};

/// Capture device names, in the device picker's order.
pub fn microphones() -> Vec<Device> {
    list(eCapture, DeviceKind::Microphone).unwrap_or_default()
}

/// Render endpoints, also used as system-audio sources.
pub fn outputs() -> Vec<Device> {
    list(eRender, DeviceKind::AudioOutput).unwrap_or_default()
}

/// A live capture stream owned by one thread.
pub(crate) struct Recorder {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    pending: Vec<i16>,
    _com: Com,
}

impl Recorder {
    /// Open a capture endpoint (default when `device` is `None`).
    pub(crate) fn open_mic(device: Option<String>) -> Result<Self, String> {
        let com = Com::init();
        let device = resolve_device(device)?;
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| e.to_string())?;
        Self::initialize(client, false, com)
    }

    /// Open process loopback for the default output device.
    pub(crate) fn open_system(_device: Option<String>) -> Result<Self, String> {
        let com = Com::init();
        let client = activate_process_loopback()?;
        Self::initialize(client, true, com)
    }

    fn initialize(client: IAudioClient, loopback: bool, com: Com) -> Result<Self, String> {
        let format = wave_format();
        let mut flags =
            AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        if loopback {
            flags |= AUDCLNT_STREAMFLAGS_LOOPBACK;
        }
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    flags,
                    0,
                    0,
                    &format as *const WAVEFORMATEX,
                    None,
                )
                .map_err(|e| e.to_string())?;
            let capture: IAudioCaptureClient = client.GetService().map_err(|e| e.to_string())?;
            client.Start().map_err(|e| e.to_string())?;
            Ok(Self {
                client,
                capture,
                pending: Vec::new(),
                _com: com,
            })
        }
    }

    /// Drain the next capture packet(s) into `pending`.
    pub(crate) fn pump(&mut self) {
        unsafe {
            loop {
                let next = match self.capture.GetNextPacketSize() {
                    Ok(next) => next,
                    Err(_) => break,
                };
                if next == 0 {
                    break;
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames: u32 = 0;
                let mut flags: u32 = 0;
                if self
                    .capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    break;
                }

                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                if silent || data.is_null() {
                    self.pending
                        .extend(std::iter::repeat_n(0i16, frames as usize * 2));
                } else {
                    let samples =
                        std::slice::from_raw_parts(data as *const i16, frames as usize * 2);
                    self.pending.extend_from_slice(samples);
                }
                let _ = self.capture.ReleaseBuffer(frames);
            }
        }
    }

    /// Take every pending interleaved stereo sample.
    pub(crate) fn take(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.pending)
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// Plays remote audio on a WASAPI render endpoint.
pub(crate) struct Playback {
    client: IAudioClient,
    render: IAudioRenderClient,
    buffer_size: u32,
    _com: Com,
}

impl Playback {
    /// Open a playback endpoint (default when `device` is `None`).
    pub(crate) fn open(device: Option<String>) -> Result<Self, String> {
        let com = Com::init();
        let device = resolve_device(device)?;
        let client: IAudioClient =
            unsafe { device.Activate(CLSCTX_ALL, None) }.map_err(|e| e.to_string())?;
        let format = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_PCM as u16,
            nChannels: 2,
            nSamplesPerSec: 48_000,
            nAvgBytesPerSec: 48_000 * 2 * 2,
            nBlockAlign: 2 * 2,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        unsafe {
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    0,
                    0,
                    &format,
                    None,
                )
                .map_err(|e| e.to_string())?;
            let render: IAudioRenderClient = client.GetService().map_err(|e| e.to_string())?;
            let buffer_size = client.GetBufferSize().map_err(|e| e.to_string())?;
            client.Start().map_err(|e| e.to_string())?;
            Ok(Self {
                client,
                render,
                buffer_size,
                _com: com,
            })
        }
    }

    /// Write interleaved 16-bit stereo samples. Drops what does not fit.
    pub(crate) fn play(&mut self, samples: &[i16]) {
        let bytes = samples
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect::<Vec<u8>>();
        unsafe {
            let padding = self.client.GetCurrentPadding().unwrap_or(0);
            let available = self.buffer_size.saturating_sub(padding);
            if available == 0 {
                return;
            }
            let Ok(buffer) = self.render.GetBuffer(available) else {
                return;
            };
            let slice = std::slice::from_raw_parts_mut(buffer as *mut u8, available as usize * 4);
            let len = bytes.len().min(slice.len());
            slice[..len].copy_from_slice(&bytes[..len]);
            let _ = self.render.ReleaseBuffer(available, 0);
        }
    }

    pub(crate) fn stop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

/// COM apartment guard for the thread that owns the capture.
struct Com {
    initialized: bool,
}

impl Com {
    fn init() -> Self {
        // `RPC_E_CHANGED_MODE` means the thread already initialized COM in a
        // different apartment; only balance a call this guard made.
        let initialized = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).is_ok() };
        Self { initialized }
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

/// Enumerate active endpoints of one data flow.
fn list(data_flow: EDataFlow, kind: DeviceKind) -> Result<Vec<Device>, String> {
    let _com = Com::init();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(|e| e.to_string())?;
        let collection: IMMDeviceCollection = enumerator
            .EnumAudioEndpoints(data_flow, DEVICE_STATE_ACTIVE)
            .map_err(|e| e.to_string())?;
        let count = collection.GetCount().map_err(|e| e.to_string())?;

        let mut devices = Vec::new();
        for index in 0..count {
            let device = collection.Item(index).map_err(|e| e.to_string())?;
            let id = device.GetId().map_err(|e| e.to_string())?;
            let id = id.to_string().map_err(|e| e.to_string())?;
            CoTaskMemFree(Some(id.as_ptr() as *const core::ffi::c_void));
            let name = device_name(&device).unwrap_or_else(|| id.clone());
            devices.push(Device { id, name, kind });
        }
        Ok(devices)
    }
}

/// Resolve a device id or the default capture endpoint.
fn resolve_device(device: Option<String>) -> Result<IMMDevice, String> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .map_err(|e| e.to_string())?;
    match device {
        Some(id) => unsafe { enumerator.GetDevice(&HSTRING::from(id)) }.map_err(|e| e.to_string()),
        None => unsafe { enumerator.GetDefaultAudioEndpoint(eCapture, eConsole) }
            .map_err(|e| e.to_string()),
    }
}

/// Friendly name from the endpoint's property store.
fn device_name(device: &IMMDevice) -> Option<String> {
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
        let name = if value.Anonymous.Anonymous.vt == VT_LPWSTR {
            let name = value.Anonymous.Anonymous.Anonymous.pwszVal;
            if name.is_null() {
                None
            } else {
                name.to_string().ok()
            }
        } else {
            None
        };
        let _ = PropVariantClear(&mut value);
        name
    }
}

/// Activate a process-loopback audio client that excludes our process tree.
fn activate_process_loopback() -> Result<IAudioClient, String> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: std::process::id(),
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };

    let mut variant = PROPVARIANT::default();
    unsafe {
        (*variant.Anonymous.Anonymous).vt = VT_BLOB;
        (*variant.Anonymous.Anonymous).Anonymous.blob = windows::Win32::System::Com::BLOB {
            cbSize: std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
            pBlobData: &params as *const _ as *mut u8,
        };
    }

    let (tx, rx) = mpsc::channel();
    let handler: IActivateAudioInterfaceCompletionHandler = CompletionHandler(tx).into();
    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&variant),
            &handler,
        )
    }
    .map_err(|e| e.to_string())?;

    rx.recv()
        .map_err(|_| "process loopback activation failed".to_string())?;

    let mut hr = windows::core::HRESULT(0);
    let mut interface: Option<IUnknown> = None;
    unsafe { operation.GetActivateResult(&mut hr, &mut interface) }.map_err(|e| e.to_string())?;
    if hr.is_err() {
        return Err(format!("process loopback activation failed: {hr:?}"));
    }
    interface
        .ok_or("process loopback returned no interface")?
        .cast::<IAudioClient>()
        .map_err(|e| e.to_string())
}

/// 48 kHz stereo 16-bit PCM, the format the Opus encoder expects.
fn wave_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 2,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 48_000 * 2 * 2,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

/// Completion handler for the async process-loopback activation.
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct CompletionHandler(mpsc::Sender<()>);

impl IActivateAudioInterfaceCompletionHandler_Impl for CompletionHandler_Impl {
    fn ActivateCompleted(
        &self,
        _activateoperation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> WinResult<()> {
        let _ = self.get_impl().0.send(());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_is_48k_stereo_16bit() {
        let format = wave_format();
        assert_eq!(format.wFormatTag, WAVE_FORMAT_PCM as u16);
        assert_eq!(format.nChannels, 2);
        assert_eq!(format.nSamplesPerSec, 48_000);
        assert_eq!(format.nBlockAlign, 4);
        assert_eq!(format.wBitsPerSample, 16);
    }
}
