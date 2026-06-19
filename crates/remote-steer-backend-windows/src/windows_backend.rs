use std::collections::HashMap;
use std::ffi::c_void;
use std::fmt::Write;
use std::mem::{offset_of, size_of};
use std::ptr::null_mut;
use std::time::{SystemTime, UNIX_EPOCH};

use remote_steer_core::{
    profile_by_id, AxisKind, AxisValue, BackendCapabilities, ButtonValue, EffectId, FfbCommand,
    FfbCommandKind, FfbEffect, FfbEffectKind, FfbEnvelope, FfbReply, FfbReplyKind,
    PhysicalWheelBackend, RemoteSteerError, Result, WheelProfileId, WheelStateSnapshot,
};
use windows::core::{w, Interface, BOOL, GUID, PCWSTR};
use windows::Win32::Devices::HumanInterfaceDevice::{
    DirectInput8Create, GUID_Button, GUID_ConstantForce, GUID_Damper, GUID_Friction, GUID_Inertia,
    GUID_RampForce, GUID_RxAxis, GUID_RyAxis, GUID_RzAxis, GUID_SawtoothDown, GUID_SawtoothUp,
    GUID_Sine, GUID_Slider, GUID_Spring, GUID_Square, GUID_Triangle, GUID_XAxis, GUID_YAxis,
    GUID_ZAxis, IDirectInput8W, IDirectInputDevice8W, IDirectInputEffect, DI8DEVCLASS_GAMECTRL,
    DICONDITION, DICONSTANTFORCE, DIDATAFORMAT, DIDEVCAPS, DIDEVICEINSTANCEW,
    DIDEVICEOBJECTINSTANCEW, DIDFT_ALL, DIDFT_AXIS, DIDFT_FFACTUATOR, DIDF_ABSAXIS,
    DIDOI_FFACTUATOR, DIEB_NOTRIGGER, DIEDFL_ATTACHEDONLY, DIEDFL_FORCEFEEDBACK, DIEFFECT,
    DIEFFECTINFOW, DIEFF_CARTESIAN, DIEFF_OBJECTIDS, DIEFF_OBJECTOFFSETS, DIEFF_POLAR, DIEFT_ALL,
    DIENUM_CONTINUE, DIENUM_STOP, DIENVELOPE, DIERR_INPUTLOST, DIERR_NOTACQUIRED, DIJOYSTATE2,
    DIOBJECTDATAFORMAT, DIPERIODIC, DIPH_BYOFFSET, DIPH_DEVICE, DIPROPDWORD, DIPROPHEADER,
    DIPROPRANGE, DIPROP_AUTOCENTER, DIPROP_FFGAIN, DIPROP_RANGE, DIRAMPFORCE, DIRECTINPUT_VERSION,
    DISCL_BACKGROUND, DISCL_EXCLUSIVE, DISFFC_RESET, DISFFC_STOPALL, DI_FFNOMINALMAX, GUID_POV,
};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE,
    WNDCLASSW,
};

const T150_VENDOR: u16 = 0x044f;
const T150_PRODUCT: u16 = 0xb677;
const DIDFT_AXIS_ANY_OPTIONAL: u32 = 0x80ff_ff03;
const DIDFT_BUTTON_ANY_OPTIONAL: u32 = 0x80ff_ff0c;
const DIDFT_POV_ANY_OPTIONAL: u32 = 0x80ff_ff10;
const DATA_FORMAT_ASPECT_POSITION: u32 = 0x0001_0000;
const DATA_FORMAT_ASPECT_VELOCITY: u32 = 0x0002_0000;
const DATA_FORMAT_ASPECT_ACCEL: u32 = 0x0003_0000;
const DATA_FORMAT_ASPECT_FORCE: u32 = 0x0004_0000;

pub struct WindowsPhysicalBackend {
    _com: ComApartment,
    _window: HiddenWindow,
    _input: IDirectInput8W,
    device: IDirectInputDevice8W,
    profile: remote_steer_core::WheelProfile,
    device_name: String,
    ffb_axis_object_id: u32,
    master_gain: u16,
    effects: HashMap<EffectId, IDirectInputEffect>,
    seq: u64,
}

unsafe impl Send for WindowsPhysicalBackend {}

impl WindowsPhysicalBackend {
    pub fn open_t150() -> Result<Self> {
        let profile = profile_by_id(WheelProfileId::T150);
        let com = ComApartment::initialize()?;
        let window = HiddenWindow::create()?;
        let input = create_direct_input()?;
        let candidate = find_t150_candidate(&input)?;
        let device = create_device(&input, &candidate)?;
        let ffb_axis_object_id = configure_device(&device, window.hwnd())?;

        Ok(Self {
            _com: com,
            _window: window,
            _input: input,
            device,
            profile,
            device_name: candidate.display_name(),
            ffb_axis_object_id,
            master_gain: u16::MAX,
            effects: HashMap::new(),
            seq: 0,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.device_name
    }

    fn reacquire(&self) -> Result<()> {
        unsafe {
            self.device
                .Acquire()
                .map_err(|err| windows_error("DirectInput Acquire", err))
        }
    }

    fn upload_effect(&mut self, effect: FfbEffect) -> Result<()> {
        if let Some(existing) = self.effects.remove(&effect.id) {
            unsafe {
                let _ = existing.Stop();
                let _ = existing.Unload();
            }
        }

        let created = unsafe {
            create_direct_input_effect(
                &self.device,
                self.ffb_axis_object_id,
                self.master_gain,
                &effect,
            )?
        };
        self.effects.insert(effect.id, created);
        Ok(())
    }

    fn erase_effect(&mut self, effect_id: EffectId) {
        if let Some(effect) = self.effects.remove(&effect_id) {
            unsafe {
                let _ = effect.Stop();
                let _ = effect.Unload();
            }
        }
    }

    fn play_effect(&mut self, effect_id: EffectId, repetitions: i32) -> Result<()> {
        let effect = self.effects.get(&effect_id).ok_or_else(|| {
            RemoteSteerError::Backend(format!(
                "DirectInput effect {:?} is not uploaded",
                effect_id
            ))
        })?;
        let repetitions = if repetitions <= 0 {
            1
        } else {
            repetitions as u32
        };
        unsafe {
            effect
                .Start(repetitions, 0)
                .map_err(|err| windows_error("DirectInput effect Start", err))
        }
    }

    fn stop_effect(&mut self, effect_id: EffectId) -> Result<()> {
        let Some(effect) = self.effects.get(&effect_id) else {
            return Ok(());
        };
        unsafe {
            effect
                .Stop()
                .map_err(|err| windows_error("DirectInput effect Stop", err))
        }
    }

    fn set_gain(&mut self, gain: u16) -> Result<()> {
        self.master_gain = gain;
        let value = scale_u16_to_di(gain);
        let mut property = DIPROPDWORD {
            diph: property_header::<DIPROPDWORD>(DIPH_DEVICE, 0),
            dwData: value,
        };
        let result = unsafe {
            self.device.SetProperty(
                &DIPROP_FFGAIN,
                &mut property as *mut DIPROPDWORD as *mut DIPROPHEADER,
            )
        };
        match result {
            Ok(()) => Ok(()),
            Err(err) if is_not_implemented_error(&err) => Ok(()),
            Err(err) => Err(windows_error("DirectInput SetProperty DIPROP_FFGAIN", err)),
        }
    }

    fn set_autocenter(&self, magnitude: u16) -> Result<()> {
        let mut property = DIPROPDWORD {
            diph: property_header::<DIPROPDWORD>(DIPH_DEVICE, 0),
            dwData: u32::from(magnitude > 0),
        };
        unsafe {
            self.device
                .SetProperty(
                    &DIPROP_AUTOCENTER,
                    &mut property as *mut DIPROPDWORD as *mut DIPROPHEADER,
                )
                .map_err(|err| windows_error("DirectInput SetProperty DIPROP_AUTOCENTER", err))
        }
    }

    fn reset_ffb(&self) -> Result<()> {
        unsafe {
            self.device
                .SendForceFeedbackCommand(DISFFC_STOPALL)
                .map_err(|err| {
                    windows_error("DirectInput SendForceFeedbackCommand STOPALL", err)
                })?;
            self.device
                .SendForceFeedbackCommand(DISFFC_RESET)
                .map_err(|err| windows_error("DirectInput SendForceFeedbackCommand RESET", err))
        }
    }
}

impl PhysicalWheelBackend for WindowsPhysicalBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            profile: self.profile.clone(),
            can_read_input: true,
            can_apply_ffb: true,
            can_inject_input: false,
            can_capture_ffb: false,
        }
    }

    fn poll_input(&mut self) -> Result<Option<WheelStateSnapshot>> {
        unsafe {
            if let Err(err) = self.device.Poll() {
                if is_reacquire_error(&err) {
                    self.reacquire()?;
                } else {
                    return Err(windows_error("DirectInput Poll", err));
                }
            }

            let mut state = DIJOYSTATE2::default();
            if let Err(err) = self.device.GetDeviceState(
                size_of::<DIJOYSTATE2>() as u32,
                &mut state as *mut DIJOYSTATE2 as *mut c_void,
            ) {
                if is_reacquire_error(&err) {
                    self.reacquire()?;
                    return Ok(None);
                }
                return Err(windows_error("DirectInput GetDeviceState", err));
            }

            self.seq = self.seq.wrapping_add(1);
            Ok(Some(snapshot_from_dijoystate(self.seq, &state)))
        }
    }

    fn apply_ffb(&mut self, command: FfbCommand) -> Result<FfbReply> {
        let command_id = command.command_id;
        let result = match command.kind {
            FfbCommandKind::Upload { effect } | FfbCommandKind::Update { effect } => {
                self.upload_effect(effect)
            }
            FfbCommandKind::Erase { effect_id } => {
                self.erase_effect(effect_id);
                Ok(())
            }
            FfbCommandKind::Play {
                effect_id,
                repetitions,
            } => self.play_effect(effect_id, repetitions),
            FfbCommandKind::Stop { effect_id } => self.stop_effect(effect_id),
            FfbCommandKind::SetGain { gain } => self.set_gain(gain),
            FfbCommandKind::SetAutocenter { magnitude } => self.set_autocenter(magnitude),
            FfbCommandKind::ResetState => self.reset_ffb(),
        };

        Ok(FfbReply {
            command_id,
            kind: match result {
                Ok(()) => FfbReplyKind::Ack,
                Err(err) => FfbReplyKind::Rejected {
                    reason: err.to_string(),
                },
            },
        })
    }
}

pub fn dump_t150_directinput() -> Result<String> {
    let _com = ComApartment::initialize()?;
    let window = HiddenWindow::create()?;
    let input = create_direct_input()?;
    let candidate = find_t150_candidate(&input)?;
    let device = create_device(&input, &candidate)?;
    let mut output = String::new();

    writeln!(
        &mut output,
        "device: {} ({:04x}:{:04x})",
        candidate.display_name(),
        candidate.vendor,
        candidate.product
    )
    .ok();

    unsafe {
        let mut caps = DIDEVCAPS {
            dwSize: size_of::<DIDEVCAPS>() as u32,
            ..Default::default()
        };
        match device.GetCapabilities(&mut caps) {
            Ok(()) => {
                writeln!(
                    &mut output,
                    "caps: flags=0x{:08x} dev_type=0x{:08x} axes={} buttons={} povs={} ff_sample_period={} ff_min_time_resolution={} firmware={} hardware={} ff_driver={}",
                    caps.dwFlags,
                    caps.dwDevType,
                    caps.dwAxes,
                    caps.dwButtons,
                    caps.dwPOVs,
                    caps.dwFFSamplePeriod,
                    caps.dwFFMinTimeResolution,
                    caps.dwFirmwareRevision,
                    caps.dwHardwareRevision,
                    caps.dwFFDriverVersion
                )
                .ok();
            }
            Err(err) => {
                writeln!(&mut output, "caps: ERROR {err}").ok();
            }
        };

        let mut objects = ObjectDumpContext::default();
        let all_result = device.EnumObjects(
            Some(enum_object_dump_callback),
            &mut objects as *mut ObjectDumpContext as *mut c_void,
            DIDFT_ALL,
        );
        writeln!(
            &mut output,
            "objects_all: {:?}",
            all_result.map_err(|err| err.to_string())
        )
        .ok();
        for object in &objects.records {
            writeln!(
                &mut output,
                "object: ofs=0x{:04x} type=0x{:08x} flags=0x{:08x} guid={} name=\"{}\" ff_max={} ff_res={} usage={:04x}:{:04x}",
                object.offset,
                object.object_type,
                object.flags,
                object.guid,
                object.name,
                object.ff_max_force,
                object.ff_force_resolution,
                object.usage_page,
                object.usage,
            )
            .ok();
        }

        let mut ffb_only = ObjectDumpContext::default();
        let ffb_result = device.EnumObjects(
            Some(enum_object_dump_callback),
            &mut ffb_only as *mut ObjectDumpContext as *mut c_void,
            DIDFT_AXIS | DIDFT_FFACTUATOR,
        );
        writeln!(
            &mut output,
            "objects_ff_actuator_axis: {:?}, count={}",
            ffb_result.map_err(|err| err.to_string()),
            ffb_only.records.len()
        )
        .ok();
        for object in &ffb_only.records {
            writeln!(
                &mut output,
                "ff_axis: ofs=0x{:04x} type=0x{:08x} flags=0x{:08x} guid={} name=\"{}\"",
                object.offset, object.object_type, object.flags, object.guid, object.name
            )
            .ok();
        }

        let mut effects = EffectDumpContext::default();
        let effects_result = device.EnumEffects(
            Some(enum_effect_dump_callback),
            &mut effects as *mut EffectDumpContext as *mut c_void,
            DIEFT_ALL,
        );
        writeln!(
            &mut output,
            "effects_all: {:?}, count={}",
            effects_result.map_err(|err| err.to_string()),
            effects.records.len()
        )
        .ok();
        for effect in &effects.records {
            writeln!(
                &mut output,
                "effect: guid={} type=0x{:08x} static=0x{:08x} dynamic=0x{:08x} name=\"{}\"",
                effect.guid,
                effect.effect_type,
                effect.static_params,
                effect.dynamic_params,
                effect.name
            )
            .ok();
        }

        let mut data_objects = joystick2_object_format();
        let mut data_format = DIDATAFORMAT {
            dwSize: size_of::<DIDATAFORMAT>() as u32,
            dwObjSize: size_of::<DIOBJECTDATAFORMAT>() as u32,
            dwFlags: DIDF_ABSAXIS,
            dwDataSize: size_of::<DIJOYSTATE2>() as u32,
            dwNumObjs: data_objects.len() as u32,
            rgodf: data_objects.as_mut_ptr(),
        };
        let set_data_format = device.SetDataFormat(&mut data_format);
        writeln!(
            &mut output,
            "set_data_format: {:?}",
            set_data_format.map_err(|err| err.to_string())
        )
        .ok();
        let set_coop =
            device.SetCooperativeLevel(window.hwnd(), DISCL_BACKGROUND | DISCL_EXCLUSIVE);
        writeln!(
            &mut output,
            "set_cooperative_level: {:?}",
            set_coop.map_err(|err| err.to_string())
        )
        .ok();
        let acquire = device.Acquire();
        writeln!(
            &mut output,
            "acquire: {:?}",
            acquire.map_err(|err| err.to_string())
        )
        .ok();

        append_create_effect_probe(
            &mut output,
            &device,
            "offset-x-cartesian-1axis",
            DIEFF_CARTESIAN | DIEFF_OBJECTOFFSETS,
            &[offset_u32(offset_of!(DIJOYSTATE2, lX))],
            &[DI_FFNOMINALMAX as i32],
        );
        append_create_effect_probe(
            &mut output,
            &device,
            "offset-x-y-polar-doc",
            DIEFF_POLAR | DIEFF_OBJECTOFFSETS,
            &[
                offset_u32(offset_of!(DIJOYSTATE2, lX)),
                offset_u32(offset_of!(DIJOYSTATE2, lY)),
            ],
            &[18_000, 0],
        );
        if let Some(object) = objects
            .records
            .iter()
            .find(|object| object.guid == "GUID_XAxis")
        {
            append_create_effect_probe(
                &mut output,
                &device,
                "objectid-x-cartesian-1axis",
                DIEFF_CARTESIAN | DIEFF_OBJECTIDS,
                &[object.object_type],
                &[DI_FFNOMINALMAX as i32],
            );
        }
        if let Some(object) = ffb_only.records.first() {
            append_create_effect_probe(
                &mut output,
                &device,
                "objectid-ff-cartesian-1axis",
                DIEFF_CARTESIAN | DIEFF_OBJECTIDS,
                &[object.object_type],
                &[DI_FFNOMINALMAX as i32],
            );
        }
    }

    Ok(output)
}

#[derive(Default)]
struct ObjectDumpContext {
    records: Vec<ObjectRecord>,
}

struct ObjectRecord {
    offset: u32,
    object_type: u32,
    flags: u32,
    guid: &'static str,
    name: String,
    ff_max_force: u32,
    ff_force_resolution: u32,
    usage_page: u16,
    usage: u16,
}

#[derive(Default)]
struct EffectDumpContext {
    records: Vec<EffectRecord>,
}

struct EffectRecord {
    guid: &'static str,
    effect_type: u32,
    static_params: u32,
    dynamic_params: u32,
    name: String,
}

unsafe extern "system" fn enum_object_dump_callback(
    object: *mut DIDEVICEOBJECTINSTANCEW,
    context: *mut c_void,
) -> BOOL {
    if object.is_null() || context.is_null() {
        return BOOL(DIENUM_CONTINUE as i32);
    }

    let object = unsafe { &*object };
    let context = unsafe { &mut *(context as *mut ObjectDumpContext) };
    context.records.push(ObjectRecord {
        offset: object.dwOfs,
        object_type: object.dwType,
        flags: object.dwFlags,
        guid: guid_label(&object.guidType),
        name: utf16z_to_string(&object.tszName),
        ff_max_force: object.dwFFMaxForce,
        ff_force_resolution: object.dwFFForceResolution,
        usage_page: object.wUsagePage,
        usage: object.wUsage,
    });

    BOOL(DIENUM_CONTINUE as i32)
}

unsafe extern "system" fn enum_effect_dump_callback(
    effect: *mut DIEFFECTINFOW,
    context: *mut c_void,
) -> BOOL {
    if effect.is_null() || context.is_null() {
        return BOOL(DIENUM_CONTINUE as i32);
    }

    let effect = unsafe { &*effect };
    let context = unsafe { &mut *(context as *mut EffectDumpContext) };
    context.records.push(EffectRecord {
        guid: guid_label(&effect.guid),
        effect_type: effect.dwEffType,
        static_params: effect.dwStaticParams,
        dynamic_params: effect.dwDynamicParams,
        name: utf16z_to_string(&effect.tszName),
    });

    BOOL(DIENUM_CONTINUE as i32)
}

unsafe fn append_create_effect_probe(
    output: &mut String,
    device: &IDirectInputDevice8W,
    label: &str,
    flags: u32,
    axes: &[u32],
    directions: &[i32],
) {
    let mut axes = axes.to_vec();
    let mut directions = directions.to_vec();
    let mut constant = DICONSTANTFORCE {
        lMagnitude: DI_FFNOMINALMAX as i32,
    };
    let mut effect = DIEFFECT {
        dwSize: size_of::<DIEFFECT>() as u32,
        dwFlags: flags,
        dwDuration: 500_000,
        dwSamplePeriod: 0,
        dwGain: DI_FFNOMINALMAX,
        dwTriggerButton: DIEB_NOTRIGGER,
        dwTriggerRepeatInterval: 0,
        cAxes: axes.len() as u32,
        rgdwAxes: axes.as_mut_ptr(),
        rglDirection: directions.as_mut_ptr(),
        lpEnvelope: null_mut(),
        cbTypeSpecificParams: size_of::<DICONSTANTFORCE>() as u32,
        lpvTypeSpecificParams: &mut constant as *mut DICONSTANTFORCE as *mut c_void,
        dwStartDelay: 0,
    };
    let mut out = None;
    let result = device.CreateEffect(&GUID_ConstantForce, &mut effect, &mut out, None);
    match result {
        Ok(()) => {
            if let Some(effect) = out {
                let _ = effect.Unload();
            }
            writeln!(output, "create_effect_probe {label}: OK").ok();
        }
        Err(err) => {
            writeln!(output, "create_effect_probe {label}: ERROR {err}").ok();
        }
    }
}

struct ComApartment;

impl ComApartment {
    fn initialize() -> Result<Self> {
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED)
                .ok()
                .map_err(|err| windows_error("CoInitializeEx", err))?;
        }
        Ok(Self)
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct HiddenWindow {
    hwnd: HWND,
}

unsafe impl Send for HiddenWindow {}

impl HiddenWindow {
    fn create() -> Result<Self> {
        unsafe {
            let module = GetModuleHandleW(None)
                .map_err(|err| windows_error("GetModuleHandleW for hidden window", err))?;
            let hinstance = HINSTANCE(module.0);
            let class_name = w!("RemoteSteerHiddenWindow");
            let window_class = WNDCLASSW {
                style: Default::default(),
                lpfnWndProc: Some(hidden_window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: Default::default(),
                hCursor: Default::default(),
                hbrBackground: Default::default(),
                lpszMenuName: PCWSTR::null(),
                lpszClassName: class_name,
            };

            if RegisterClassW(&window_class) == 0 {
                return Err(windows_error(
                    "RegisterClassW hidden window",
                    windows::core::Error::from_thread(),
                ));
            }

            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                class_name,
                w!("remote-steer"),
                WINDOW_STYLE(0),
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinstance),
                None,
            )
            .map_err(|err| windows_error("CreateWindowExW hidden window", err))?;

            Ok(Self { hwnd })
        }
    }

    fn hwnd(&self) -> HWND {
        self.hwnd
    }
}

impl Drop for HiddenWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

unsafe extern "system" fn hidden_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

#[derive(Debug, Clone)]
struct DeviceCandidate {
    guid_instance: GUID,
    instance_name: String,
    product_name: String,
    vendor: u16,
    product: u16,
}

impl DeviceCandidate {
    fn display_name(&self) -> String {
        if self.product_name.is_empty() {
            self.instance_name.clone()
        } else {
            self.product_name.clone()
        }
    }
}

#[derive(Default)]
struct EnumContext {
    selected: Option<DeviceCandidate>,
    seen: Vec<String>,
}

fn create_direct_input() -> Result<IDirectInput8W> {
    unsafe {
        let module =
            GetModuleHandleW(None).map_err(|err| windows_error("GetModuleHandleW", err))?;
        let mut out: *mut c_void = null_mut();
        DirectInput8Create(
            HINSTANCE(module.0),
            DIRECTINPUT_VERSION,
            &IDirectInput8W::IID,
            &mut out,
            None,
        )
        .map_err(|err| windows_error("DirectInput8Create", err))?;

        if out.is_null() {
            return Err(RemoteSteerError::DeviceNotFound(
                "DirectInput8Create returned null".to_string(),
            ));
        }
        Ok(IDirectInput8W::from_raw(out))
    }
}

fn find_t150_candidate(input: &IDirectInput8W) -> Result<DeviceCandidate> {
    let mut context = EnumContext::default();
    unsafe {
        input
            .EnumDevices(
                DI8DEVCLASS_GAMECTRL,
                Some(enum_device_callback),
                &mut context as *mut EnumContext as *mut c_void,
                DIEDFL_ATTACHEDONLY | DIEDFL_FORCEFEEDBACK,
            )
            .map_err(|err| windows_error("DirectInput EnumDevices", err))?;
    }

    context.selected.ok_or_else(|| {
        RemoteSteerError::DeviceNotFound(format!(
            "T150RS DirectInput force-feedback device not found; seen devices: {}",
            if context.seen.is_empty() {
                "(none)".to_string()
            } else {
                context.seen.join(", ")
            }
        ))
    })
}

unsafe extern "system" fn enum_device_callback(
    instance: *mut DIDEVICEINSTANCEW,
    context: *mut c_void,
) -> BOOL {
    if instance.is_null() || context.is_null() {
        return BOOL(DIENUM_CONTINUE as i32);
    }

    let instance = unsafe { &*instance };
    let context = unsafe { &mut *(context as *mut EnumContext) };
    let instance_name = utf16z_to_string(&instance.tszInstanceName);
    let product_name = utf16z_to_string(&instance.tszProductName);
    let (vendor, product) = vid_pid_from_guid_product(instance.guidProduct);
    let display_name = if product_name.is_empty() {
        &instance_name
    } else {
        &product_name
    };
    context
        .seen
        .push(format!("{display_name} ({vendor:04x}:{product:04x})"));

    let name = display_name.to_ascii_lowercase();
    let is_t150 = (vendor == T150_VENDOR && product == T150_PRODUCT)
        || name.contains("t150")
        || name.contains("t150rs");
    if is_t150 {
        context.selected = Some(DeviceCandidate {
            guid_instance: instance.guidInstance,
            instance_name,
            product_name,
            vendor,
            product,
        });
        return BOOL(DIENUM_STOP as i32);
    }

    BOOL(DIENUM_CONTINUE as i32)
}

fn create_device(
    input: &IDirectInput8W,
    candidate: &DeviceCandidate,
) -> Result<IDirectInputDevice8W> {
    let mut device = None;
    unsafe {
        input
            .CreateDevice(&candidate.guid_instance, &mut device, None)
            .map_err(|err| {
                windows_error(
                    &format!(
                        "DirectInput CreateDevice {} ({:04x}:{:04x})",
                        candidate.display_name(),
                        candidate.vendor,
                        candidate.product
                    ),
                    err,
                )
            })?;
    }

    device.ok_or_else(|| {
        RemoteSteerError::DeviceNotFound(format!(
            "DirectInput CreateDevice returned null for {}",
            candidate.display_name()
        ))
    })
}

fn configure_device(device: &IDirectInputDevice8W, hwnd: HWND) -> Result<u32> {
    let mut objects = joystick2_object_format();
    let mut data_format = DIDATAFORMAT {
        dwSize: size_of::<DIDATAFORMAT>() as u32,
        dwObjSize: size_of::<DIOBJECTDATAFORMAT>() as u32,
        dwFlags: DIDF_ABSAXIS,
        dwDataSize: size_of::<DIJOYSTATE2>() as u32,
        dwNumObjs: objects.len() as u32,
        rgodf: objects.as_mut_ptr(),
    };

    unsafe {
        device
            .SetDataFormat(&mut data_format)
            .map_err(|err| windows_error("DirectInput SetDataFormat DIJOYSTATE2", err))?;
        let ffb_axis_object_id = find_ffb_axis_object_id(device)?;

        try_set_axis_range(device, offset_u32(offset_of!(DIJOYSTATE2, lX)), 0, 65535);
        try_set_axis_range(device, offset_u32(offset_of!(DIJOYSTATE2, lY)), 0, 255);
        try_set_axis_range(device, offset_u32(offset_of!(DIJOYSTATE2, lZ)), 0, 255);
        try_set_axis_range(device, offset_u32(offset_of!(DIJOYSTATE2, lRz)), 0, 255);
        try_set_axis_range(
            device,
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider)),
            0,
            255,
        );

        device
            .SetCooperativeLevel(hwnd, DISCL_BACKGROUND | DISCL_EXCLUSIVE)
            .map_err(|err| windows_error("DirectInput SetCooperativeLevel exclusive", err))?;
        let _ = set_device_dword_property(device, &DIPROP_AUTOCENTER, 0);
        let _ = set_device_dword_property(device, &DIPROP_FFGAIN, DI_FFNOMINALMAX);
        device
            .Acquire()
            .map_err(|err| windows_error("DirectInput Acquire", err))?;
        device
            .SendForceFeedbackCommand(DISFFC_STOPALL)
            .map_err(|err| windows_error("DirectInput SendForceFeedbackCommand STOPALL", err))?;
        device
            .SendForceFeedbackCommand(DISFFC_RESET)
            .map_err(|err| windows_error("DirectInput SendForceFeedbackCommand RESET", err))?;

        Ok(ffb_axis_object_id)
    }
}

fn joystick2_object_format() -> Vec<DIOBJECTDATAFORMAT> {
    let mut objects = Vec::with_capacity(164);
    push_axis_group(
        &mut objects,
        DATA_FORMAT_ASPECT_POSITION,
        [
            offset_u32(offset_of!(DIJOYSTATE2, lX)),
            offset_u32(offset_of!(DIJOYSTATE2, lY)),
            offset_u32(offset_of!(DIJOYSTATE2, lZ)),
            offset_u32(offset_of!(DIJOYSTATE2, lRx)),
            offset_u32(offset_of!(DIJOYSTATE2, lRy)),
            offset_u32(offset_of!(DIJOYSTATE2, lRz)),
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider)),
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider) + size_of::<i32>()),
        ],
    );

    for index in 0..4 {
        objects.push(object_format(
            &GUID_POV,
            offset_u32(offset_of!(DIJOYSTATE2, rgdwPOV) + index * size_of::<u32>()),
            DIDFT_POV_ANY_OPTIONAL,
            0,
        ));
    }

    for index in 0..128 {
        objects.push(object_format(
            &GUID_Button,
            offset_u32(offset_of!(DIJOYSTATE2, rgbButtons) + index),
            DIDFT_BUTTON_ANY_OPTIONAL,
            0,
        ));
    }

    push_axis_group(
        &mut objects,
        DATA_FORMAT_ASPECT_VELOCITY,
        [
            offset_u32(offset_of!(DIJOYSTATE2, lVX)),
            offset_u32(offset_of!(DIJOYSTATE2, lVY)),
            offset_u32(offset_of!(DIJOYSTATE2, lVZ)),
            offset_u32(offset_of!(DIJOYSTATE2, lVRx)),
            offset_u32(offset_of!(DIJOYSTATE2, lVRy)),
            offset_u32(offset_of!(DIJOYSTATE2, lVRz)),
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider)),
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider) + size_of::<i32>()),
        ],
    );
    push_axis_group(
        &mut objects,
        DATA_FORMAT_ASPECT_ACCEL,
        [
            offset_u32(offset_of!(DIJOYSTATE2, lAX)),
            offset_u32(offset_of!(DIJOYSTATE2, lAY)),
            offset_u32(offset_of!(DIJOYSTATE2, lAZ)),
            offset_u32(offset_of!(DIJOYSTATE2, lARx)),
            offset_u32(offset_of!(DIJOYSTATE2, lARy)),
            offset_u32(offset_of!(DIJOYSTATE2, lARz)),
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider)),
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider) + size_of::<i32>()),
        ],
    );
    push_axis_group(
        &mut objects,
        DATA_FORMAT_ASPECT_FORCE,
        [
            offset_u32(offset_of!(DIJOYSTATE2, lFX)),
            offset_u32(offset_of!(DIJOYSTATE2, lFY)),
            offset_u32(offset_of!(DIJOYSTATE2, lFZ)),
            offset_u32(offset_of!(DIJOYSTATE2, lFRx)),
            offset_u32(offset_of!(DIJOYSTATE2, lFRy)),
            offset_u32(offset_of!(DIJOYSTATE2, lFRz)),
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider)),
            offset_u32(offset_of!(DIJOYSTATE2, rglSlider) + size_of::<i32>()),
        ],
    );

    objects
}

fn push_axis_group(objects: &mut Vec<DIOBJECTDATAFORMAT>, flags: u32, offsets: [u32; 8]) {
    let guids = [
        &GUID_XAxis,
        &GUID_YAxis,
        &GUID_ZAxis,
        &GUID_RxAxis,
        &GUID_RyAxis,
        &GUID_RzAxis,
        &GUID_Slider,
        &GUID_Slider,
    ];
    for (guid, offset) in guids.into_iter().zip(offsets) {
        objects.push(object_format(guid, offset, DIDFT_AXIS_ANY_OPTIONAL, flags));
    }
}

fn object_format(guid: &GUID, offset: u32, data_type: u32, flags: u32) -> DIOBJECTDATAFORMAT {
    DIOBJECTDATAFORMAT {
        pguid: guid as *const GUID,
        dwOfs: offset,
        dwType: data_type,
        dwFlags: flags,
    }
}

#[derive(Default)]
struct FfbObjectContext {
    x_axis_object_id: Option<u32>,
    fallback_axis_object_id: Option<u32>,
}

fn find_ffb_axis_object_id(device: &IDirectInputDevice8W) -> Result<u32> {
    let mut context = FfbObjectContext::default();
    unsafe {
        device
            .EnumObjects(
                Some(enum_ffb_object_callback),
                &mut context as *mut FfbObjectContext as *mut c_void,
                DIDFT_AXIS | DIDFT_FFACTUATOR,
            )
            .map_err(|err| windows_error("DirectInput EnumObjects FFACTUATOR", err))?;
    }

    context
        .x_axis_object_id
        .or(context.fallback_axis_object_id)
        .ok_or_else(|| {
            RemoteSteerError::Backend(
                "DirectInput did not expose a force-feedback actuator axis".into(),
            )
        })
}

unsafe extern "system" fn enum_ffb_object_callback(
    object: *mut DIDEVICEOBJECTINSTANCEW,
    context: *mut c_void,
) -> BOOL {
    if object.is_null() || context.is_null() {
        return BOOL(DIENUM_CONTINUE as i32);
    }

    let object = unsafe { &*object };
    let context = unsafe { &mut *(context as *mut FfbObjectContext) };
    if object.dwFlags & DIDOI_FFACTUATOR != 0 {
        if context.fallback_axis_object_id.is_none() {
            context.fallback_axis_object_id = Some(object.dwType);
        }
        if object.guidType == GUID_XAxis {
            context.x_axis_object_id = Some(object.dwType);
            return BOOL(DIENUM_STOP as i32);
        }
    }

    BOOL(DIENUM_CONTINUE as i32)
}

unsafe fn create_direct_input_effect(
    device: &IDirectInputDevice8W,
    axis_object_id: u32,
    master_gain: u16,
    effect: &FfbEffect,
) -> Result<IDirectInputEffect> {
    match &effect.kind {
        FfbEffectKind::Constant { level, envelope } => {
            let mut params = DICONSTANTFORCE {
                lMagnitude: scale_i16_to_di(*level),
            };
            create_effect_with_params(
                device,
                axis_object_id,
                master_gain,
                effect,
                &GUID_ConstantForce,
                &mut params as *mut DICONSTANTFORCE as *mut c_void,
                size_of::<DICONSTANTFORCE>() as u32,
                Some(*envelope),
            )
        }
        FfbEffectKind::Periodic {
            waveform,
            period_ms,
            magnitude,
            offset,
            phase,
            envelope,
        } => {
            let guid = match waveform {
                remote_steer_core::PeriodicWaveform::Sine => GUID_Sine,
                remote_steer_core::PeriodicWaveform::Square => GUID_Square,
                remote_steer_core::PeriodicWaveform::Triangle => GUID_Triangle,
                remote_steer_core::PeriodicWaveform::SawUp => GUID_SawtoothUp,
                remote_steer_core::PeriodicWaveform::SawDown => GUID_SawtoothDown,
            };
            let mut params = DIPERIODIC {
                dwMagnitude: scale_i16_magnitude_to_di(*magnitude),
                lOffset: scale_i16_to_di(*offset),
                dwPhase: phase_to_direct_input(*phase),
                dwPeriod: u32::from(*period_ms) * 1000,
            };
            create_effect_with_params(
                device,
                axis_object_id,
                master_gain,
                effect,
                &guid,
                &mut params as *mut DIPERIODIC as *mut c_void,
                size_of::<DIPERIODIC>() as u32,
                Some(*envelope),
            )
        }
        FfbEffectKind::Ramp {
            start_level,
            end_level,
            envelope,
        } => {
            let mut params = DIRAMPFORCE {
                lStart: scale_i16_to_di(*start_level),
                lEnd: scale_i16_to_di(*end_level),
            };
            create_effect_with_params(
                device,
                axis_object_id,
                master_gain,
                effect,
                &GUID_RampForce,
                &mut params as *mut DIRAMPFORCE as *mut c_void,
                size_of::<DIRAMPFORCE>() as u32,
                Some(*envelope),
            )
        }
        FfbEffectKind::Condition { condition, axes } => {
            let guid = match condition {
                remote_steer_core::ConditionKind::Spring => GUID_Spring,
                remote_steer_core::ConditionKind::Damper => GUID_Damper,
                remote_steer_core::ConditionKind::Friction => GUID_Friction,
                remote_steer_core::ConditionKind::Inertia => GUID_Inertia,
            };
            let mut params = [condition_axis_to_direct_input(axes[0])];
            create_effect_with_params(
                device,
                axis_object_id,
                master_gain,
                effect,
                &guid,
                params.as_mut_ptr() as *mut c_void,
                size_of::<DICONDITION>() as u32,
                None,
            )
        }
        FfbEffectKind::Rumble { .. } => Err(RemoteSteerError::UnsupportedOperation(
            "DirectInput wheel backend does not map rumble-only effects",
        )),
        FfbEffectKind::Custom { .. } => Err(RemoteSteerError::UnsupportedOperation(
            "DirectInput custom force effects are not implemented",
        )),
    }
}

unsafe fn create_effect_with_params(
    device: &IDirectInputDevice8W,
    axis_object_id: u32,
    master_gain: u16,
    source: &FfbEffect,
    guid: &GUID,
    params: *mut c_void,
    params_size: u32,
    envelope: Option<FfbEnvelope>,
) -> Result<IDirectInputEffect> {
    let mut axes = [axis_object_id];
    let mut direction = [direction_to_direct_input_axis(source.direction)];
    let mut envelope = envelope.map(direct_input_envelope);
    let mut di_effect = DIEFFECT {
        dwSize: size_of::<DIEFFECT>() as u32,
        dwFlags: DIEFF_CARTESIAN | DIEFF_OBJECTIDS,
        dwDuration: duration_to_direct_input(source.replay.length_ms),
        dwSamplePeriod: 0,
        dwGain: scale_u16_to_di(master_gain),
        dwTriggerButton: DIEB_NOTRIGGER,
        dwTriggerRepeatInterval: u32::from(source.trigger_interval_ms) * 1000,
        cAxes: axes.len() as u32,
        rgdwAxes: axes.as_mut_ptr(),
        rglDirection: direction.as_mut_ptr(),
        lpEnvelope: envelope
            .as_mut()
            .map_or(null_mut(), |value| value as *mut DIENVELOPE),
        cbTypeSpecificParams: params_size,
        lpvTypeSpecificParams: params,
        dwStartDelay: u32::from(source.replay.delay_ms) * 1000,
    };

    let mut out = None;
    device
        .CreateEffect(guid, &mut di_effect, &mut out, None)
        .map_err(|err| windows_error("DirectInput CreateEffect", err))?;
    out.ok_or_else(|| RemoteSteerError::Backend("DirectInput CreateEffect returned null".into()))
}

unsafe fn try_set_axis_range(
    device: &IDirectInputDevice8W,
    offset: u32,
    minimum: i32,
    maximum: i32,
) {
    let _ = set_axis_range(device, offset, minimum, maximum);
}

unsafe fn set_axis_range(
    device: &IDirectInputDevice8W,
    offset: u32,
    minimum: i32,
    maximum: i32,
) -> Result<()> {
    let mut property = DIPROPRANGE {
        diph: property_header::<DIPROPRANGE>(DIPH_BYOFFSET, offset),
        lMin: minimum,
        lMax: maximum,
    };
    device
        .SetProperty(
            &DIPROP_RANGE,
            &mut property as *mut DIPROPRANGE as *mut DIPROPHEADER,
        )
        .map_err(|err| windows_error("DirectInput SetProperty DIPROP_RANGE", err))
}

unsafe fn set_device_dword_property(
    device: &IDirectInputDevice8W,
    property_guid: &GUID,
    value: u32,
) -> Result<()> {
    let mut property = DIPROPDWORD {
        diph: property_header::<DIPROPDWORD>(DIPH_DEVICE, 0),
        dwData: value,
    };
    device
        .SetProperty(
            property_guid,
            &mut property as *mut DIPROPDWORD as *mut DIPROPHEADER,
        )
        .map_err(|err| windows_error("DirectInput SetProperty DWORD", err))
}

fn property_header<T>(how: u32, object: u32) -> DIPROPHEADER {
    DIPROPHEADER {
        dwSize: size_of::<T>() as u32,
        dwHeaderSize: size_of::<DIPROPHEADER>() as u32,
        dwObj: object,
        dwHow: how,
    }
}

fn snapshot_from_dijoystate(seq: u64, state: &DIJOYSTATE2) -> WheelStateSnapshot {
    let (hat_x, hat_y) = pov_to_hat(state.rgdwPOV[0]);
    let buttons = (0..13)
        .map(|index| ButtonValue {
            linux_code: 0x120 + index,
            pressed: state.rgbButtons[index as usize] & 0x80 != 0,
        })
        .collect();

    WheelStateSnapshot {
        seq,
        timestamp_micros: now_micros(),
        axes: vec![
            AxisValue {
                axis: AxisKind::Wheel,
                value: state.lX,
            },
            AxisValue {
                axis: AxisKind::PedalY,
                value: state.lY,
            },
            AxisValue {
                axis: AxisKind::PedalRz,
                value: state.lRz,
            },
            AxisValue {
                axis: AxisKind::Throttle,
                value: state.lZ,
            },
            AxisValue {
                axis: AxisKind::HatX,
                value: hat_x,
            },
            AxisValue {
                axis: AxisKind::HatY,
                value: hat_y,
            },
        ],
        buttons,
    }
}

fn pov_to_hat(value: u32) -> (i32, i32) {
    if value == u32::MAX {
        return (0, 0);
    }

    let degrees = value / 100;
    match degrees {
        0..=22 | 338..=359 => (0, -1),
        23..=67 => (1, -1),
        68..=112 => (1, 0),
        113..=157 => (1, 1),
        158..=202 => (0, 1),
        203..=247 => (-1, 1),
        248..=292 => (-1, 0),
        293..=337 => (-1, -1),
        _ => (0, 0),
    }
}

fn direct_input_envelope(envelope: FfbEnvelope) -> DIENVELOPE {
    DIENVELOPE {
        dwSize: size_of::<DIENVELOPE>() as u32,
        dwAttackLevel: scale_u16_to_di(envelope.attack_level),
        dwAttackTime: u32::from(envelope.attack_length_ms) * 1000,
        dwFadeLevel: scale_u16_to_di(envelope.fade_level),
        dwFadeTime: u32::from(envelope.fade_length_ms) * 1000,
    }
}

fn condition_axis_to_direct_input(axis: remote_steer_core::ConditionAxis) -> DICONDITION {
    DICONDITION {
        lOffset: scale_i16_to_di(axis.center),
        lPositiveCoefficient: scale_i16_to_di(axis.right_coefficient),
        lNegativeCoefficient: scale_i16_to_di(axis.left_coefficient),
        dwPositiveSaturation: scale_u16_to_di(axis.right_saturation),
        dwNegativeSaturation: scale_u16_to_di(axis.left_saturation),
        lDeadBand: scale_u16_to_di(axis.deadband) as i32,
    }
}

fn duration_to_direct_input(length_ms: u16) -> u32 {
    if length_ms == 0 {
        u32::MAX
    } else {
        u32::from(length_ms) * 1000
    }
}

fn scale_i16_to_di(value: i16) -> i32 {
    ((i32::from(value) * DI_FFNOMINALMAX as i32) / i32::from(i16::MAX))
        .clamp(-(DI_FFNOMINALMAX as i32), DI_FFNOMINALMAX as i32)
}

fn scale_i16_magnitude_to_di(value: i16) -> u32 {
    scale_i16_to_di(value).unsigned_abs().min(DI_FFNOMINALMAX)
}

fn scale_u16_to_di(value: u16) -> u32 {
    ((u32::from(value) * DI_FFNOMINALMAX) / u32::from(u16::MAX)).min(DI_FFNOMINALMAX)
}

fn phase_to_direct_input(phase: u16) -> u32 {
    (u32::from(phase) * 35_999) / u32::from(u16::MAX)
}

fn direction_to_direct_input_axis(direction: u16) -> i32 {
    let radians = (f64::from(direction) / 65_536.0) * std::f64::consts::TAU;
    let projected = (radians.sin() * f64::from(DI_FFNOMINALMAX)).round() as i32;
    projected.clamp(-(DI_FFNOMINALMAX as i32), DI_FFNOMINALMAX as i32)
}

fn offset_u32(offset: usize) -> u32 {
    offset.try_into().expect("DIJOYSTATE2 offset fits in u32")
}

fn vid_pid_from_guid_product(guid: GUID) -> (u16, u16) {
    ((guid.data1 & 0xffff) as u16, (guid.data1 >> 16) as u16)
}

fn utf16z_to_string(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|char| *char == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn guid_label(guid: &GUID) -> &'static str {
    if *guid == GUID_XAxis {
        "GUID_XAxis"
    } else if *guid == GUID_YAxis {
        "GUID_YAxis"
    } else if *guid == GUID_ZAxis {
        "GUID_ZAxis"
    } else if *guid == GUID_RxAxis {
        "GUID_RxAxis"
    } else if *guid == GUID_RyAxis {
        "GUID_RyAxis"
    } else if *guid == GUID_RzAxis {
        "GUID_RzAxis"
    } else if *guid == GUID_Slider {
        "GUID_Slider"
    } else if *guid == GUID_POV {
        "GUID_POV"
    } else if *guid == GUID_Button {
        "GUID_Button"
    } else if *guid == GUID_ConstantForce {
        "GUID_ConstantForce"
    } else if *guid == GUID_RampForce {
        "GUID_RampForce"
    } else if *guid == GUID_Square {
        "GUID_Square"
    } else if *guid == GUID_Sine {
        "GUID_Sine"
    } else if *guid == GUID_Triangle {
        "GUID_Triangle"
    } else if *guid == GUID_SawtoothUp {
        "GUID_SawtoothUp"
    } else if *guid == GUID_SawtoothDown {
        "GUID_SawtoothDown"
    } else if *guid == GUID_Spring {
        "GUID_Spring"
    } else if *guid == GUID_Damper {
        "GUID_Damper"
    } else if *guid == GUID_Inertia {
        "GUID_Inertia"
    } else if *guid == GUID_Friction {
        "GUID_Friction"
    } else {
        "UNKNOWN"
    }
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros() as u64)
        .unwrap_or_default()
}

fn is_reacquire_error(error: &windows::core::Error) -> bool {
    let code = error.code();
    code == DIERR_INPUTLOST || code == DIERR_NOTACQUIRED
}

fn is_not_implemented_error(error: &windows::core::Error) -> bool {
    error.code().0 as u32 == 0x8000_4001
}

fn windows_error(context: &str, error: windows::core::Error) -> RemoteSteerError {
    RemoteSteerError::Backend(format!("{context}: {error}"))
}
