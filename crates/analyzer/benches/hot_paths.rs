//! Scaling benchmarks for analyzer hot paths that are quadratic in the number
//! of statements/locals within a single scope.
//!
//! These exist to give a reproducible before/after comparison for targeted
//! performance work. Each scenario is generated at several sizes so the scaling
//! curve (ideally linear) is visible in the criterion output; if a change turns
//! an `O(n)` path back into `O(n^2)`, the larger sizes blow up first.
//!
//! Scenarios (see the per-generator docs):
//! - `distinct_locals_scalar` / `distinct_locals_array` stress the per-assignment
//!   `block_context.locals` bookkeeping (one entry accumulated per distinct
//!   local, rescanned on every subsequent assignment).
//! - `compound_assign` stresses the compound-assignment (`+=`) path, which
//!   snapshots the growing expression-type map on each operation.
//! - `array_literal` is a linear control: a single giant literal array. It
//!   should stay flat per element regardless of the fixes above, and guards
//!   against accidentally making array construction quadratic.
//!
//! Run: `cargo bench -p mago-analyzer --bench hot_paths`
//! Compare: `cargo bench -p mago-analyzer --bench hot_paths -- --save-baseline before`
//! then, after a change, `... -- --baseline before`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::borrow::Cow;
use std::fmt::Write;
use std::sync::LazyLock;

use criterion::BenchmarkId;
use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use mago_allocator::LocalArena;

use foldhash::HashSet;

use mago_analyzer::Analyzer;
use mago_analyzer::analysis_result::AnalysisResult;
use mago_analyzer::plugin::PluginRegistry;
use mago_analyzer::settings::Settings;
use mago_codex::populator::populate_codebase;
use mago_codex::scanner::scan_program;
use mago_database::DatabaseReader;
use mago_database::file::File;
use mago_names::resolver::NameResolver;
use mago_prelude::Prelude;
use mago_syntax::parser::parse_file;
use mago_word::WordSet;

static PRELUDE: LazyLock<Prelude> = LazyLock::new(Prelude::build);
static PLUGIN_REGISTRY: LazyLock<PluginRegistry> = LazyLock::new(PluginRegistry::with_library_providers);

/// Sizes (number of generated statements/elements) probed per scenario.
///
/// Chosen so a quadratic path is unmistakable — each doubling should ~4x the
/// time of the quadratic scenarios while the linear control (`array_literal`)
/// barely moves. The smallest size is kept above the fixed per-iteration floor
/// (prelude populate + analyze of the enclosing file) so that floor doesn't
/// dilute the signal; the largest makes any surviving `O(n^2)` term dominate.
const SIZES: &[usize] = &[1000, 2000, 4000];

/// Runs the full single-file analyzer pipeline, mirroring `benches/cases.rs`
/// so the numbers are comparable to the corpus benchmarks.
fn analyze_source(source: &str) {
    let Prelude { mut database, mut metadata, mut symbol_references } = PRELUDE.clone();

    let file = File::ephemeral(Cow::Borrowed(b"hot_paths.php".as_slice()), Cow::Owned(source.as_bytes().to_vec()));
    let file_id = database.add(file);
    let source_file = database.get_ref(&file_id).expect("file just added must exist");

    let arena = LocalArena::new();
    let program = parse_file(&arena, source_file);
    assert!(!program.has_errors(), "generated benchmark source failed to parse: {:?}", program.errors);

    let resolver = NameResolver::new(&arena);
    let resolved_names = resolver.resolve(program);

    let settings = Settings {
        find_unused_expressions: true,
        find_unused_definitions: true,
        check_throws: true,
        allow_possibly_undefined_array_keys: false,
        strict_list_index_checks: true,
        check_property_initialization: true,
        ..Default::default()
    };

    metadata.extend(scan_program(&arena, source_file, program, &resolved_names, settings.version));
    populate_codebase(&mut metadata, &mut symbol_references, WordSet::default(), HashSet::default());

    let mut analysis_result = AnalysisResult::new(symbol_references);
    let analyzer = Analyzer::new(&arena, source_file, &resolved_names, &metadata, &PLUGIN_REGISTRY, settings);
    analyzer.analyze(program, &mut analysis_result).expect("analysis of generated benchmark source failed");
}

/// Wraps a generated statement body in a single function scope, so every
/// generated local lives in the same block context (the shape a large Laravel
/// seeder or data-fixture method takes).
fn in_scope(body: &str) -> String {
    format!("<?php\n\nfunction bench_scope(): void\n{{\n{body}}}\n")
}

/// `n` distinct locals assigned a scalar: `$v0 = 0; $v1 = 1; ...`.
fn distinct_locals_scalar(n: usize) -> String {
    let mut body = String::new();
    for i in 0..n {
        let _ = writeln!(body, "    $v{i} = {i};");
    }
    in_scope(&body)
}

/// `n` distinct locals assigned a small array (seeder row shape):
/// `$row0 = ['id' => 0, 'name' => 'n0', 'active' => true]; ...`.
fn distinct_locals_array(n: usize) -> String {
    let mut body = String::new();
    for i in 0..n {
        let _ = writeln!(body, "    $row{i} = ['id' => {i}, 'name' => 'n{i}', 'active' => true];");
    }
    in_scope(&body)
}

/// One local mutated `n` times with a compound operator: `$sum = 0; $sum += 1; ...`.
fn compound_assign(n: usize) -> String {
    let mut body = String::from("    $sum = 0;\n");
    for i in 0..n {
        let _ = writeln!(body, "    $sum += {i};");
    }
    in_scope(&body)
}

/// A single giant literal array of `n` rows (linear control):
/// `$data = [ ['id' => 0, 'name' => 'n0'], ... ];`.
fn array_literal(n: usize) -> String {
    let mut body = String::from("    $data = [\n");
    for i in 0..n {
        let _ = writeln!(body, "        ['id' => {i}, 'name' => 'n{i}'],");
    }
    body.push_str("    ];\n");
    in_scope(&body)
}

fn bench_hot_paths(c: &mut Criterion) {
    LazyLock::force(&PRELUDE);
    LazyLock::force(&PLUGIN_REGISTRY);

    let scenarios: &[(&str, fn(usize) -> String)] = &[
        ("distinct_locals_scalar", distinct_locals_scalar),
        ("distinct_locals_array", distinct_locals_array),
        ("compound_assign", compound_assign),
        ("array_literal", array_literal),
    ];

    let mut group = c.benchmark_group("analyzer_hot_paths");
    // These inputs are individually expensive; keep sample counts modest so the
    // full sweep stays in the tens of seconds while remaining comparable.
    group.sample_size(10);

    for (name, generate) in scenarios {
        for &n in SIZES {
            let source = generate(n);
            // Sanity-check the generated program up front (parse + analyze once),
            // outside the measured loop, so a generator bug fails fast.
            group.bench_with_input(BenchmarkId::new(*name, n), &source, |b, source| {
                b.iter(|| analyze_source(source));
            });
        }
    }

    group.finish();
}

criterion_group!(benches, bench_hot_paths);
criterion_main!(benches);
