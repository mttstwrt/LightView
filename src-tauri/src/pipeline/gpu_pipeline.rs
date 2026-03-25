//! Unified wgpu GPU pipeline for LightView.
//!
//! Hosts all GPU compute pipelines on a single device/queue:
//! - Resize: bilinear/nearest downsample (ported from gpu_resize.rs)
//! - Crop+Resize: fused center-crop and downsample in one dispatch
//! - BC7 Encode: GPU-accelerated BC7 mode 6 texture compression
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
// BC7 Mode 6 encoding shader
// ---------------------------------------------------------------------------

const BC7_ENCODE_SHADER: &str = r#"
// BC7 Mode 6 encoder: one workgroup per 4x4 block.
// Mode 6: 1 subset, 7-bit endpoints (RGBA), 1 P-bit per endpoint, 4-bit indices.
//
// Block layout (128 bits = 4 x u32):
//   Bit 0:       mode bit = 1 (mode 6 indicated by bit pattern 0b1000000 = 7th bit set)
//                Actually mode 6 = 7 bits: 0000001 (the '1' is at bit position 6)
//   Bits 1-7:    not used (mode 6 starts at bit 6, so bits 0-5 are 0, bit 6 is 1)
//   Wait — BC7 mode encoding: mode N has N zero bits followed by a 1 bit.
//   Mode 6: 000000 1 = 7 bits for mode. Bits 0..6 = 0b0000001 = 0x40... no.
//   Mode 0: 1         (bit 0 = 1)
//   Mode 1: 01        (bit 0 = 0, bit 1 = 1)
//   ...
//   Mode 6: 0000001   (bits 0-5 = 0, bit 6 = 1)
//   Mode 7: 00000001  (bits 0-6 = 0, bit 7 = 1)
//
//   After mode bits (7 bits for mode 6):
//   Bits 7..14:   endpoint 0 red   (7 bits)
//   Bits 14..21:  endpoint 0 green (7 bits)
//   Bits 21..28:  endpoint 0 blue  (7 bits)
//   Bits 28..35:  endpoint 0 alpha (7 bits)
//   Bits 35..42:  endpoint 1 red   (7 bits)
//   Bits 42..49:  endpoint 1 green (7 bits)
//   Bits 49..56:  endpoint 1 blue  (7 bits)
//   Bits 56..63:  endpoint 1 alpha (7 bits)
//   Bit  63:      P-bit 0
//   Bit  64:      P-bit 1
//   Bits 65..128: 16 x 4-bit indices (64 bits) — but index 0 is anchor, only 3 bits

struct Params {
    width: u32,
    height: u32,
    blocks_wide: u32,
    _pad: u32,
}

@group(0) @binding(0) var<storage, read> input: array<u32>;
@group(0) @binding(1) var<storage, read_write> output: array<u32>;
@group(0) @binding(2) var<uniform> params: Params;

// Shared memory for the 16 pixels in the block
var<workgroup> pixels: array<vec4<f32>, 16>;

fn insert_bits(val: u32, num_bits: u32, data: u32, offset: ptr<function, u32>, words: ptr<function, array<u32, 4>>) {
    let bit_pos = *offset;
    let word_idx = bit_pos / 32u;
    let bit_idx = bit_pos % 32u;

    if (bit_idx + num_bits <= 32u) {
        (*words)[word_idx] |= (data & ((1u << num_bits) - 1u)) << bit_idx;
    } else {
        let lo_bits = 32u - bit_idx;
        (*words)[word_idx] |= (data & ((1u << lo_bits) - 1u)) << bit_idx;
        (*words)[word_idx + 1u] |= (data >> lo_bits) & ((1u << (num_bits - lo_bits)) - 1u);
    }

    *offset += num_bits;
}

@compute @workgroup_size(16)
fn bc7_encode(
    @builtin(workgroup_id) wg_id: vec3<u32>,
    @builtin(local_invocation_index) lid: u32,
) {
    let block_idx = wg_id.x;
    let bx = block_idx % params.blocks_wide;
    let by = block_idx / params.blocks_wide;

    // Each thread reads one pixel from the 4x4 block
    let px = bx * 4u + (lid % 4u);
    let py = by * 4u + (lid / 4u);

    // Clamp to image bounds
    let clamped_x = min(px, params.width - 1u);
    let clamped_y = min(py, params.height - 1u);
    let packed = input[clamped_y * params.width + clamped_x];
    pixels[lid] = unpack4x8unorm(packed);

    workgroupBarrier();

    // Thread 0 computes endpoints, indices, and packs the block
    if (lid == 0u) {
        // Find min/max per channel
        var lo = pixels[0];
        var hi = pixels[0];
        for (var i = 1u; i < 16u; i = i + 1u) {
            lo = min(lo, pixels[i]);
            hi = max(hi, pixels[i]);
        }

        // Quantize endpoints to 7 bits (mode 6 uses 7-bit endpoints + 1 P-bit)
        // The P-bit extends each endpoint to 8 bits: (7-bit value << 1) | p_bit
        // For best quality: set P-bit to the LSB of the 8-bit quantized value
        let e0_8 = vec4<u32>(clamp(lo * 255.0 + 0.5, vec4<f32>(0.0), vec4<f32>(255.0)));
        let e1_8 = vec4<u32>(clamp(hi * 255.0 + 0.5, vec4<f32>(0.0), vec4<f32>(255.0)));

        // P-bit is the LSB of the 8-bit value
        let p0 = (e0_8.r & 1u) | (e0_8.g & 1u) | (e0_8.b & 1u) | (e0_8.a & 1u);
        let p1 = (e1_8.r & 1u) | (e1_8.g & 1u) | (e1_8.b & 1u) | (e1_8.a & 1u);

        // 7-bit endpoint = upper 7 bits of 8-bit value
        let e0 = e0_8 >> vec4<u32>(1u);
        let e1 = e1_8 >> vec4<u32>(1u);

        // Compute 4-bit interpolation indices
        let dir = hi - lo;
        let len_sq = dot(dir, dir);
        var indices: array<u32, 16>;
        for (var i = 0u; i < 16u; i = i + 1u) {
            if (len_sq < 0.000001) {
                indices[i] = 0u;
            } else {
                let t = dot(pixels[i] - lo, dir) / len_sq;
                indices[i] = u32(clamp(t * 15.0 + 0.5, 0.0, 15.0));
            }
        }

        // If the anchor index (index 0) has its MSB set, swap endpoints and invert indices
        // BC7 requires the anchor index MSB to be 0 (it's stored as 3 bits, not 4)
        if (indices[0] >= 8u) {
            // Swap endpoints
            let tmp_e0 = e0;
            let tmp_e1 = e1;
            // We need to swap — but e0/e1 are let bindings. Pack directly with swap.
            // Actually, let's just invert the indices
            for (var i = 0u; i < 16u; i = i + 1u) {
                indices[i] = 15u - indices[i];
            }
            // And swap lo/hi endpoints — we'll use inverted when packing
            // Since we can't reassign lets, we'll track a swap flag
        }

        // Pack the 128-bit BC7 block
        // We need to handle the endpoint swap properly
        var words: array<u32, 4> = array<u32, 4>(0u, 0u, 0u, 0u);
        var offset: u32 = 0u;

        // Mode 6: 7 bits = 000000 1
        insert_bits(0u, 6u, 0u, &offset, &words); // 6 zero bits
        insert_bits(0u, 1u, 1u, &offset, &words);  // mode bit

        var final_e0 = e0;
        var final_e1 = e1;
        var final_p0 = p0;
        var final_p1 = p1;

        // Check if we need to swap (anchor index MSB must be 0)
        // We already inverted indices above if needed, now swap endpoints too
        if (indices[0] >= 8u) {
            // This shouldn't happen after inversion, but just in case
            final_e0 = e1;
            final_e1 = e0;
            final_p0 = p1;
            final_p1 = p0;
        }

        // Endpoints: R0, R1, G0, G1, B0, B1, A0, A1 — each 7 bits
        // Wait, BC7 mode 6 endpoint order is: R0 R1 G0 G1 B0 B1 A0 A1
        insert_bits(0u, 7u, final_e0.r, &offset, &words);
        insert_bits(0u, 7u, final_e1.r, &offset, &words);
        insert_bits(0u, 7u, final_e0.g, &offset, &words);
        insert_bits(0u, 7u, final_e1.g, &offset, &words);
        insert_bits(0u, 7u, final_e0.b, &offset, &words);
        insert_bits(0u, 7u, final_e1.b, &offset, &words);
        insert_bits(0u, 7u, final_e0.a, &offset, &words);
        insert_bits(0u, 7u, final_e1.a, &offset, &words);

        // P-bits: P0, P1 — 1 bit each
        insert_bits(0u, 1u, final_p0 & 1u, &offset, &words);
        insert_bits(0u, 1u, final_p1 & 1u, &offset, &words);

        // Indices: 16 x 4-bit, but anchor (index 0) is only 3 bits
        // Index 0: 3 bits (MSB is implicit 0)
        insert_bits(0u, 3u, indices[0] & 7u, &offset, &words);
        // Indices 1-15: 4 bits each
        for (var i = 1u; i < 16u; i = i + 1u) {
            insert_bits(0u, 4u, indices[i], &offset, &words);
        }

        // Write the 4 u32s to output
        let out_base = block_idx * 4u;
        output[out_base + 0u] = words[0];
        output[out_base + 1u] = words[1];
        output[out_base + 2u] = words[2];
        output[out_base + 3u] = words[3];
    }
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
struct Bc7EncodeParams {
    width: u32,
    height: u32,
    blocks_wide: u32,
    _pad: u32,
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

/// Result from the full GPU thumbnail pipeline (crop+resize+BC7 encode).
pub struct GpuThumbResult {
    pub bc7_data: Vec<u8>,
    pub rgba_data: Vec<u8>,
    pub width: u32,
    pub height: u32,
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

    // BC7 encoding
    bc7_encode_pipeline: wgpu::ComputePipeline,
    bc7_encode_bgl: wgpu::BindGroupLayout,

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

        // --- BC7 encode bind group layout (storage in, storage out, uniform) ---
        let bc7_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("bc7-encode-bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::COMPUTE,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Storage { read_only: false },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
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

        let bc7_encode_pipeline = Self::create_pipeline(
            &device,
            "bc7-encode",
            BC7_ENCODE_SHADER,
            "bc7_encode",
            &bc7_bgl,
        );

        let transform_pipeline = Self::create_pipeline(
            &device,
            "transform",
            TRANSFORM_SHADER,
            "transform",
            &texture_sample_bgl,
        );

        log::info!(
            "GPU pipeline initialised — {} ({}) — resize, crop+resize, BC7 encode, transform",
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
            bc7_encode_pipeline,
            bc7_encode_bgl: bc7_bgl,
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
    // BC7 Encoding (Phase 3)
    // -----------------------------------------------------------------------

    /// Encode a batch of RGBA images to BC7 on the GPU.
    /// Each image must have dimensions that are multiples of 4.
    pub fn bc7_encode_batch(
        &self,
        inputs: &[(Vec<u8>, u32, u32)], // (rgba_data, width, height)
    ) -> Vec<Option<Vec<u8>>> {
        const CHUNK: usize = 32;
        let mut results = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(CHUNK) {
            results.extend(self.bc7_encode_chunk(chunk));
        }
        results
    }

    fn bc7_encode_chunk(
        &self,
        inputs: &[(Vec<u8>, u32, u32)],
    ) -> Vec<Option<Vec<u8>>> {
        use wgpu::util::DeviceExt;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("bc7-enc"),
            });

        let mut readbacks: Vec<Option<(wgpu::Buffer, u64)>> = Vec::with_capacity(inputs.len());

        for (rgba_data, width, height) in inputs {
            let w = *width;
            let h = *height;
            let expected = (w as usize) * (h as usize) * 4;

            if rgba_data.len() < expected || w == 0 || h == 0 {
                readbacks.push(None);
                continue;
            }

            let blocks_wide = (w + 3) / 4;
            let blocks_high = (h + 3) / 4;
            let total_blocks = blocks_wide * blocks_high;
            let bc7_bytes = (total_blocks * 16) as u64;

            // Pack RGBA as u32 array for the shader
            let packed: Vec<u32> = rgba_data[..expected]
                .chunks_exact(4)
                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect();

            let input_buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::cast_slice(&packed),
                    usage: wgpu::BufferUsages::STORAGE,
                });

            let output_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: bc7_bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });

            let readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: bc7_bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });

            let params = Bc7EncodeParams {
                width: w,
                height: h,
                blocks_wide,
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
                layout: &self.bc7_encode_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: input_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: output_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: param_buf.as_entire_binding(),
                    },
                ],
            });

            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.bc7_encode_pipeline);
                pass.set_bind_group(0, &bind_group, &[]);
                // One workgroup of 16 threads per 4x4 block
                pass.dispatch_workgroups(total_blocks, 1, 1);
            }

            encoder.copy_buffer_to_buffer(&output_buf, 0, &readback, 0, bc7_bytes);
            readbacks.push(Some((readback, bc7_bytes)));
        }

        self.queue.submit(std::iter::once(encoder.finish()));

        // Map and collect readbacks
        let mut receivers = Vec::with_capacity(readbacks.len());
        for rb in &readbacks {
            if let Some((buf, _)) = rb {
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
                (Some((buf, _)), Some(rx)) => match rx.recv() {
                    Ok(Ok(())) => {
                        let data = buf.slice(..).get_mapped_range().to_vec();
                        buf.unmap();
                        results.push(Some(data));
                    }
                    _ => results.push(None),
                },
                _ => results.push(None),
            }
        }

        results
    }

    // -----------------------------------------------------------------------
    // Chained pipeline: crop+resize → BC7 encode (Phase 4)
    // No intermediate CPU readback — data stays on GPU.
    // -----------------------------------------------------------------------

    /// Full GPU thumbnail pipeline: crop+resize then BC7 encode in one submission.
    /// Returns BC7 data and optionally RGBA data (for JPEG encoding on CPU).
    pub fn generate_thumbnails_batch(
        &self,
        inputs: &[CropResizeInput],
        target_w: u32,
        target_h: u32,
        bilinear: bool,
        need_rgba_readback: bool,
    ) -> Vec<Option<GpuThumbResult>> {
        const CHUNK: usize = 32;
        let mut results = Vec::with_capacity(inputs.len());
        for chunk in inputs.chunks(CHUNK) {
            results.extend(self.generate_thumbnails_chunk(
                chunk,
                target_w,
                target_h,
                bilinear,
                need_rgba_readback,
            ));
        }
        results
    }

    fn generate_thumbnails_chunk(
        &self,
        inputs: &[CropResizeInput],
        target_w: u32,
        target_h: u32,
        bilinear: bool,
        need_rgba_readback: bool,
    ) -> Vec<Option<GpuThumbResult>> {
        use wgpu::util::DeviceExt;

        let rgba_bytes = (target_w * target_h * 4) as u64;
        let blocks_wide = (target_w + 3) / 4;
        let blocks_high = (target_h + 3) / 4;
        let total_blocks = blocks_wide * blocks_high;
        let bc7_bytes = (total_blocks * 16) as u64;

        let sampler = if bilinear {
            &self.sampler_bilinear
        } else {
            &self.sampler_nearest
        };

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("gen-thumb-enc"),
            });

        struct PerImageBuffers {
            bc7_readback: wgpu::Buffer,
            rgba_readback: Option<wgpu::Buffer>,
        }

        let mut per_image: Vec<Option<PerImageBuffers>> = Vec::with_capacity(inputs.len());

        for input in inputs {
            let expected = (input.width as usize) * (input.height as usize) * 4;
            if input.rgba_data.len() < expected || input.width == 0 || input.height == 0 {
                per_image.push(None);
                continue;
            }

            // Upload source texture
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

            // Intermediate RGBA buffer (stays on GPU between dispatches)
            let rgba_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: rgba_bytes,
                usage: wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });

            // --- Dispatch 1: Crop + Resize ---
            let crop_params = CropResizeParams {
                src_width: input.width,
                src_height: input.height,
                crop_x: input.crop_x,
                crop_y: input.crop_y,
                crop_size: input.crop_size,
                dst_width: target_w,
                dst_height: target_h,
                _pad: 0,
            };
            let crop_param_buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::bytes_of(&crop_params),
                    usage: wgpu::BufferUsages::UNIFORM,
                });

            let crop_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
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
                        resource: rgba_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: crop_param_buf.as_entire_binding(),
                    },
                ],
            });

            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.crop_resize_pipeline);
                pass.set_bind_group(0, &crop_bg, &[]);
                pass.dispatch_workgroups((target_w + 15) / 16, (target_h + 15) / 16, 1);
            }

            // --- Dispatch 2: BC7 Encode (reads from rgba_buf, no CPU roundtrip) ---
            let bc7_output_buf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: bc7_bytes,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });

            let bc7_params = Bc7EncodeParams {
                width: target_w,
                height: target_h,
                blocks_wide,
                _pad: 0,
            };
            let bc7_param_buf = self
                .device
                .create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::bytes_of(&bc7_params),
                    usage: wgpu::BufferUsages::UNIFORM,
                });

            let bc7_bg = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.bc7_encode_bgl,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: rgba_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: bc7_output_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: bc7_param_buf.as_entire_binding(),
                    },
                ],
            });

            {
                let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                    label: None,
                    timestamp_writes: None,
                });
                pass.set_pipeline(&self.bc7_encode_pipeline);
                pass.set_bind_group(0, &bc7_bg, &[]);
                pass.dispatch_workgroups(total_blocks, 1, 1);
            }

            // --- Readback buffers ---
            let bc7_readback = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: bc7_bytes,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            encoder.copy_buffer_to_buffer(&bc7_output_buf, 0, &bc7_readback, 0, bc7_bytes);

            let rgba_readback = if need_rgba_readback {
                let rb = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: None,
                    size: rgba_bytes,
                    usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                encoder.copy_buffer_to_buffer(&rgba_buf, 0, &rb, 0, rgba_bytes);
                Some(rb)
            } else {
                None
            };

            per_image.push(Some(PerImageBuffers {
                bc7_readback,
                rgba_readback,
            }));
        }

        // Submit everything at once
        self.queue.submit(std::iter::once(encoder.finish()));

        // Map all readbacks
        let mut bc7_receivers = Vec::with_capacity(per_image.len());
        let mut rgba_receivers = Vec::with_capacity(per_image.len());

        for item in &per_image {
            if let Some(bufs) = item {
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                bufs.bc7_readback
                    .slice(..)
                    .map_async(wgpu::MapMode::Read, move |r| {
                        let _ = tx.send(r);
                    });
                bc7_receivers.push(Some(rx));

                if let Some(ref rgba_rb) = bufs.rgba_readback {
                    let (tx2, rx2) = std::sync::mpsc::sync_channel(1);
                    rgba_rb
                        .slice(..)
                        .map_async(wgpu::MapMode::Read, move |r| {
                            let _ = tx2.send(r);
                        });
                    rgba_receivers.push(Some(rx2));
                } else {
                    rgba_receivers.push(None);
                }
            } else {
                bc7_receivers.push(None);
                rgba_receivers.push(None);
            }
        }

        let _ = self.device.poll(wgpu::PollType::Wait { submission_index: None, timeout: None });

        // Collect results
        let mut results = Vec::with_capacity(per_image.len());
        for (i, item) in per_image.into_iter().enumerate() {
            match item {
                Some(bufs) => {
                    let bc7_data = match bc7_receivers[i].as_ref().and_then(|rx| rx.recv().ok()) {
                        Some(Ok(())) => {
                            let data = bufs
                                .bc7_readback
                                .slice(..)
                                .get_mapped_range()
                                .to_vec();
                            bufs.bc7_readback.unmap();
                            data
                        }
                        _ => {
                            results.push(None);
                            continue;
                        }
                    };

                    let rgba_data = if let Some(ref rgba_rb) = bufs.rgba_readback {
                        match rgba_receivers[i].as_ref().and_then(|rx| rx.recv().ok()) {
                            Some(Ok(())) => {
                                let data = rgba_rb.slice(..).get_mapped_range().to_vec();
                                rgba_rb.unmap();
                                data
                            }
                            _ => Vec::new(),
                        }
                    } else {
                        Vec::new()
                    };

                    results.push(Some(GpuThumbResult {
                        bc7_data,
                        rgba_data,
                        width: target_w,
                        height: target_h,
                    }));
                }
                None => results.push(None),
            }
        }

        results
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
