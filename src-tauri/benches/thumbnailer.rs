use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use lightview_lib::pipeline::thumbnailer::{
    self, ResizeFilter, ThumbFormat, STANDARD_THUMB_SIZE,
};

/// Creates a test JPEG in memory and writes it to a temp file.
/// Returns the path to the temp file (lives until the TempDir is dropped).
fn create_test_jpeg(width: u32, height: u32) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("test.jpg");
    let img = image::RgbImage::from_fn(width, height, |x, y| {
        image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
    });
    img.save(&path).expect("save test JPEG");
    (dir, path)
}

/// Creates a test PNG in memory and writes it to a temp file.
fn create_test_png(width: u32, height: u32) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("create temp dir");
    let path = dir.path().join("test.png");
    let img = image::RgbaImage::from_fn(width, height, |x, y| {
        image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, 255])
    });
    img.save(&path).expect("save test PNG");
    (dir, path)
}

fn bench_jpeg_thumbnail(c: &mut Criterion) {
    let mut group = c.benchmark_group("jpeg_thumbnail");

    let sizes: &[(u32, u32)] = &[(1920, 1080), (4000, 3000), (6000, 4000)];

    for &(w, h) in sizes {
        let (_dir, path) = create_test_jpeg(w, h);

        for filter in [ResizeFilter::Nearest, ResizeFilter::Bilinear, ResizeFilter::Lanczos3] {
            group.bench_with_input(
                BenchmarkId::new(format!("{}x{}/{}", w, h, filter.as_str()), "jpeg"),
                &(&path, filter),
                |b, (path, filter)| {
                    b.iter(|| {
                        thumbnailer::generate_image_thumbnail(
                            black_box(path),
                            black_box(*filter),
                            ThumbFormat::Jpeg,
                            STANDARD_THUMB_SIZE,
                            STANDARD_THUMB_SIZE,
                        )
                        .expect("thumbnail generation")
                    })
                },
            );
        }
    }

    group.finish();
}

fn bench_png_thumbnail(c: &mut Criterion) {
    let mut group = c.benchmark_group("png_thumbnail");

    let sizes: &[(u32, u32)] = &[(1920, 1080), (4000, 3000)];

    for &(w, h) in sizes {
        let (_dir, path) = create_test_png(w, h);

        for filter in [ResizeFilter::Nearest, ResizeFilter::Lanczos3] {
            group.bench_with_input(
                BenchmarkId::new(format!("{}x{}/{}", w, h, filter.as_str()), "png"),
                &(&path, filter),
                |b, (path, filter)| {
                    b.iter(|| {
                        thumbnailer::generate_image_thumbnail(
                            black_box(path),
                            black_box(*filter),
                            ThumbFormat::Jpeg,
                            STANDARD_THUMB_SIZE,
                            STANDARD_THUMB_SIZE,
                        )
                        .expect("thumbnail generation")
                    })
                },
            );
        }
    }

    group.finish();
}

fn bench_decode_image(c: &mut Criterion) {
    let mut group = c.benchmark_group("decode_image");

    let (_dir_jpg, path_jpg) = create_test_jpeg(4000, 3000);
    let (_dir_png, path_png) = create_test_png(4000, 3000);

    group.bench_function("jpeg/4000x3000", |b| {
        b.iter(|| thumbnailer::decode_image(black_box(&path_jpg), 512).expect("decode"))
    });

    group.bench_function("png/4000x3000", |b| {
        b.iter(|| thumbnailer::decode_image(black_box(&path_png), 512).expect("decode"))
    });

    group.finish();
}

fn bench_pixel_convert(c: &mut Criterion) {
    let mut group = c.benchmark_group("pixel_convert");

    // ~1MP buffers — representative of a DCT-scaled decode fed into the
    // justified/HEIC/WebP conversion path.
    let px = 1024 * 1024;
    let rgb: Vec<u8> = (0..px * 3).map(|i| (i % 256) as u8).collect();
    let rgba: Vec<u8> = (0..px * 4).map(|i| (i % 256) as u8).collect();

    group.bench_function("rgb_to_rgba/1MP", |b| {
        b.iter(|| thumbnailer::rgb_to_rgba(black_box(&rgb)))
    });
    group.bench_function("rgba_to_rgb/1MP", |b| {
        b.iter(|| thumbnailer::rgba_to_rgb(black_box(&rgba)))
    });

    group.finish();
}

fn bench_resize_filters(c: &mut Criterion) {
    let mut group = c.benchmark_group("resize_filter_comparison");

    let (_dir, path) = create_test_jpeg(4000, 3000);

    for filter in [ResizeFilter::Nearest, ResizeFilter::Bilinear, ResizeFilter::Lanczos3] {
        group.bench_with_input(
            BenchmarkId::from_parameter(filter.as_str()),
            &filter,
            |b, &filter| {
                b.iter(|| {
                    thumbnailer::generate_image_thumbnail(
                        black_box(&path),
                        black_box(filter),
                        ThumbFormat::Jpeg,
                        STANDARD_THUMB_SIZE,
                        STANDARD_THUMB_SIZE,
                    )
                    .expect("thumbnail generation")
                })
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_jpeg_thumbnail,
    bench_png_thumbnail,
    bench_decode_image,
    bench_pixel_convert,
    bench_resize_filters,
);
criterion_main!(benches);
