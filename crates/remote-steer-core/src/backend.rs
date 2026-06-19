use crate::{FfbCommand, FfbReply, Result, WheelProfile, WheelStateSnapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendCapabilities {
    pub profile: WheelProfile,
    pub can_read_input: bool,
    pub can_apply_ffb: bool,
    pub can_inject_input: bool,
    pub can_capture_ffb: bool,
}

pub trait PhysicalWheelBackend: Send {
    fn capabilities(&self) -> BackendCapabilities;
    fn poll_input(&mut self) -> Result<Option<WheelStateSnapshot>>;
    fn apply_ffb(&mut self, command: FfbCommand) -> Result<FfbReply>;
}

pub trait VirtualWheelBackend: Send {
    fn capabilities(&self) -> BackendCapabilities;
    fn inject_input(&mut self, snapshot: WheelStateSnapshot) -> Result<()>;
    fn poll_ffb(&mut self) -> Result<Option<FfbCommand>>;
    fn complete_ffb(&mut self, reply: FfbReply) -> Result<()>;
}
