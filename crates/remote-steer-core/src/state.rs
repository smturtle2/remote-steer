use crate::profile::AxisKind;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisValue {
    pub axis: AxisKind,
    pub value: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ButtonValue {
    pub linux_code: u16,
    pub pressed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WheelStateSnapshot {
    pub seq: u64,
    pub timestamp_micros: u64,
    pub axes: Vec<AxisValue>,
    pub buttons: Vec<ButtonValue>,
}

impl WheelStateSnapshot {
    pub fn empty(seq: u64, timestamp_micros: u64) -> Self {
        Self {
            seq,
            timestamp_micros,
            axes: Vec::new(),
            buttons: Vec::new(),
        }
    }

    pub fn axis(&self, axis: AxisKind) -> Option<i32> {
        self.axes
            .iter()
            .find(|value| value.axis == axis)
            .map(|value| value.value)
    }

    pub fn button(&self, linux_code: u16) -> bool {
        self.buttons
            .iter()
            .any(|button| button.linux_code == linux_code && button.pressed)
    }
}
