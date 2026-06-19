#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod stub {
    use remote_steer_core::{
        BackendCapabilities, FfbCommand, FfbReply, PhysicalWheelBackend, RemoteSteerError, Result,
        VirtualWheelBackend, WheelStateSnapshot,
    };

    pub struct LinuxPhysicalBackend;
    pub struct LinuxVirtualBackend;

    impl LinuxPhysicalBackend {
        pub fn open_default() -> Result<Self> {
            Err(RemoteSteerError::BackendUnavailable("linux physical"))
        }
    }

    impl LinuxVirtualBackend {
        pub fn create_t150() -> Result<Self> {
            Err(RemoteSteerError::BackendUnavailable("linux virtual"))
        }
    }

    impl PhysicalWheelBackend for LinuxPhysicalBackend {
        fn capabilities(&self) -> BackendCapabilities {
            unreachable!("unavailable backend")
        }

        fn poll_input(&mut self) -> Result<Option<WheelStateSnapshot>> {
            Err(RemoteSteerError::BackendUnavailable("linux physical"))
        }

        fn apply_ffb(&mut self, _command: FfbCommand) -> Result<FfbReply> {
            Err(RemoteSteerError::BackendUnavailable("linux physical"))
        }
    }

    impl VirtualWheelBackend for LinuxVirtualBackend {
        fn capabilities(&self) -> BackendCapabilities {
            unreachable!("unavailable backend")
        }

        fn inject_input(&mut self, _snapshot: WheelStateSnapshot) -> Result<()> {
            Err(RemoteSteerError::BackendUnavailable("linux virtual"))
        }

        fn poll_ffb(&mut self) -> Result<Option<FfbCommand>> {
            Err(RemoteSteerError::BackendUnavailable("linux virtual"))
        }

        fn complete_ffb(&mut self, _reply: FfbReply) -> Result<()> {
            Err(RemoteSteerError::BackendUnavailable("linux virtual"))
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub use stub::*;
