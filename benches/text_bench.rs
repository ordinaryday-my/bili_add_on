use criterion::{Criterion, black_box, criterion_group, criterion_main};
use image::Rgb;

use bili_add_on::{fonts::FontStack, interaction::Args};
use clap::Parser;

fn stack() -> FontStack {
    let args =
        Args::try_parse_from(["bili_add_on", "--input", "bench.mp4", "--bvid", "BV1test"]).unwrap();
    FontStack::load(&args).unwrap()
}

fn bench_render_sprite_unique_texts(c: &mut Criterion) {
    let mut stack = stack();
    let texts: Vec<String> = (0..64)
        .map(|i| format!("第{i}条测试弹幕内容各不相同用于压测"))
        .collect();

    c.bench_function("render_sprite_unique_texts", |b| {
        b.iter(|| {
            for text in &texts {
                black_box(stack.render_sprite(text, 25.0, Rgb([255, 255, 255])));
            }
        })
    });
}

fn bench_render_sprite_repeated_texts(c: &mut Criterion) {
    let mut stack = stack();
    let texts: Vec<String> = (0..64).map(|_| "2333".to_string()).collect();

    c.bench_function("render_sprite_repeated_texts_cached", |b| {
        b.iter(|| {
            for text in &texts {
                black_box(stack.render_sprite(text, 25.0, Rgb([255, 255, 255])));
            }
        })
    });
}

fn bench_text_width(c: &mut Criterion) {
    let mut stack = stack();
    c.bench_function("text_width_mixed", |b| {
        b.iter(|| {
            black_box(stack.text_width("✟†☑ 中文符号混合弹幕测试", 25.0));
        })
    });
}

criterion_group!(
    benches,
    bench_render_sprite_unique_texts,
    bench_render_sprite_repeated_texts,
    bench_text_width,
);
criterion_main!(benches);
