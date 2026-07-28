//! Benchmark for `build_file_signature`, the per-file signature/fingerprint
//! pass that runs on every scanned source file (host *and* vendor) during the
//! compile phase.
//!
//! This exists to quantify the per-file cost that a full (non-incremental)
//! analysis pays for signatures it never uses: the compile phase scans ~54k
//! files on a real project, so even a small per-file cost here is multiplied
//! heavily. Use it for a before/after when gating this pass behind the
//! incremental path.
//!
//! Run: `cargo bench -p mago-codex --bench signature_builder`

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::borrow::Cow;
use std::fmt::Write;
use std::hint::black_box;

use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;

use mago_allocator::LocalArena;
use mago_codex::signature_builder::build_file_signature;
use mago_database::file::File;
use mago_names::resolver::NameResolver;
use mago_syntax::parser::parse_file;

/// Number of methods on the generated class per file. A couple of sizes so the
/// per-file curve is visible; a typical vendor file sits at the low end, but
/// larger files exist and multiply the compile cost.
const METHOD_COUNTS: &[usize] = &[10, 50, 200];

/// Generates a single class with `n` small methods — a stand-in for a typical
/// scanned library/vendor file that the signature pass walks and fingerprints.
fn class_with_methods(n: usize) -> String {
    let mut src = String::from("<?php\n\nnamespace Bench\\Generated;\n\nclass Widget\n{\n");
    for i in 0..n {
        let _ = write!(
            src,
            "    public function method{i}(int $a{i}, string $b{i}): int\n    {{\n        $x = $a{i} + {i};\n        return $x;\n    }}\n\n",
        );
    }
    src.push_str("}\n");
    src
}

fn bench_build_file_signature(c: &mut Criterion) {
    let mut group = c.benchmark_group("build_file_signature");

    for &n in METHOD_COUNTS {
        let source = class_with_methods(n);

        // Parse + resolve once, outside the measured loop: the signature pass is
        // what we're measuring, not parsing/resolution. The arena, file and
        // resolved names must outlive the iterations, so build them here.
        let arena = LocalArena::new();
        let file = File::ephemeral(Cow::Borrowed(b"widget.php".as_slice()), Cow::Owned(source.into_bytes()));

        let program = parse_file(&arena, &file);
        assert!(!program.has_errors(), "generated benchmark source failed to parse: {:?}", program.errors);
        let resolver = NameResolver::new(&arena);
        let resolved_names = resolver.resolve(program);

        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(build_file_signature(black_box(&file), black_box(program), black_box(&resolved_names))));
        });
    }

    group.finish();
}

criterion_group!(benches, bench_build_file_signature);
criterion_main!(benches);
