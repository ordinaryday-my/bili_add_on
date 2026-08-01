use criterion::{black_box, criterion_group, criterion_main, Criterion};
use image::{RgbImage, RgbaImage};

use bili_add_on::utils;

fn bench_blit_cached_text_fully_visible(c: &mut Criterion) {
    let mut frame = RgbImage::new(1920, 1080);
    let mut sprite = RgbaImage::new(200, 40);
    for pixel in sprite.pixels_mut() {
        *pixel = image::Rgba([255, 255, 255, 200]);
    }

    c.bench_function("blit_cached_text_1920x1080_full", |b| {
        b.iter(|| {
            utils::blit_cached_text(
                black_box(&mut frame),
                black_box(&sprite),
                black_box(100),
                black_box(500),
                black_box(0.93),
            );
        })
    });
}

fn bench_blit_cached_text_small(c: &mut Criterion) {
    let mut frame = RgbImage::new(640, 360);
    let mut sprite = RgbaImage::new(80, 20);
    for pixel in sprite.pixels_mut() {
        *pixel = image::Rgba([255, 255, 255, 200]);
    }

    c.bench_function("blit_cached_text_640x360_small", |b| {
        b.iter(|| {
            utils::blit_cached_text(
                black_box(&mut frame),
                black_box(&sprite),
                black_box(50),
                black_box(100),
                black_box(0.93),
            );
        })
    });
}

fn bench_fix_bili_xml_small(c: &mut Criterion) {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<i>
    <chatserver>chat.bilibili.com</chatserver>
    <chatid>17001</chatid>
    <d p="0.95400,1,25,16777215,1738389256,0,057b89f9,115610398131421632">hello</d>
    <d p="1.53600,4,18,65280,1738389257,0,09ace8a8,115610425619205632">world</d>
</i>"#;

    c.bench_function("fix_bili_xml_small", |b| {
        b.iter(|| {
            let _ = bili_add_on::danmaku::fix_bili_xml(black_box(xml));
        })
    });
}

fn bench_decode_rgb(c: &mut Criterion) {
    c.bench_function("decode_rgb", |b| {
        b.iter(|| {
            let _ = bili_add_on::utils::decode_rgb(black_box(16777215u32));
        })
    });
}

fn bench_decode_bytes_utf8(c: &mut Criterion) {
    let data = "这是一段测试文字，用于验证编码检测性能。".repeat(100);

    c.bench_function("decode_bytes_utf8", |b| {
        b.iter(|| {
            let _ = bili_add_on::utils::decode_bytes(
                black_box(data.as_bytes()),
                black_box("text/plain; charset=utf-8"),
            )
            .unwrap();
        })
    });
}

criterion_group!(
    benches,
    bench_blit_cached_text_fully_visible,
    bench_blit_cached_text_small,
    bench_fix_bili_xml_small,
    bench_decode_rgb,
    bench_decode_bytes_utf8,
);
criterion_main!(benches);
