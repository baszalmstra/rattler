use criterion::{black_box, criterion_group, criterion_main, Criterion};
use rattler_conda_types::{MatchSpec, PackageRecord, Version};
use std::str::FromStr;

fn criterion_benchmark(c: &mut Criterion) {
    c.bench_function("parse simple version", |b| {
        b.iter(|| black_box("3.11.4").parse::<Version>())
    });
    c.bench_function("parse complex version", |b| {
        b.iter(|| black_box("1!1.0b2.post345.dev456+3.2.20.rc3").parse::<Version>())
    });

    c.bench_function("parse matchspec", |b| {
        b.iter(|| {
            let _ = black_box("blas *.* mkl").parse::<MatchSpec>();
            let _ = black_box("foo=1.0=py27_0").parse::<MatchSpec>();
            let _ = black_box("foo==1.0=py27_0").parse::<MatchSpec>();
            let _ = black_box("python 3.8.* *_cpython").parse::<MatchSpec>();
            let _ = black_box("pytorch=*=cuda*").parse::<MatchSpec>();
            let _ = black_box("x264 >=1!164.3095,<1!165").parse::<MatchSpec>();
            let _ = black_box("conda-forge::foo[md5=8b1a9953c4611296a827abf8c47804d7]")
                .parse::<MatchSpec>();
        })
    });

    let package_record = PackageRecord::new(
        String::from("foo"),
        Version::from_str("1.0").unwrap(),
        String::from("bar"),
    );

    c.bench_function("match matchspec", |b| {
        b.iter(|| {
            black_box("blas *.* mkl")
                .parse::<MatchSpec>()
                .unwrap()
                .matches(&package_record)
        })
    });
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
