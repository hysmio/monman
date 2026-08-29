use crate::model::{AudioDeviceConfig, MonitorLayout};
use anyhow::Result;

#[derive(Debug, Clone, Default)]
pub struct AudioDeviceInventory {
    pub playback: Vec<AudioDeviceConfig>,
    pub microphones: Vec<AudioDeviceConfig>,
    pub default_playback_id: Option<String>,
    pub default_microphone_id: Option<String>,
}

pub fn enumerate_devices() -> Result<AudioDeviceInventory> {
    platform::enumerate_devices()
}

pub fn capture_current_devices(layout: &mut MonitorLayout) -> Result<()> {
    let (playback, microphone) = platform::current_defaults()?;
    layout.playback_device = playback;
    layout.microphone_device = microphone;
    Ok(())
}

pub fn apply_layout(layout: &MonitorLayout) -> Result<()> {
    platform::apply_layout(layout)
}

#[cfg(not(windows))]
mod platform {
    use super::*;
    use anyhow::bail;

    pub(super) fn enumerate_devices() -> Result<AudioDeviceInventory> {
        Ok(AudioDeviceInventory::default())
    }

    pub(super) fn current_defaults()
    -> Result<(Option<AudioDeviceConfig>, Option<AudioDeviceConfig>)> {
        Ok((None, None))
    }

    pub(super) fn apply_layout(layout: &MonitorLayout) -> Result<()> {
        if layout.playback_device.is_some() || layout.microphone_device.is_some() {
            bail!("MonMan audio device control is only available on Windows");
        }
        Ok(())
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use anyhow::{Context, bail};
    use std::ffi::c_void;
    use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
    use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
    use windows::Win32::Media::Audio::{
        DEVICE_STATE_ACTIVE, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        eCapture, eCommunications, eConsole, eMultimedia, eRender,
    };
    use windows::Win32::System::Com::{
        CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        CoUninitialize, STGM_READ,
    };
    use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface, PCWSTR};

    const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
    const ENDPOINT_NOT_FOUND: HRESULT = HRESULT(0x80070490_u32 as i32);
    const AUDIO_ROLES: [ERole; 3] = [eConsole, eMultimedia, eCommunications];

    #[repr(transparent)]
    #[derive(Clone, PartialEq, Eq)]
    struct IPolicyConfig(IUnknown);

    impl windows::core::imp::CanInto<IUnknown> for IPolicyConfig {}

    unsafe impl Interface for IPolicyConfig {
        type Vtable = IPolicyConfig_Vtbl;
        const IID: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
    }

    #[repr(C)]
    struct IPolicyConfig_Vtbl {
        base__: IUnknown_Vtbl,
        get_mix_format: usize,
        get_device_format: usize,
        reset_device_format: usize,
        set_device_format: usize,
        get_processing_period: usize,
        set_processing_period: usize,
        get_share_mode: usize,
        set_share_mode: usize,
        get_property_value: usize,
        set_property_value: usize,
        set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
        set_endpoint_visibility: usize,
    }

    impl IPolicyConfig {
        unsafe fn set_default_endpoint(&self, endpoint_id: PCWSTR, role: ERole) -> Result<()> {
            unsafe {
                (Interface::vtable(self).set_default_endpoint)(
                    Interface::as_raw(self),
                    endpoint_id,
                    role,
                )
                .ok()
                .context("Windows audio policy rejected the default endpoint")
            }
        }
    }

    struct ComApartment {
        uninitialize: bool,
    }

    impl ComApartment {
        fn initialize() -> Result<Self> {
            let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            if result.is_ok() {
                return Ok(Self { uninitialize: true });
            }
            if result == RPC_E_CHANGED_MODE {
                // The eframe UI thread may already be an STA. COM is usable in that apartment.
                return Ok(Self {
                    uninitialize: false,
                });
            }
            Err(windows::core::Error::from_hresult(result))
                .context("could not initialize COM for Windows audio devices")
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            if self.uninitialize {
                unsafe { CoUninitialize() };
            }
        }
    }

    pub(super) fn enumerate_devices() -> Result<AudioDeviceInventory> {
        let _com = ComApartment::initialize()?;
        let enumerator = create_enumerator()?;
        let playback = enumerate_flow(&enumerator, eRender)
            .context("could not enumerate Windows playback devices")?;
        let microphones = enumerate_flow(&enumerator, eCapture)
            .context("could not enumerate Windows microphone devices")?;
        let default_playback_id = default_device(&enumerator, eRender)?.map(|device| device.id);
        let default_microphone_id = default_device(&enumerator, eCapture)?.map(|device| device.id);

        Ok(AudioDeviceInventory {
            playback,
            microphones,
            default_playback_id,
            default_microphone_id,
        })
    }

    pub(super) fn current_defaults()
    -> Result<(Option<AudioDeviceConfig>, Option<AudioDeviceConfig>)> {
        let _com = ComApartment::initialize()?;
        let enumerator = create_enumerator()?;
        Ok((
            default_device(&enumerator, eRender)
                .context("could not read the current default playback device")?,
            default_device(&enumerator, eCapture)
                .context("could not read the current default microphone")?,
        ))
    }

    pub(super) fn apply_layout(layout: &MonitorLayout) -> Result<()> {
        if layout.playback_device.is_none() && layout.microphone_device.is_none() {
            return Ok(());
        }

        let _com = ComApartment::initialize()?;
        let enumerator = create_enumerator()?;
        let active_playback = enumerate_flow(&enumerator, eRender)
            .context("could not enumerate Windows playback devices before apply")?;
        let active_microphones = enumerate_flow(&enumerator, eCapture)
            .context("could not enumerate Windows microphone devices before apply")?;

        if let Some(device) = &layout.playback_device {
            ensure_available(device, &active_playback, "playback")?;
        }
        if let Some(device) = &layout.microphone_device {
            ensure_available(device, &active_microphones, "microphone")?;
        }

        let policy: IPolicyConfig =
            unsafe { CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }
                .context("could not open Windows audio policy configuration")?;

        if let Some(device) = &layout.playback_device {
            set_default_for_all_roles(&policy, device).with_context(|| {
                format!("could not select playback device '{}'", device.label())
            })?;
        }
        if let Some(device) = &layout.microphone_device {
            set_default_for_all_roles(&policy, device)
                .with_context(|| format!("could not select microphone '{}'", device.label()))?;
        }
        Ok(())
    }

    fn create_enumerator() -> Result<IMMDeviceEnumerator> {
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("could not create the Windows MMDevice enumerator")
    }

    fn enumerate_flow(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
    ) -> Result<Vec<AudioDeviceConfig>> {
        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) }?;
        let count = unsafe { collection.GetCount() }?;
        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let device = unsafe { collection.Item(index) }?;
            devices.push(device_config(&device)?);
        }
        devices.sort_by(|left, right| {
            left.label()
                .to_ascii_lowercase()
                .cmp(&right.label().to_ascii_lowercase())
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(devices)
    }

    fn default_device(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
    ) -> Result<Option<AudioDeviceConfig>> {
        match unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) } {
            Ok(device) => device_config(&device).map(Some),
            Err(error) if error.code() == ENDPOINT_NOT_FOUND => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    fn device_config(device: &IMMDevice) -> Result<AudioDeviceConfig> {
        let id = device_id(device)?;
        let store = unsafe { device.OpenPropertyStore(STGM_READ) }
            .context("could not open audio endpoint properties")?;
        let name = unsafe { store.GetValue(&PKEY_Device_FriendlyName) }
            .context("could not read audio endpoint friendly name")?
            .to_string();

        Ok(AudioDeviceConfig { id, name })
    }

    fn device_id(device: &IMMDevice) -> Result<String> {
        let raw = unsafe { device.GetId() }.context("could not read audio endpoint ID")?;
        let id = unsafe { raw.to_string() };
        unsafe { CoTaskMemFree(Some(raw.0.cast())) };
        id.context("audio endpoint ID was not valid UTF-16")
    }

    fn ensure_available(
        selected: &AudioDeviceConfig,
        active: &[AudioDeviceConfig],
        kind: &str,
    ) -> Result<()> {
        if active.iter().any(|device| device.id == selected.id) {
            return Ok(());
        }
        bail!(
            "saved {kind} device '{}' is disconnected or unavailable",
            selected.label()
        )
    }

    fn set_default_for_all_roles(policy: &IPolicyConfig, device: &AudioDeviceConfig) -> Result<()> {
        let endpoint_id = device
            .id
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        for role in AUDIO_ROLES {
            unsafe { policy.set_default_endpoint(PCWSTR(endpoint_id.as_ptr()), role) }?;
        }
        Ok(())
    }
}
