use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EffectId(pub i16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PeriodicWaveform {
    Sine,
    Square,
    Triangle,
    SawUp,
    SawDown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfbReplay {
    pub length_ms: u16,
    pub delay_ms: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfbEnvelope {
    pub attack_length_ms: u16,
    pub attack_level: u16,
    pub fade_length_ms: u16,
    pub fade_level: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionAxis {
    pub right_saturation: u16,
    pub left_saturation: u16,
    pub right_coefficient: i16,
    pub left_coefficient: i16,
    pub deadband: u16,
    pub center: i16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FfbEffectKind {
    Constant {
        level: i16,
        envelope: FfbEnvelope,
    },
    Periodic {
        waveform: PeriodicWaveform,
        period_ms: u16,
        magnitude: i16,
        offset: i16,
        phase: u16,
        envelope: FfbEnvelope,
    },
    Ramp {
        start_level: i16,
        end_level: i16,
        envelope: FfbEnvelope,
    },
    Condition {
        condition: ConditionKind,
        axes: [ConditionAxis; 2],
    },
    Rumble {
        strong_magnitude: u16,
        weak_magnitude: u16,
    },
    Custom {
        sample_period_ms: u16,
        samples: Vec<i16>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConditionKind {
    Spring,
    Damper,
    Friction,
    Inertia,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfbEffect {
    pub id: EffectId,
    pub direction: u16,
    pub trigger_button: u16,
    pub trigger_interval_ms: u16,
    pub replay: FfbReplay,
    pub kind: FfbEffectKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FfbCommandKind {
    Upload {
        effect: FfbEffect,
    },
    Update {
        effect: FfbEffect,
    },
    Erase {
        effect_id: EffectId,
    },
    Play {
        effect_id: EffectId,
        repetitions: i32,
    },
    Stop {
        effect_id: EffectId,
    },
    SetGain {
        gain: u16,
    },
    SetAutocenter {
        magnitude: u16,
    },
    ResetState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfbCommand {
    pub command_id: u64,
    pub kind: FfbCommandKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FfbReplyKind {
    Ack,
    Rejected { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FfbReply {
    pub command_id: u64,
    pub kind: FfbReplyKind,
}

impl Default for FfbEnvelope {
    fn default() -> Self {
        Self {
            attack_length_ms: 0,
            attack_level: 0,
            fade_length_ms: 0,
            fade_level: 0,
        }
    }
}

impl Default for FfbReplay {
    fn default() -> Self {
        Self {
            length_ms: 0,
            delay_ms: 0,
        }
    }
}
