#[cfg(windows)]
mod windows_backend;

#[cfg(windows)]
pub use windows_backend::*;

#[cfg(not(windows))]
mod stub {
    use remote_steer_core::{
        profile_by_id, BackendCapabilities, FfbCommand, FfbReply, PhysicalWheelBackend,
        RemoteSteerError, Result, WheelProfileId, WheelStateSnapshot,
    };

    pub struct WindowsPhysicalBackend;

    impl WindowsPhysicalBackend {
        pub fn open_t150() -> Result<Self> {
            Err(RemoteSteerError::BackendUnavailable("windows physical"))
        }
    }

    impl PhysicalWheelBackend for WindowsPhysicalBackend {
        fn capabilities(&self) -> BackendCapabilities {
            BackendCapabilities {
                profile: profile_by_id(WheelProfileId::T150),
                can_read_input: true,
                can_apply_ffb: true,
                can_inject_input: false,
                can_capture_ffb: false,
            }
        }

        fn poll_input(&mut self) -> Result<Option<WheelStateSnapshot>> {
            Err(RemoteSteerError::BackendUnavailable("windows physical"))
        }

        fn apply_ffb(&mut self, _command: FfbCommand) -> Result<FfbReply> {
            Err(RemoteSteerError::BackendUnavailable("windows physical"))
        }
    }
}

#[cfg(not(windows))]
pub use stub::*;
