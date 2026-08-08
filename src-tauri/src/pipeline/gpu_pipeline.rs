//! GPU-accelerated thumbnail generation on wgpu.
//!
//! One device and queue hosting the fused crop+resize: a centre-crop and a
//! downsample in a single dispatch, which is what makes this worth the
//! round-trip to the GPU at all — either step alone would not be.
//!
//! Entirely optional. `GpuPipeline::new()` returns `None` when no Vulkan/GL
//! adapter is available and every caller falls back to the CPU path in
//! `pipeline::thumbnailer`, so this module is a fast path, never a dependency.

use bytemuck::{Pod, Zeroable};

// ---------------------------------------------------------------------------
// Crop + Resize shader (fused — eliminates CPU center-crop)
// ---------------------------------------------------------------------------

const CROP_RESIZE_SHADER: &str = r#"
struct Params {
    src_width: u32,
    src_height: u32,
    crop_x: u32,
    crop_y: u32,
    crop_size: u32,
    dst_width: u32,
    dst_height: u32,
    _pad: u32,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<storage, read_write> output: array<u32>;
@group(0) @binding(3) var<uniform> params: Params;

@compute @workgroup_size(16, 16)
fn crop_resize(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.dst_width || gid.y >= params.dst_height) {
        return;
    }

    // Map output pixel to crop region in source texture
    let crop_u = (f32(gid.x) + 0.5) / f32(params.dst_width);
    let crop_v = (f32(gid.y) + 0.5) / f32(params.dst_height);

    let src_x = f32(params.crop_x) + crop_u * f32(params.crop_size);
    let src_y = f32(params.crop_y) + crop_v * f32(params.crop_size);

    let uv = vec2f(src_x / f32(params.src_width), src_y / f32(params.src_height));
    let color = textureSampleLevel(src, samp, uv, 0.0);

    output[gid.y * params.dst_width + gid.x] = pack4x8unorm(color);
}
"#;

// ---------------------------------------------------------------------------
// Rust types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct CropResizeParams {
    pub src_width: u32,
    pub src_height: u32,
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_size: u32,
    pub dst_width: u32,
    pub dst_height: u32,
    pub _pad: u32,
}

/// Output of a single GPU resize operation.
pub struct ResizeOutput {
    pub rgba_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Input for fused crop+resize: full decoded image + crop rect.
pub struct CropResizeInput {
    pub rgba_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub crop_x: u32,
    pub crop_y: u32,
    pub crop_size: u32,
}

/// Unified wgpu GPU pipeline for all image processing.
pub struct GpuPipeline {
    device: wgpu::Device,
    queue: wgpu::Queue,

    // Crop + Resize (fused)
    crop_resize_pipeline: wgpu::ComputePipeline,
    crop_resize_bgl: wgpu::BindGroupLayout,

    // Shared samplers
    sampler_bilinear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
}

// GpuPipeline is Send+Sync because wgpu Device/Queue are.
unsafe impl Send for GpuPipeline {}
unsafe impl Sync for GpuPipeline {}

impl GpuPipeline {
    /// Try to initialise a headless wgpu device with all pipelines.
    /// Returns `None` if no GPU is available.
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
            "GPU pipeline adapter: {} ({:?})",
            info.name,
            info.backend
        );

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("lightview-gpu-pipeline"),
                ..Default::default()
            })
            .await
            .ok()?;

        // --- Shared samplers ---
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

        // --- Texture + sampler + storage + uniform bind group layout (shared by resize and crop_resize) ---
        let texture_sample_bgl =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("texture-sample-bgl"),
                entries: &[
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
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                        count: None,
                    },
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

        // --- Create pipelines ---
        let crop_resize_pipeline = Self::create_pipeline(
            &device,
            "crop-resize",
            CROP_RESIZE_SHADER,
            "crop_resize",
            &texture_sample_bgl,
        );

        log::info!(
            "GPU pipeline initialised — {} ({}) — crop+resize",
            info.name,
            info.backend
        );

        Some(Self {
            device,
            queue,
            crop_resize_pipeline,
            crop_resize_bgl: texture_sample_bgl,
            sampler_bilinear,
            sampler_nearest,
        })
    }

    fn create_pipeline(
        device: &wgpu::Device,
        label: &str,
        shader_source: &str,
        entry_point: &str,
        bgl: &wgpu::BindGroupLayout,
    ) -> wgpu::ComputePipeline {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some(label),
            source: wgpu::ShaderSource::Wgsl(shader_source.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some(&format!("{label}-pl")),
            bind_group_layouts: &[Some(bgl)],
            immediate_size: 0,
        });

        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(&format!("{label}-pipeline")),
            layout: Some(&pipeline_layout),
            module: &shader,
            entry_point: Some(entry_point),
            compilation_options: Default::default(),
            cache: None,
        })
    }

    // -----------------------------------------------------------------------
    // Crop + Resize (fused — Phase 2)
    // -----------------------------------------------------------------------

    /// Fused crop + resize: takes full decoded images with crop rects, produces
    /// resized RGBA output in a single GPU dispatch per image (no CPU crop step).
    pub fn crop_resize_batch(
        &self,
        inputs: &[CropResizeInput],
        target_w: u32,
        target_h: u32,
        bilinear: bool,
    ) -> Vec<Option<ResizeOutput>> {
        const CHUNK: usize = 32;
        let mut results = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(CHUNK) {
            results.extend(self.crop_resize_chunk(chunk, target_w, target_h, bilinear));
        }
        results
    }

    fn crop_resize_chunk(
        &self,
        inputs: &[CropResizeInput],
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
                label: Some("crop-resize-enc"),
            });

        let mut readbacks: Vec<Option<wgpu::Buffer>> = Vec::with_capacity(inputs.len());

        for input in inputs {
            let expected = (input.width as usize) * (input.height as usize) * 4;
            if input.rgba_data.len() < expected || input.width == 0 || input.height == 0 {
                readbacks.push(None);
                continue;
            }

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

            let out_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: output_bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });

            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: output_bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            let params = CropResizeParams {
                src_width: input.width,
                src_height: input.height,
                crop_x: input.crop_x,
                crop_y: input.crop_y,
                crop_size: input.crop_size,
                dst_width: target_w,
                dst_height: target_h,
                _pad: 0,
            };
            let param_buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::bytes_of(&params),
                    usage: wgpu::BufferUsages::UNIFORM,
                });

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.crop_resize_bgl,
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

            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.crop_resize_pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.dispatch_workgroups((target_w + 15) / 16, (target_h + 15) / 16, 1);
            }

            encoder.copy_buffer_to_buffer(&out_buf, 0, &readback, 0, output_bytes);
            readbacks.push(Some(readback));
        }

        self.queue.submit(std::iter::once(encoder.finish()));
        self.collect_readbacks(readbacks, target_w, target_h)
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    fn collect_readbacks(
        &self,
        readbacks: Vec<Option<wgpu::Buffer>>,
        width: u32,
        height: u32,
    ) -> Vec<Option<ResizeOutput>> {
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

        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });

        let mut results = Vec::with_capacity(readbacks.len());
        for (rb, rx) in readbacks.into_iter().zip(receivers) {
            match (rb, rx) {
                (Some(buf), Some(rx)) => match rx.recv() {
                    Ok(Ok(())) => {
                        let data = buf.slice(..).get_mapped_range().to_vec();
                        buf.unmap();
                        results.push(Some(ResizeOutput {
                            rgba_data: data,
                            width,
                            height,
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
