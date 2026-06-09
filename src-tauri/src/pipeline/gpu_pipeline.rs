//! Unified wgpu GPU pipeline for LightView.
//!
//! Hosts all GPU compute pipelines on a single device/queue:
//! - Resize: bilinear/nearest downsample (ported from gpu_resize.rs)
//! - Crop+Resize: fused center-crop and downsample in one dispatch
//! - Image Transform: rotation, exposure, color adjustments for the viewer
//!
//! Falls back gracefully: if no Vulkan/GL adapter is found, `GpuPipeline::new()`
//! returns `None` and the app uses the existing CPU paths.

use bytemuck::{Pod, Zeroable};

// ---------------------------------------------------------------------------
// Resize shader (ported from gpu_resize.rs)
// ---------------------------------------------------------------------------

const RESIZE_SHADER: &str = r#"
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
// Image transform shader (for full-res viewer)
// ---------------------------------------------------------------------------

const TRANSFORM_SHADER: &str = r#"
struct Params {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    // Transform parameters
    rotation_cos: f32,
    rotation_sin: f32,
    exposure: f32,      // EV stops (multiply by pow(2, exposure))
    saturation: f32,    // 1.0 = normal
    contrast: f32,      // 1.0 = normal
    _pad0: f32,
    _pad1: f32,
    _pad2: f32,
}

@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
@group(0) @binding(2) var<storage, read_write> output: array<u32>;
@group(0) @binding(3) var<uniform> params: Params;

@compute @workgroup_size(16, 16)
fn transform(@builtin(global_invocation_id) gid: vec3<u32>) {
    if (gid.x >= params.dst_width || gid.y >= params.dst_height) {
        return;
    }

    // Normalized coordinates centered at (0.5, 0.5)
    let uv = vec2f(
        (f32(gid.x) + 0.5) / f32(params.dst_width) - 0.5,
        (f32(gid.y) + 0.5) / f32(params.dst_height) - 0.5,
    );

    // Apply rotation
    let rotated = vec2f(
        uv.x * params.rotation_cos - uv.y * params.rotation_sin + 0.5,
        uv.x * params.rotation_sin + uv.y * params.rotation_cos + 0.5,
    );

    // Sample (out-of-bounds returns transparent black)
    var color: vec4<f32>;
    if (rotated.x < 0.0 || rotated.x > 1.0 || rotated.y < 0.0 || rotated.y > 1.0) {
        color = vec4<f32>(0.0, 0.0, 0.0, 1.0);
    } else {
        color = textureSampleLevel(src, samp, rotated, 0.0);
    }

    // Exposure adjustment
    let exposure_mult = pow(2.0, params.exposure);
    color = vec4<f32>(color.rgb * exposure_mult, color.a);

    // Saturation adjustment
    let luminance = dot(color.rgb, vec3<f32>(0.2126, 0.7152, 0.0722));
    color = vec4<f32>(mix(vec3<f32>(luminance), color.rgb, params.saturation), color.a);

    // Contrast adjustment
    color = vec4<f32>((color.rgb - 0.5) * params.contrast + 0.5, color.a);

    // Clamp to [0, 1]
    color = clamp(color, vec4<f32>(0.0), vec4<f32>(1.0));

    let idx = gid.y * params.dst_width + gid.x;
    output[idx] = pack4x8unorm(color);
}
"#;

// ---------------------------------------------------------------------------
// Rust types
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
struct ResizeParams {
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
}

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

#[repr(C)]
#[derive(Copy, Clone, Pod, Zeroable)]
pub struct TransformParams {
    pub src_width: u32,
    pub src_height: u32,
    pub dst_width: u32,
    pub dst_height: u32,
    pub rotation_cos: f32,
    pub rotation_sin: f32,
    pub exposure: f32,
    pub saturation: f32,
    pub contrast: f32,
    pub _pad0: f32,
    pub _pad1: f32,
    pub _pad2: f32,
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

    // Resize (standalone)
    resize_pipeline: wgpu::ComputePipeline,
    resize_bgl: wgpu::BindGroupLayout,

    // Crop + Resize (fused)
    crop_resize_pipeline: wgpu::ComputePipeline,
    crop_resize_bgl: wgpu::BindGroupLayout,

    // Image transforms (viewer)
    transform_pipeline: wgpu::ComputePipeline,
    transform_bgl: wgpu::BindGroupLayout,

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
        let resize_pipeline = Self::create_pipeline(
            &device,
            "resize",
            RESIZE_SHADER,
            "resize",
            &texture_sample_bgl,
        );

        let crop_resize_pipeline = Self::create_pipeline(
            &device,
            "crop-resize",
            CROP_RESIZE_SHADER,
            "crop_resize",
            &texture_sample_bgl,
        );

        let transform_pipeline = Self::create_pipeline(
            &device,
            "transform",
            TRANSFORM_SHADER,
            "transform",
            &texture_sample_bgl,
        );

        log::info!(
            "GPU pipeline initialised — {} ({}) — resize, crop+resize, transform",
            info.name,
            info.backend
        );

        Some(Self {
            device,
            queue,
            resize_pipeline,
            resize_bgl: texture_sample_bgl.clone(),
            crop_resize_pipeline,
            crop_resize_bgl: texture_sample_bgl.clone(),
            transform_pipeline,
            transform_bgl: texture_sample_bgl,
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
    // Resize (ported from GpuResizer)
    // -----------------------------------------------------------------------

    /// Resize a batch of RGBA images to `target_w × target_h`.
    pub fn resize_batch(
        &self,
        inputs: &[ResizeInput],
        target_w: u32,
        target_h: u32,
        bilinear: bool,
    ) -> Vec<Option<ResizeOutput>> {
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

            let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.resize_bgl,
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
                pass.set_pipeline(&self.resize_pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                pass.dispatch_workgroups((target_w + 15) / 16, (target_h + 15) / 16, 1);
            }

            encoder.copy_buffer_to_buffer(&out_buf, 0, &readback, 0, output_bytes);
            readbacks.push(Some(readback));
        }

        self.queue.submit(std::iter::once(encoder.finish()));

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
    // Image Transforms (Phase 5 — for the full-res viewer)
    // -----------------------------------------------------------------------

    /// Apply transforms (rotation, exposure, saturation, contrast) to an image on the GPU.
    /// Returns RGBA pixel data at the specified output dimensions.
    pub fn transform_image(
        &self,
        rgba_data: &[u8],
        src_width: u32,
        src_height: u32,
        dst_width: u32,
        dst_height: u32,
        params: TransformParams,
    ) -> Option<Vec<u8>> {
        use wgpu::util::DeviceExt;

        let expected = (src_width as usize) * (src_height as usize) * 4;
        if rgba_data.len() < expected || src_width == 0 || src_height == 0 {
            return None;
        }

        let output_bytes = (dst_width as u64) * (dst_height as u64) * 4;

        let tex = self.device.create_texture_with_data(
            &self.queue,
            &wgpu::TextureDescriptor {
                label: None,
                size: wgpu::Extent3d {
                    width: src_width,
                    height: src_height,
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
            &rgba_data[..expected],
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

        let param_buf = self
            .device
            .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None,
                contents: bytemuck::bytes_of(&params),
                usage: wgpu::BufferUsages::UNIFORM,
            });

        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.transform_bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&tex_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&self.sampler_bilinear),
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

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("transform-enc"),
            });

        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: None,
                timestamp_writes: None,
            });
            pass.set_pipeline(&self.transform_pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.dispatch_workgroups((dst_width + 15) / 16, (dst_height + 15) / 16, 1);
        }

        encoder.copy_buffer_to_buffer(&out_buf, 0, &readback, 0, output_bytes);
        self.queue.submit(std::iter::once(encoder.finish()));

        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        readback
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |r| {
                let _ = tx.send(r);
            });
        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });

        match rx.recv() {
            Ok(Ok(())) => {
                let data = readback.slice(..).get_mapped_range().to_vec();
                readback.unmap();
                Some(data)
            }
            _ => None,
        }
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
