//! The staging globs agree with the path closures of the bench's
//! `stage_plan` (bench/showdown/src/arms.rs) on the bench app's paths.

use slimtoken::profile::{Layer, Staging, NEXTJS, NO_STAGING};

/// The bench closures as globs.
const BENCH: Staging = Staging {
    layers: &[
        ("**/types.ts", Layer::Types),
        ("**/*.d.ts", Layer::Types),
        ("*/**/types/**/*.{ts,tsx}", Layer::Types),
        ("**/seed.{ts,tsx}", Layer::Data),
        ("**/seed.*.{ts,tsx}", Layer::Data),
        ("{lib,src/lib}/**/*.ts", Layer::Core),
        ("**/*.{ts,tsx}", Layer::Consumer),
    ],
    test_globs: &["**/*.test.*", "**/*.spec.*"],
    ..NO_STAGING
};

// Copied from `stage_plan`.
fn is_code(path: &str) -> bool {
    path.ends_with(".ts") || path.ends_with(".tsx")
}
fn is_test(path: &str) -> bool {
    path.contains(".test.") || path.contains(".spec.")
}
fn is_types(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    name == "types.ts" || name.ends_with(".d.ts") || path.contains("/types/")
}
fn is_seed(path: &str) -> bool {
    path.rsplit('/')
        .next()
        .is_some_and(|name| name.starts_with("seed."))
}
fn is_lib(path: &str) -> bool {
    (path.starts_with("lib/") || path.starts_with("src/lib/"))
        && !is_seed(path)
        && path.ends_with(".ts")
        && !is_test(path)
        && !is_types(path)
}

/// The stage `stage_plan` gives a path (the seed with its `seed` switch on).
fn bench_layer(path: &str) -> Option<Layer> {
    if !is_code(path) {
        None
    } else if is_types(path) {
        Some(Layer::Types)
    } else if is_lib(path) {
        Some(Layer::Core)
    } else if is_seed(path) {
        Some(Layer::Data)
    } else {
        Some(Layer::Consumer)
    }
}

fn assert_agrees(path: &str) {
    let staging = &NEXTJS.staging;
    assert_eq!(staging.layer_of(path), bench_layer(path), "layer of {path}");
    assert_eq!(
        staging.is_test(path),
        is_test(path),
        "test status of {path}"
    );
    // The bench adds TEST_RULE to tests and NEXT_RULE to other `.tsx` files.
    let (test_rule, next_rule) = (staging.path_rules[0].1, staging.path_rules[1].1);
    let bench_rule = if !is_code(path) {
        None
    } else if is_test(path) {
        Some(test_rule)
    } else if path.ends_with(".tsx") {
        Some(next_rule)
    } else {
        None
    };
    assert_eq!(staging.path_rule(path), bench_rule, "path rule of {path}");
}

#[test]
fn nextjs_staging_uses_the_bench_globs() {
    assert_eq!(NEXTJS.staging.layers, BENCH.layers);
    assert_eq!(NEXTJS.staging.test_globs, BENCH.test_globs);
}

#[test]
fn globs_agree_with_bench_closures_on_bench_paths() {
    let paths = include_str!("fixtures/showdown-paths.txt");
    let paths: Vec<&str> = paths.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(paths.len(), 47, "46 app files and one test file");
    for path in &paths {
        assert_agrees(path);
    }
    // Every layer occurs in the bench app.
    for layer in [Layer::Types, Layer::Data, Layer::Core, Layer::Consumer] {
        assert!(paths
            .iter()
            .any(|path| NEXTJS.staging.layer_of(path) == Some(layer)));
    }
}

#[test]
fn globs_agree_with_bench_closures_on_edge_paths() {
    for path in [
        "types.ts",
        "types/index.ts",
        "src/types/index.ts",
        "lib/types/index.ts",
        "lib/types/model.test.ts",
        "next-env.d.ts",
        "lib/global.d.ts",
        "src/lib/util.ts",
        "src/lib/deep/util.ts",
        "lib/util.test.ts",
        "lib/util.spec.ts",
        "lib/seed.ts",
        "lib/seed.test.ts",
        "app/seed.tsx",
        "lib/view.tsx",
        "app/page.test.tsx",
        "components/Card.spec.tsx",
        "src/types/props.tsx",
        "app/lib/util.ts",
        "tests/01_tasks.test.ts",
        "README.md",
        "app/globals.css",
    ] {
        assert_agrees(path);
    }
}
