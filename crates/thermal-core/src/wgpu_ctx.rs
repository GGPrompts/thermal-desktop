use wgpu::{Adapter, Device, Instance, Queue};

pub struct WgpuContext {
    pub instance: Instance,
    pub adapter: Adapter,
    pub device: Device,
    pub queue: Queue,
}

impl WgpuContext {
    /// Synchronously create a WgpuContext with low-power defaults suitable
    /// for all thermal components. Panics if no adapter is found.
    pub async fn new() -> Self {
        let instance = Instance::default();
        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                force_fallback_adapter: false,
                compatible_surface: None,
            })
            .await
            .expect("thermal-core: no wgpu adapter found");
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor::default(), None)
            .await
            .expect("thermal-core: failed to create wgpu device");
        Self { instance, adapter, device, queue }
    }
}
