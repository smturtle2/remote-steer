use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WheelProfileId {
    T150,
}

impl WheelProfileId {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::T150 => "t150",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct UsbIdentity {
    pub bustype: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AxisKind {
    Wheel,
    PedalY,
    PedalRz,
    Throttle,
    HatX,
    HatY,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AxisProfile {
    pub kind: AxisKind,
    pub linux_code: u16,
    pub minimum: i32,
    pub maximum: i32,
    pub flat: i32,
    pub fuzz: i32,
    pub resolution: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ButtonProfile {
    pub linux_code: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FfbCapability {
    Constant,
    Periodic,
    Spring,
    Damper,
    Friction,
    Inertia,
    Ramp,
    Rumble,
    Gain,
    Autocenter,
    Sine,
    Square,
    Triangle,
    SawUp,
    SawDown,
    Custom,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfbProfile {
    pub max_effects: u16,
    pub capabilities: Vec<FfbCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityBits {
    pub ev: &'static str,
    pub key: &'static str,
    pub abs: &'static str,
    pub ff: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WheelProfile {
    pub id: WheelProfileId,
    pub display_name: &'static str,
    pub event_name: &'static str,
    pub usb: UsbIdentity,
    pub capability_bits: CapabilityBits,
    pub axes: Vec<AxisProfile>,
    pub buttons: Vec<ButtonProfile>,
    pub ffb: FfbProfile,
}

pub fn t150_profile() -> WheelProfile {
    WheelProfile {
        id: WheelProfileId::T150,
        display_name: "Thrustmaster T150RS",
        event_name: "Thrustmaster Thrustmaster T150RS",
        usb: UsbIdentity {
            bustype: 0x0003,
            vendor: 0x044f,
            product: 0xb677,
            version: 0x0111,
        },
        capability_bits: CapabilityBits {
            ev: "20001b",
            key: "1fff00000000 0 0 0 0",
            abs: "30063",
            ff: "11c2f0000 0",
        },
        axes: vec![
            AxisProfile {
                kind: AxisKind::Wheel,
                linux_code: 0x00,
                minimum: 0,
                maximum: 65535,
                flat: 0,
                fuzz: 0,
                resolution: 0,
            },
            AxisProfile {
                kind: AxisKind::PedalY,
                linux_code: 0x01,
                minimum: 0,
                maximum: 255,
                flat: 0,
                fuzz: 0,
                resolution: 0,
            },
            AxisProfile {
                kind: AxisKind::PedalRz,
                linux_code: 0x05,
                minimum: 0,
                maximum: 255,
                flat: 0,
                fuzz: 0,
                resolution: 0,
            },
            AxisProfile {
                kind: AxisKind::Throttle,
                linux_code: 0x06,
                minimum: 0,
                maximum: 255,
                flat: 0,
                fuzz: 0,
                resolution: 0,
            },
            AxisProfile {
                kind: AxisKind::HatX,
                linux_code: 0x10,
                minimum: -1,
                maximum: 1,
                flat: 0,
                fuzz: 0,
                resolution: 0,
            },
            AxisProfile {
                kind: AxisKind::HatY,
                linux_code: 0x11,
                minimum: -1,
                maximum: 1,
                flat: 0,
                fuzz: 0,
                resolution: 0,
            },
        ],
        buttons: (0x120..=0x12c)
            .map(|linux_code| ButtonProfile { linux_code })
            .collect(),
        ffb: FfbProfile {
            max_effects: 96,
            capabilities: vec![
                FfbCapability::Constant,
                FfbCapability::Periodic,
                FfbCapability::Spring,
                FfbCapability::Damper,
                FfbCapability::Gain,
                FfbCapability::Autocenter,
                FfbCapability::Sine,
                FfbCapability::SawUp,
                FfbCapability::SawDown,
            ],
        },
    }
}

pub fn profile_by_id(id: WheelProfileId) -> WheelProfile {
    match id {
        WheelProfileId::T150 => t150_profile(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t150_identity_is_fixed() {
        let profile = t150_profile();
        assert_eq!(profile.usb.vendor, 0x044f);
        assert_eq!(profile.usb.product, 0xb677);
        assert_eq!(profile.ffb.max_effects, 96);
        assert!(profile.ffb.capabilities.contains(&FfbCapability::Constant));
    }
}
