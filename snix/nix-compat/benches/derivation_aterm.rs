use std::{collections::BTreeMap, path::Path};

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use mimalloc::MiMalloc;
use nix_compat::derivation::UnverifiedDerivation;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

const RESOURCES_PATHS: &str = "src/derivation/tests/derivation_tests/ok";

fn fixtures() -> BTreeMap<&'static str, Vec<u8>> {
    [
        "0hm2f1psjpcwg8fijsmr4wwxrx59s092-bar.drv",
        "292w8yzv5nn7nhdpxcs8b7vby2p27s09-nested-json.drv",
        "4wvvbi4jwn0prsdxb7vs673qa5h9gr7x-foo.drv",
        "52a9id8hx688hvlnz4d1n25ml1jdykz0-unicode.drv",
        "9lj1lkjm2ag622mh4h9rpy6j607an8g2-structured-attrs.drv",
        "ch49594n9avinrf8ip0aslidkc4lxkqv-foo.drv",
        "h32dahq0bx5rp1krcdx3a53asj21jvhk-has-multi-out.drv",
        "m1vfixn8iprlf0v9abmlrz7mjw1xj8kp-cp1252.drv",
        "ss2p4wmxijn652haqyd7dckxwl4c7hxx-bar.drv",
        "x6p0hg79i3wg0kkv7699935f7rrj9jf3-latin1.drv",
    ]
    .into_iter()
    .map(|name| {
        (
            name,
            std::fs::read(Path::new(RESOURCES_PATHS).join(name)).unwrap(),
        )
    })
    .collect()
}

fn criterion_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("derivation");

    for (drv_name, drv_bytes) in fixtures() {
        group.bench_with_input(
            BenchmarkId::new("from_aterm_bytes", drv_name),
            &drv_bytes,
            |b, i| b.iter(|| UnverifiedDerivation::from_aterm_bytes(black_box(i.as_slice()))),
        );
        group.bench_with_input(
            BenchmarkId::new("to_aterm_bytes", drv_name),
            &drv_bytes,
            |b, i| b.iter(|| UnverifiedDerivation::from_aterm_bytes(black_box(i.as_slice()))),
        );
    }
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
