//! Headless wgpu compute pipeline for GPU-accelerated thumbnail resize.
//!
//! Decoding and cropping still happen on the CPU (rayon). The GPU handles
//! the resize step, which is massively parallel — one thread per output pixel.
//! Images are batched (up to 32 at a time) to amortise upload/readback overhead.
//!
//! Falls back gracefully: if no Vulkan/GL adapter is found, `GpuResizer::new()`
//! returns `None` and the pipeline uses the existing CPU SIMD path.

use bytemuck::{Pod, Zeroable};

/// WGSL compute shader: samples source texture at UV, writes packed RGBA to output buffer.
const SHADER: &str = r#"
struct Params {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<storage, read_write> output: array<u32>;
@group(0) @binding(3) var<uniform> params: Params;

@compute @workgroup_size(16, 16)
fn resize(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.dst_width || gid.y >= params.dst_height) {
        return;
    }

    let uv = vec2f(
        (f32(gid.x) + 0.5) / f32(params.dst_width),
        (f32(gid.y) + 0.5) / f32(params.dst_height),
    );

    let color = textureSampleLevel(src, samp, uv, 0.0);
    let idx = gid.y * params.dst_width + gid.x;
    output[idx] = pack4x8unorm(color);
}
"#;

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct ResizeParams {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
}

/// Input for a single GPU resize operation.
pub struct ResizeInput {
    pub rgba_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Output of a single GPU resize operation.
pub struct ResizeOutput {
    pub rgba_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Headless wgpu compute pipeline for batched image resize.
pub struct GpuResizer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler_bilinear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
}

// GpuResizer is Send+Sync because wgpu Device/Queue are.
unsafe impl Send for GpuResizer {}
unsafe impl Sync for GpuResizer {}

impl GpuResizer {
    /// Try to initialise a headless wgpu device. Returns `None` if no GPU is available.
    pub fn new() -> Option<Self> {
        pollster::block_on(Self::init())
    }

    async fn init() -> Option<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends: wgpu::Backends::VULKAN | wgpu::Backends::GL,
            flags: wgpu::InstanceFlags::empty(),
            memory_budget_thresholds: Default::default(),
            backend_options: Default::default(),
            display: None,
        });

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: None,
                force_fallback_adapter: false,
            })
            .await
            .ok()?;

        let info = adapter.get_info();
        log::info!(
            "GPU resize adapter: {} ({:?})",
            info.name,
            info.backend
        );

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("lightview-gpu-resize"),
                ..Default::default()
            })
            .await
            .ok()?;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("resize-shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let bind_group_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("resize-bgl"),
                entries: &[
                    // binding 0: source texture
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Texture {
                            multisampled: false,
                            view_dimension: wgpu::TextureViewDimension::D2,
                            sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        },
                        count: None,
                    },
                    // binding 1: sampler
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
                    // binding 2: output storage buffer
                    wgpu::BindGroupLayoutEntry {
                        binding: 2,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: false },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    // binding 3: uniform params
                    wgpu::BindGroupLayoutEntry {
                        binding: 3,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("resize-pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("resize-pipeline"),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some("resize"),
            compilation_options: Default::default(),
            cache: None,
        });

        let sampler_bilinear = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("bilinear"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let sampler_nearest = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("nearest"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        log::info!("GPU resizer initialised — {} ({})", info.name, info.backend);

        Some(Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            sampler_bilinear,
            sampler_nearest,
        })
    }

    /// Resize a batch of RGBA images to `target_w × target_h`.
    /// Returns `None` per-slot for images that failed (bad dimensions, etc.).
    pub fn resize_batch(
        &self,
        inputs: &[ResizeInput],
        target_w: u32,
        target_h: u32,
        bilinear: bool,
    ) -> Vec<Option<ResizeOutput>> {
        // Process in chunks to limit peak GPU memory.
        const CHUNK: usize = 32;
        let mut results = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(CHUNK) {
            results.extend(self.resize_chunk(chunk, target_w, target_h, bilinear));
        }
        results
    }

    fn resize_chunk(
        &self,
        inputs: &[ResizeInput],
        target_w: u32,
        target_h: u32,
        bilinear: bool,
    ) -> Vec<Option<ResizeOutput>> {
        use wgpu::util::DeviceExt;

        let output_bytes = (target_w * target_h * 4) as u64;
        let sampler = if bilinear {
            &self.sampler_bilinear
        } else {
            &self.sampler_nearest
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("resize-enc"),
            });

        // Per-image readback buffers and validity flags.
        let mut readbacks: Vec<Option<wgpu::Buffer>> = Vec::with_capacity(inputs.len());

        for input in inputs {
            let expected = (input.width as usize) * (input.height as usize) * 4;
            if input.rgba_data.len() < expected || input.width == 0 || input.height == 0 {
                readbacks.push(None);
                continue;
            }

            // -- Source texture --
            let tex = self.device.create_texture_with_data(
                &self.queue,
                &wgpu::TextureDescriptor {
                    label: None,
                    size: wgpu::Extent3d {
                        width: input.width,
                        height: input.height,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    usage: wgpu::TextureUsages::TEXTURE_BINDING,
                    view_formats: &[],
                },
                wgpu::util::TextureDataOrder::LayerMajor,
                &input.rgba_data[..expected],
            );
            let tex_view = tex.create_view(&Default::default());

            // -- Output storage buffer --
            let out_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: output_bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });

            // -- Readback buffer --
            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: output_bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            // -- Uniform buffer --
            let params = ResizeParams {
                src_width: input.width,
                src_height: input.height,
                dst_width: target_w,
                dst_height: target_h,
            };
            let param_buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::bytes_of(&params),
                    usage: wgpu::BufferUsages::UNIFORM,
                });

            // -- Bind group --
            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.bind_group_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&tex_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(sampler),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: out_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: param_buf.as_entire_binding(),
                    },
                ],
            });

            // -- Dispatch compute --
            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.dispatch_workgroups((target_w + 15) / 16, (target_h + 15) / 16, 1);
            }

            // Copy GPU output → readback buffer
            encoder.copy_buffer_to_buffer(&out_buf, 0, &readback, 0, output_bytes);
            readbacks.push(Some(readback));
        }

        // Submit all work at once for the entire chunk.
        self.queue.submit(std::iter::once(encoder.finish()));

        // Map all readback buffers, then poll once.
        let mut receivers = Vec::with_capacity(readbacks.len());
        for rb in &readbacks {
            if let Some(buf) = rb {
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                buf.slice(..).map_async(wgpu::MapMode::Read, move |r| {
                    let _ = tx.send(r);
                });
                receivers.push(Some(rx));
            } else {
                receivers.push(None);
            }
        }

        // Single poll waits for all maps to complete.
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });

        // Collect results.
        let mut results = Vec::with_capacity(readbacks.len());
        for (rb, rx) in readbacks.into_iter().zip(receivers) {
            match (rb, rx) {
                (Some(buf), Some(rx)) => match rx.recv() {
                    Ok(Ok(())) => {
                        let data = buf.slice(..).get_mapped_range().to_vec();
                        buf.unmap();
                        results.push(Some(ResizeOutput {
                            rgba_data: data,
                            width: target_w,
                            height: target_h,
                        }));
                    }
                    _ => results.push(None),
                },
                _ => results.push(None),
            }
        }

        results
    }
}
