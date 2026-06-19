use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::mem::MaybeUninit;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use input_linux::{
    sys, AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EvdevHandle, EventKind, ForceFeedbackKind,
    InputId, Key, UInputHandle,
};
use remote_steer_core::{
    profile_by_id, AxisKind, AxisValue, BackendCapabilities, ButtonValue, ConditionAxis,
    ConditionKind, EffectId, FfbCommand, FfbCommandKind, FfbEffect, FfbEffectKind, FfbEnvelope,
    FfbReplay, FfbReply, FfbReplyKind, PeriodicWaveform, PhysicalWheelBackend, RemoteSteerError,
    Result, VirtualWheelBackend, WheelProfileId, WheelStateSnapshot,
};
use tracing::debug;

const COMMAND_NAMESPACE_INLINE: u64 = 0x4000_0000_0000_0000;
const COMMAND_NAMESPACE_UPLOAD: u64 = 0x8000_0000_0000_0000;
const COMMAND_NAMESPACE_ERASE: u64 = 0x8100_0000_0000_0000;
const COMMAND_NAMESPACE_PAYLOAD_MASK: u64 = 0x00ff_ffff_ffff_ffff;

pub struct LinuxPhysicalBackend {
    event_path: PathBuf,
    profile: remote_steer_core::WheelProfile,
}

pub struct LinuxVirtualBackend {
    handle: UInputHandle<File>,
    profile: remote_steer_core::WheelProfile,
    pending_uploads: HashMap<u64, sys::uinput_ff_upload>,
    pending_erases: HashMap<u64, sys::uinput_ff_erase>,
    pending_commands: VecDeque<FfbCommand>,
    next_command_seq: u64,
    last_axes: HashMap<AxisKind, i32>,
    last_buttons: HashMap<u16, bool>,
}

#[derive(Debug, Clone)]
pub struct LinuxEventProbe {
    pub path: PathBuf,
    pub name: String,
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
    pub ev_bits: String,
    pub key_bits: String,
    pub abs_bits: String,
    pub ff_bits: String,
}

impl LinuxPhysicalBackend {
    pub fn open_default() -> Result<Self> {
        let probe = probe_t150_event()?.ok_or_else(|| {
            RemoteSteerError::DeviceNotFound("linux event device 044f:b677".to_string())
        })?;
        Ok(Self {
            event_path: probe.path,
            profile: profile_by_id(WheelProfileId::T150),
        })
    }

    pub fn event_path(&self) -> &Path {
        &self.event_path
    }
}

impl LinuxVirtualBackend {
    pub fn create_t150() -> Result<Self> {
        let profile = profile_by_id(WheelProfileId::T150);
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open("/dev/uinput")?;
        let handle = UInputHandle::new(file);

        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;
        handle.set_evbit(EventKind::ForceFeedback)?;
        for axis in &profile.axes {
            handle.set_absbit(axis_from_linux_code(axis.linux_code)?)?;
        }
        for button in &profile.buttons {
            handle.set_keybit(key_from_linux_code(button.linux_code)?)?;
        }
        for kind in t150_ff_kinds() {
            handle.set_ffbit(kind)?;
        }

        let id = InputId {
            bustype: profile.usb.bustype,
            vendor: profile.usb.vendor,
            product: profile.usb.product,
            version: profile.usb.version,
        };
        let abs = profile
            .axes
            .iter()
            .map(|axis| {
                Ok(AbsoluteInfoSetup {
                    axis: axis_from_linux_code(axis.linux_code)?,
                    info: AbsoluteInfo {
                        value: 0,
                        minimum: axis.minimum,
                        maximum: axis.maximum,
                        fuzz: axis.fuzz,
                        flat: axis.flat,
                        resolution: axis.resolution,
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        handle.create(
            &id,
            profile.event_name.as_bytes(),
            profile.ffb.max_effects as u32,
            &abs,
        )?;

        Ok(Self {
            handle,
            profile,
            pending_uploads: HashMap::new(),
            pending_erases: HashMap::new(),
            pending_commands: VecDeque::new(),
            next_command_seq: 0,
            last_axes: HashMap::new(),
            last_buttons: HashMap::new(),
        })
    }

    pub fn evdev_path(&self) -> Result<PathBuf> {
        Ok(self.handle.evdev_path()?)
    }

    fn next_command_id(&mut self, namespace: u64) -> u64 {
        next_namespaced_command_id(&mut self.next_command_seq, namespace)
    }
}

impl Drop for LinuxVirtualBackend {
    fn drop(&mut self) {
        let _ = self.handle.dev_destroy();
    }
}

impl PhysicalWheelBackend for LinuxPhysicalBackend {
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
        Err(RemoteSteerError::UnsupportedOperation(
            "linux physical input polling is not wired yet",
        ))
    }

    fn apply_ffb(&mut self, _command: FfbCommand) -> Result<FfbReply> {
        Err(RemoteSteerError::UnsupportedOperation(
            "linux physical FFB replay is not wired yet",
        ))
    }
}

impl VirtualWheelBackend for LinuxVirtualBackend {
    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            profile: self.profile.clone(),
            can_read_input: false,
            can_apply_ffb: false,
            can_inject_input: true,
            can_capture_ffb: true,
        }
    }

    fn inject_input(&mut self, snapshot: WheelStateSnapshot) -> Result<()> {
        let events = input_events_for_snapshot(
            &self.profile,
            &mut self.last_axes,
            &mut self.last_buttons,
            snapshot,
        )?;
        if events.is_empty() {
            return Ok(());
        }
        self.handle.write(&events)?;
        Ok(())
    }

    fn poll_ffb(&mut self) -> Result<Option<FfbCommand>> {
        if let Some(command) = self.pending_commands.pop_front() {
            return Ok(Some(command));
        }

        let mut events = [raw_event(sys::EV_SYN as u16, sys::SYN_REPORT as u16, 0); 16];
        let count = match self.handle.read(&mut events) {
            Ok(count) => count,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
            Err(err) => return Err(err.into()),
        };

        for event in events.into_iter().take(count) {
            match (event.type_ as i32, event.code as i32) {
                (sys::EV_UINPUT, sys::UI_FF_UPLOAD) => {
                    let request_id = event.value as u64;
                    let command_id = self.next_command_id(COMMAND_NAMESPACE_UPLOAD);
                    debug!(request_id, command_id, "uinput ff upload requested");
                    let mut upload = zeroed_upload(request_id);
                    self.handle.ff_upload_begin(&mut upload)?;
                    let effect = ffb_effect_from_sys(upload.effect)?;
                    debug!(
                        request_id,
                        command_id,
                        effect_id = upload.effect.id,
                        effect_type = upload.effect.type_,
                        "uinput ff upload begin completed"
                    );
                    self.pending_uploads.insert(command_id, upload);
                    self.pending_commands.push_back(FfbCommand {
                        command_id,
                        kind: FfbCommandKind::Upload { effect },
                    });
                }
                (sys::EV_UINPUT, sys::UI_FF_ERASE) => {
                    let request_id = event.value as u64;
                    let command_id = self.next_command_id(COMMAND_NAMESPACE_ERASE);
                    debug!(request_id, command_id, "uinput ff erase requested");
                    let mut erase = zeroed_erase(request_id);
                    self.handle.ff_erase_begin(&mut erase)?;
                    let effect_id = EffectId(erase.effect_id as i16);
                    debug!(
                        request_id,
                        command_id,
                        effect_id = erase.effect_id,
                        "uinput ff erase begin completed"
                    );
                    self.pending_erases.insert(command_id, erase);
                    self.pending_commands.push_back(FfbCommand {
                        command_id,
                        kind: FfbCommandKind::Erase { effect_id },
                    });
                }
                (sys::EV_FF, code) if code == sys::FF_GAIN as i32 => {
                    let command_id = self.next_command_id(COMMAND_NAMESPACE_INLINE);
                    self.pending_commands.push_back(FfbCommand {
                        command_id,
                        kind: FfbCommandKind::SetGain {
                            gain: event.value.clamp(0, u16::MAX as i32) as u16,
                        },
                    });
                }
                (sys::EV_FF, code) if code == sys::FF_AUTOCENTER as i32 => {
                    let command_id = self.next_command_id(COMMAND_NAMESPACE_INLINE);
                    self.pending_commands.push_back(FfbCommand {
                        command_id,
                        kind: FfbCommandKind::SetAutocenter {
                            magnitude: event.value.clamp(0, u16::MAX as i32) as u16,
                        },
                    });
                }
                (sys::EV_FF, code) if event.value == 0 => {
                    let command_id = self.next_command_id(COMMAND_NAMESPACE_INLINE);
                    self.pending_commands.push_back(FfbCommand {
                        command_id,
                        kind: FfbCommandKind::Stop {
                            effect_id: EffectId(code as i16),
                        },
                    });
                }
                (sys::EV_FF, code) => {
                    let command_id = self.next_command_id(COMMAND_NAMESPACE_INLINE);
                    self.pending_commands.push_back(FfbCommand {
                        command_id,
                        kind: FfbCommandKind::Play {
                            effect_id: EffectId(code as i16),
                            repetitions: event.value,
                        },
                    });
                }
                _ => {}
            }
        }
        Ok(self.pending_commands.pop_front())
    }

    fn complete_ffb(&mut self, reply: FfbReply) -> Result<()> {
        let retval = match &reply.kind {
            FfbReplyKind::Ack => 0,
            FfbReplyKind::Rejected { .. } => -1,
        };
        if let Some(mut upload) = self.pending_uploads.remove(&reply.command_id) {
            upload.retval = retval;
            debug!(
                command_id = reply.command_id,
                retval, "uinput ff upload completing"
            );
            self.handle.ff_upload_end(&upload)?;
            return Ok(());
        }
        if let Some(mut erase) = self.pending_erases.remove(&reply.command_id) {
            erase.retval = retval;
            debug!(
                command_id = reply.command_id,
                retval, "uinput ff erase completing"
            );
            self.handle.ff_erase_end(&erase)?;
            return Ok(());
        }
        Ok(())
    }
}

fn input_events_for_snapshot(
    profile: &remote_steer_core::WheelProfile,
    last_axes: &mut HashMap<AxisKind, i32>,
    last_buttons: &mut HashMap<u16, bool>,
    snapshot: WheelStateSnapshot,
) -> Result<Vec<sys::input_event>> {
    let mut events = Vec::with_capacity(snapshot.axes.len() + snapshot.buttons.len() + 1);
    for axis in snapshot.axes {
        let axis_profile = profile
            .axes
            .iter()
            .find(|profile| profile.kind == axis.axis)
            .ok_or_else(|| RemoteSteerError::Backend(format!("unknown axis {:?}", axis.axis)))?;
        let value = axis.value.clamp(axis_profile.minimum, axis_profile.maximum);
        if last_axes.get(&axis.axis).copied() != Some(value) {
            events.push(raw_event(
                sys::EV_ABS as u16,
                axis_profile.linux_code,
                value,
            ));
            last_axes.insert(axis.axis, value);
        }
    }
    for button in snapshot.buttons {
        if !profile
            .buttons
            .iter()
            .any(|profile| profile.linux_code == button.linux_code)
        {
            return Err(RemoteSteerError::Backend(format!(
                "unknown button {}",
                button.linux_code
            )));
        }
        if last_buttons.get(&button.linux_code).copied() != Some(button.pressed) {
            events.push(raw_event(
                sys::EV_KEY as u16,
                button.linux_code,
                i32::from(button.pressed),
            ));
            last_buttons.insert(button.linux_code, button.pressed);
        }
    }
    if !events.is_empty() {
        events.push(raw_event(sys::EV_SYN as u16, sys::SYN_REPORT as u16, 0));
    }
    Ok(events)
}

pub fn probe_t150_event() -> Result<Option<LinuxEventProbe>> {
    let input_root = Path::new("/sys/class/input");
    for entry in fs::read_dir(input_root)? {
        let entry = entry?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with("event") {
            continue;
        }
        let device = entry.path().join("device");
        let vendor = read_hex_u16(device.join("id/vendor"))?;
        let product = read_hex_u16(device.join("id/product"))?;
        if vendor != 0x044f || product != 0xb677 {
            continue;
        }
        return Ok(Some(LinuxEventProbe {
            path: Path::new("/dev/input").join(file_name.as_ref()),
            name: read_trimmed(device.join("name"))?,
            bustype: read_hex_u16(device.join("id/bustype"))?,
            vendor,
            product,
            version: read_hex_u16(device.join("id/version"))?,
            ev_bits: read_trimmed(device.join("capabilities/ev"))?,
            key_bits: read_trimmed(device.join("capabilities/key"))?,
            abs_bits: read_trimmed(device.join("capabilities/abs"))?,
            ff_bits: read_trimmed(device.join("capabilities/ff"))?,
        }));
    }
    Ok(None)
}

pub fn snapshot_from_probe(probe: &LinuxEventProbe) -> WheelStateSnapshot {
    let mut snapshot = WheelStateSnapshot::empty(0, 0);
    snapshot.axes = profile_by_id(WheelProfileId::T150)
        .axes
        .iter()
        .map(|axis| AxisValue {
            axis: axis.kind,
            value: 0,
        })
        .collect();
    snapshot.buttons = profile_by_id(WheelProfileId::T150)
        .buttons
        .iter()
        .map(|button| ButtonValue {
            linux_code: button.linux_code,
            pressed: false,
        })
        .collect();
    let _ = probe;
    snapshot
}

pub fn play_ffb_test_effect(
    event_path: impl AsRef<Path>,
    effect: FfbEffect,
    duration: Duration,
) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(event_path.as_ref())?;
    let handle = EvdevHandle::new(file);
    let mut effect = ffb_effect_to_sys(effect)?;

    handle.send_force_feedback(&mut effect)?;
    let effect_id = effect.id;
    handle.write(&[raw_event(sys::EV_FF as u16, effect_id as u16, 1)])?;
    thread::sleep(duration);
    handle.write(&[raw_event(sys::EV_FF as u16, effect_id as u16, 0)])?;
    handle.erase_force_feedback(effect_id)?;
    Ok(())
}

fn read_trimmed(path: impl AsRef<Path>) -> Result<String> {
    Ok(fs::read_to_string(path)?.trim().to_string())
}

fn read_hex_u16(path: impl AsRef<Path>) -> Result<u16> {
    let value = read_trimmed(path)?;
    u16::from_str_radix(value.trim_start_matches("0x"), 16)
        .map_err(|err| RemoteSteerError::Backend(err.to_string()))
}

fn axis_from_linux_code(code: u16) -> Result<AbsoluteAxis> {
    match code {
        0x00 => Ok(AbsoluteAxis::X),
        0x01 => Ok(AbsoluteAxis::Y),
        0x05 => Ok(AbsoluteAxis::RZ),
        0x06 => Ok(AbsoluteAxis::Throttle),
        0x10 => Ok(AbsoluteAxis::Hat0X),
        0x11 => Ok(AbsoluteAxis::Hat0Y),
        _ => Err(RemoteSteerError::Backend(format!(
            "unsupported T150 axis code {code}"
        ))),
    }
}

fn key_from_linux_code(code: u16) -> Result<Key> {
    match code {
        0x120 => Ok(Key::ButtonTrigger),
        0x121 => Ok(Key::ButtonThumb),
        0x122 => Ok(Key::ButtonThumb2),
        0x123 => Ok(Key::ButtonTop),
        0x124 => Ok(Key::ButtonTop2),
        0x125 => Ok(Key::ButtonPinkie),
        0x126 => Ok(Key::ButtonBase),
        0x127 => Ok(Key::ButtonBase2),
        0x128 => Ok(Key::ButtonBase3),
        0x129 => Ok(Key::ButtonBase4),
        0x12a => Ok(Key::ButtonBase5),
        0x12b => Ok(Key::ButtonBase6),
        0x12c => Ok(Key::ButtonDead),
        _ => Err(RemoteSteerError::Backend(format!(
            "unsupported T150 button code {code}"
        ))),
    }
}

fn t150_ff_kinds() -> [ForceFeedbackKind; 9] {
    [
        ForceFeedbackKind::Periodic,
        ForceFeedbackKind::Constant,
        ForceFeedbackKind::Spring,
        ForceFeedbackKind::Damper,
        ForceFeedbackKind::Sine,
        ForceFeedbackKind::Gain,
        ForceFeedbackKind::Autocenter,
        ForceFeedbackKind::Unknown62,
        ForceFeedbackKind::Unknown68,
    ]
}

pub fn ack(command: FfbCommand) -> FfbReply {
    FfbReply {
        command_id: command.command_id,
        kind: FfbReplyKind::Ack,
    }
}

fn raw_event(type_: u16, code: u16, value: i32) -> sys::input_event {
    sys::input_event {
        time: sys::timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_,
        code,
        value,
    }
}

fn zeroed_upload(request_id: u64) -> sys::uinput_ff_upload {
    let mut upload: sys::uinput_ff_upload = unsafe { MaybeUninit::zeroed().assume_init() };
    upload.request_id = request_id as u32;
    upload
}

fn zeroed_erase(request_id: u64) -> sys::uinput_ff_erase {
    let mut erase: sys::uinput_ff_erase = unsafe { MaybeUninit::zeroed().assume_init() };
    erase.request_id = request_id as u32;
    erase
}

fn next_namespaced_command_id(sequence: &mut u64, namespace: u64) -> u64 {
    let command_id = namespace | (*sequence & COMMAND_NAMESPACE_PAYLOAD_MASK);
    *sequence = sequence.wrapping_add(1);
    command_id
}

fn ffb_effect_from_sys(effect: sys::ff_effect) -> Result<FfbEffect> {
    let replay = FfbReplay {
        length_ms: effect.replay.length,
        delay_ms: effect.replay.delay,
    };
    let trigger_button = effect.trigger.button;
    let trigger_interval_ms = effect.trigger.interval;
    let id = EffectId(effect.id);
    let direction = effect.direction;
    let union: &sys::ff_effect_union = (&effect).into();
    let kind = match effect.type_ {
        sys::FF_CONSTANT => {
            let constant = union.constant();
            FfbEffectKind::Constant {
                level: constant.level,
                envelope: envelope_from_sys(constant.envelope),
            }
        }
        sys::FF_PERIODIC => {
            let periodic = union.periodic();
            FfbEffectKind::Periodic {
                waveform: waveform_from_sys(periodic.waveform)?,
                period_ms: periodic.period,
                magnitude: periodic.magnitude,
                offset: periodic.offset,
                phase: periodic.phase,
                envelope: envelope_from_sys(periodic.envelope),
            }
        }
        sys::FF_RAMP => {
            let ramp = union.ramp();
            FfbEffectKind::Ramp {
                start_level: ramp.start_level,
                end_level: ramp.end_level,
                envelope: envelope_from_sys(ramp.envelope),
            }
        }
        sys::FF_SPRING | sys::FF_DAMPER | sys::FF_FRICTION | sys::FF_INERTIA => {
            let condition = condition_kind_from_sys(effect.type_)?;
            let axes = union.condition();
            FfbEffectKind::Condition {
                condition,
                axes: [
                    condition_axis_from_sys(axes[0]),
                    condition_axis_from_sys(axes[1]),
                ],
            }
        }
        sys::FF_RUMBLE => {
            let rumble = union.rumble();
            FfbEffectKind::Rumble {
                strong_magnitude: rumble.strong_magnitude,
                weak_magnitude: rumble.weak_magnitude,
            }
        }
        sys::FF_CUSTOM => {
            let periodic = union.periodic();
            FfbEffectKind::Custom {
                sample_period_ms: periodic.period,
                samples: Vec::new(),
            }
        }
        other => {
            return Err(RemoteSteerError::Backend(format!(
                "unsupported Linux FF effect type 0x{other:x}"
            )))
        }
    };
    Ok(FfbEffect {
        id,
        direction,
        trigger_button,
        trigger_interval_ms,
        replay,
        kind,
    })
}

fn ffb_effect_to_sys(effect: FfbEffect) -> Result<sys::ff_effect> {
    let mut out: sys::ff_effect = unsafe { MaybeUninit::zeroed().assume_init() };
    out.id = effect.id.0;
    out.direction = effect.direction;
    out.trigger = sys::ff_trigger {
        button: effect.trigger_button,
        interval: effect.trigger_interval_ms,
    };
    out.replay = sys::ff_replay {
        length: effect.replay.length_ms,
        delay: effect.replay.delay_ms,
    };

    match effect.kind {
        FfbEffectKind::Constant { level, envelope } => {
            out.type_ = sys::FF_CONSTANT;
            let union: &mut sys::ff_effect_union = (&mut out).into();
            *union.constant_mut() = sys::ff_constant_effect {
                level,
                envelope: envelope_to_sys(envelope),
            };
        }
        FfbEffectKind::Periodic {
            waveform,
            period_ms,
            magnitude,
            offset,
            phase,
            envelope,
        } => {
            out.type_ = sys::FF_PERIODIC;
            let union: &mut sys::ff_effect_union = (&mut out).into();
            *union.periodic_mut() = sys::ff_periodic_effect {
                waveform: waveform_to_sys(waveform),
                period: period_ms,
                magnitude,
                offset,
                phase,
                envelope: envelope_to_sys(envelope),
                custom_len: 0,
                custom_data: std::ptr::null_mut(),
            };
        }
        FfbEffectKind::Ramp {
            start_level,
            end_level,
            envelope,
        } => {
            out.type_ = sys::FF_RAMP;
            let union: &mut sys::ff_effect_union = (&mut out).into();
            *union.ramp_mut() = sys::ff_ramp_effect {
                start_level,
                end_level,
                envelope: envelope_to_sys(envelope),
            };
        }
        FfbEffectKind::Condition { condition, axes } => {
            out.type_ = condition_kind_to_sys(condition);
            let union: &mut sys::ff_effect_union = (&mut out).into();
            *union.condition_mut() = [
                condition_axis_to_sys(axes[0]),
                condition_axis_to_sys(axes[1]),
            ];
        }
        FfbEffectKind::Rumble {
            strong_magnitude,
            weak_magnitude,
        } => {
            out.type_ = sys::FF_RUMBLE;
            let union: &mut sys::ff_effect_union = (&mut out).into();
            *union.rumble_mut() = sys::ff_rumble_effect {
                strong_magnitude,
                weak_magnitude,
            };
        }
        FfbEffectKind::Custom { .. } => {
            return Err(RemoteSteerError::UnsupportedOperation(
                "custom FFB test effects are not supported on Linux evdev".into(),
            ));
        }
    }

    Ok(out)
}

fn envelope_from_sys(envelope: sys::ff_envelope) -> FfbEnvelope {
    FfbEnvelope {
        attack_length_ms: envelope.attack_length,
        attack_level: envelope.attack_level,
        fade_length_ms: envelope.fade_length,
        fade_level: envelope.fade_level,
    }
}

fn envelope_to_sys(envelope: FfbEnvelope) -> sys::ff_envelope {
    sys::ff_envelope {
        attack_length: envelope.attack_length_ms,
        attack_level: envelope.attack_level,
        fade_length: envelope.fade_length_ms,
        fade_level: envelope.fade_level,
    }
}

fn condition_axis_from_sys(axis: sys::ff_condition_effect) -> ConditionAxis {
    ConditionAxis {
        right_saturation: axis.right_saturation,
        left_saturation: axis.left_saturation,
        right_coefficient: axis.right_coeff,
        left_coefficient: axis.left_coeff,
        deadband: axis.deadband,
        center: axis.center,
    }
}

fn condition_axis_to_sys(axis: ConditionAxis) -> sys::ff_condition_effect {
    sys::ff_condition_effect {
        right_saturation: axis.right_saturation,
        left_saturation: axis.left_saturation,
        right_coeff: axis.right_coefficient,
        left_coeff: axis.left_coefficient,
        deadband: axis.deadband,
        center: axis.center,
    }
}

fn waveform_from_sys(waveform: u16) -> Result<PeriodicWaveform> {
    match waveform {
        sys::FF_SINE => Ok(PeriodicWaveform::Sine),
        sys::FF_SQUARE => Ok(PeriodicWaveform::Square),
        sys::FF_TRIANGLE => Ok(PeriodicWaveform::Triangle),
        sys::FF_SAW_UP => Ok(PeriodicWaveform::SawUp),
        sys::FF_SAW_DOWN => Ok(PeriodicWaveform::SawDown),
        other => Err(RemoteSteerError::Backend(format!(
            "unsupported Linux periodic waveform 0x{other:x}"
        ))),
    }
}

fn waveform_to_sys(waveform: PeriodicWaveform) -> u16 {
    match waveform {
        PeriodicWaveform::Sine => sys::FF_SINE,
        PeriodicWaveform::Square => sys::FF_SQUARE,
        PeriodicWaveform::Triangle => sys::FF_TRIANGLE,
        PeriodicWaveform::SawUp => sys::FF_SAW_UP,
        PeriodicWaveform::SawDown => sys::FF_SAW_DOWN,
    }
}

fn condition_kind_from_sys(kind: u16) -> Result<ConditionKind> {
    match kind {
        sys::FF_SPRING => Ok(ConditionKind::Spring),
        sys::FF_DAMPER => Ok(ConditionKind::Damper),
        sys::FF_FRICTION => Ok(ConditionKind::Friction),
        sys::FF_INERTIA => Ok(ConditionKind::Inertia),
        other => Err(RemoteSteerError::Backend(format!(
            "unsupported Linux condition effect 0x{other:x}"
        ))),
    }
}

fn condition_kind_to_sys(condition: ConditionKind) -> u16 {
    match condition {
        ConditionKind::Spring => sys::FF_SPRING,
        ConditionKind::Damper => sys::FF_DAMPER,
        ConditionKind::Friction => sys::FF_FRICTION,
        ConditionKind::Inertia => sys::FF_INERTIA,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        input_events_for_snapshot, next_namespaced_command_id, sys, COMMAND_NAMESPACE_ERASE,
        COMMAND_NAMESPACE_INLINE, COMMAND_NAMESPACE_PAYLOAD_MASK, COMMAND_NAMESPACE_UPLOAD,
    };
    use remote_steer_core::{
        profile_by_id, AxisKind, AxisValue, ButtonValue, WheelProfileId, WheelStateSnapshot,
    };
    use std::collections::HashMap;

    #[test]
    fn command_ids_are_namespaced_and_monotonic() {
        let mut sequence = 0;
        let upload_0 = next_namespaced_command_id(&mut sequence, COMMAND_NAMESPACE_UPLOAD);
        let upload_1 = next_namespaced_command_id(&mut sequence, COMMAND_NAMESPACE_UPLOAD);
        let erase_2 = next_namespaced_command_id(&mut sequence, COMMAND_NAMESPACE_ERASE);
        let inline_3 = next_namespaced_command_id(&mut sequence, COMMAND_NAMESPACE_INLINE);

        assert_eq!(upload_0, COMMAND_NAMESPACE_UPLOAD);
        assert_eq!(upload_1, COMMAND_NAMESPACE_UPLOAD | 1);
        assert_eq!(erase_2, COMMAND_NAMESPACE_ERASE | 2);
        assert_eq!(inline_3, COMMAND_NAMESPACE_INLINE | 3);
        assert_ne!(upload_0, erase_2);
        assert_ne!(upload_0, inline_3);
    }

    #[test]
    fn command_id_payload_wraps_inside_namespace() {
        let mut sequence = COMMAND_NAMESPACE_PAYLOAD_MASK;
        let last_payload = next_namespaced_command_id(&mut sequence, COMMAND_NAMESPACE_UPLOAD);
        let wrapped_payload = next_namespaced_command_id(&mut sequence, COMMAND_NAMESPACE_UPLOAD);

        assert_eq!(
            last_payload,
            COMMAND_NAMESPACE_UPLOAD | COMMAND_NAMESPACE_PAYLOAD_MASK
        );
        assert_eq!(wrapped_payload, COMMAND_NAMESPACE_UPLOAD);
    }

    #[test]
    fn input_events_are_clamped_and_deduplicated() {
        let profile = profile_by_id(WheelProfileId::T150);
        let mut last_axes = HashMap::new();
        let mut last_buttons = HashMap::new();
        let snapshot = WheelStateSnapshot {
            seq: 1,
            timestamp_micros: 0,
            axes: vec![
                AxisValue {
                    axis: AxisKind::Wheel,
                    value: -100,
                },
                AxisValue {
                    axis: AxisKind::HatX,
                    value: 7,
                },
            ],
            buttons: vec![ButtonValue {
                linux_code: 0x120,
                pressed: true,
            }],
        };

        let events = input_events_for_snapshot(
            &profile,
            &mut last_axes,
            &mut last_buttons,
            snapshot.clone(),
        )
        .unwrap();

        assert_eq!(events.len(), 4);
        assert_eq!(
            (events[0].type_, events[0].code, events[0].value),
            (sys::EV_ABS as u16, 0x00, 0)
        );
        assert_eq!(
            (events[1].type_, events[1].code, events[1].value),
            (sys::EV_ABS as u16, 0x10, 1)
        );
        assert_eq!(
            (events[2].type_, events[2].code, events[2].value),
            (sys::EV_KEY as u16, 0x120, 1)
        );
        assert_eq!(
            (events[3].type_, events[3].code, events[3].value),
            (sys::EV_SYN as u16, sys::SYN_REPORT as u16, 0)
        );

        let duplicate =
            input_events_for_snapshot(&profile, &mut last_axes, &mut last_buttons, snapshot)
                .unwrap();
        assert!(duplicate.is_empty());
    }

    #[test]
    fn input_events_emit_only_changed_values_after_initial_state() {
        let profile = profile_by_id(WheelProfileId::T150);
        let mut last_axes = HashMap::new();
        let mut last_buttons = HashMap::new();
        let initial = WheelStateSnapshot {
            seq: 1,
            timestamp_micros: 0,
            axes: vec![AxisValue {
                axis: AxisKind::Wheel,
                value: 0,
            }],
            buttons: vec![ButtonValue {
                linux_code: 0x120,
                pressed: false,
            }],
        };
        input_events_for_snapshot(&profile, &mut last_axes, &mut last_buttons, initial).unwrap();

        let changed = WheelStateSnapshot {
            seq: 2,
            timestamp_micros: 0,
            axes: vec![AxisValue {
                axis: AxisKind::Wheel,
                value: 1234,
            }],
            buttons: vec![ButtonValue {
                linux_code: 0x120,
                pressed: false,
            }],
        };
        let events =
            input_events_for_snapshot(&profile, &mut last_axes, &mut last_buttons, changed)
                .unwrap();

        assert_eq!(events.len(), 2);
        assert_eq!(
            (events[0].type_, events[0].code, events[0].value),
            (sys::EV_ABS as u16, 0x00, 1234)
        );
        assert_eq!(
            (events[1].type_, events[1].code, events[1].value),
            (sys::EV_SYN as u16, sys::SYN_REPORT as u16, 0)
        );
    }
}
