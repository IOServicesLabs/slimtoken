//! Rust 2021 CLI/library (cargo): contracts-first generation for a std-only
//! crate. Toolchain config (Cargo.toml) is rendered, not generated, so
//! `cargo test` runs offline with no external crates.

use super::{keep, Profile};

pub static RUST: Profile = Profile {
    name: "rust",
    markers: &["rust", "cargo"],
    comment: "//",
    example_module: "src/module.rs",
    example_export: "pub fn fn_name(arg: &str) -> Result<Output, Error>",
    example_test: "tests/all.rs",
    suite: "cargo test",
    planner_rules: "- Target Rust 2021, STANDARD LIBRARY ONLY (no external \
  crates; the crate must build and test offline).
- The crate name is `app` (see Cargo.toml). Library code lives in \
  `src/lib.rs` plus one `src/<module>.rs` per module; `src/lib.rs` contains \
  ONLY `pub mod <name>;` declarations. A binary entry point (`src/main.rs`) \
  calls the library through `app::<module>::<fn>` paths.
- In Rust exports, a `//` comment after the signature is the annotation \
  (instead of `# ...`).
- Error handling: library functions return `Result<_, E>` with the error \
  type the contract pins (a module-local `Error` enum or `String`); they \
  NEVER panic — no `unwrap`/`expect`/`panic!` outside tests. Contracts pin \
  exactly which inputs are `Err`.
- Structs/enums: field names and types EXACTLY as pinned; derive only what \
  the contract names (usually `Debug, PartialEq, Clone`).\n",
    edge_rule: EDGE_RULE,
    fragment_note: "",
    listed_module_exts: &[".rs"],
    module_head: "Program contracts — every module in this program. Call the \
             other modules only through these signatures:\n{all}\n\n\
             Rules:\n- Output exactly one fenced code block and nothing else — \
             no explanation, no headers.\n- Rust 2021, standard library only, \
             compact style, no boilerplate comments.\n- Library code never \
             panics: invalid input returns the pinned `Err` variant.\n- \
             Return EXACTLY the fields/variants named in an export's \
             return-shape annotation — no extra or missing members (tests \
             assert shapes exactly).\n- `src/lib.rs` is ONLY `pub mod ...;` \
             lines; a `src/main.rs` CLI entry parses args, calls library \
             functions, prints the pinned output and RETURNS the exit code \
             as `std::process::ExitCode` (0 success, 2 usage error).\n\n",
    test_head: "Module contracts — use ONLY these signatures; {visibility}:\n{all}\n\n\
             Rules:\n- Output exactly one fenced code block and nothing else.\n\
             - Each test shard is its own integration crate under tests/, so \
             import library items as `use app::<module>::<fn>;`.\n- Cover \
             happy paths, invalid input (the pinned `Err` cases) and edge \
             cases; never write placeholder or tautological tests.\n- Where \
             an export annotates its return shape, assert it exactly \
             (assert_eq! on the whole value).\n- Keep tests compact.\n{silent}\n",
    density_rule: "- Density: no comments except the contract `//` \
                 annotations on exports; no comments inside test functions; \
                 do not re-export or wrap another module's function — call \
                 it from its defining module.\n\n",
    type_ownership_rule: "- Every struct/enum is defined in exactly one \
                 module and imported by path everywhere else; do not \
                 duplicate a type definition across modules.",
    // No in-run Rust syntax validator: a broken artifact fails at the
    // external acceptance gate (cargo test) instead of repairing in-run.
    validates: |_| false,
    is_testable: |_| true,
    // lib.rs declares every module and main.rs consumes the library, so both
    // wait for the in-wave wiring task.
    is_consumer: |path| {
        path == "src/lib.rs" || path == "src/main.rs" || path.starts_with("src/bin/")
    },
    shard_path: |index, module| {
        let name = module.rsplit('/').next().unwrap_or("part");
        let stem = name.trim_end_matches(".rs");
        format!("tests/{index:02}_{stem}.rs")
    },
    scaffold,
    normalize_skeleton: keep,
    normalize_manifest: keep,
    readme_sections: |_| {
        "\n## Run\n\n```\ncargo run -- <args>\n```\n\n## Tests\n\n```\ncargo test\n```\n"
            .to_string()
    },
};

/// The shared edge rule in Rust comment syntax.
const EDGE_RULE: &str = "
- After each export, a `//` comment pinning its edge behavior in a few words:
  empty/zero input result, which invalid inputs return which `Err` variant,
  missing-key/not-found result, and exact bounds (e.g.
  `pub fn make(n: usize) -> Vec<Item>  // n===0 -> []; n<0 -> Err(Error::Range)`).
  An export returning a struct starts that comment with its
  `{field: type}` shape and puts the edges after it — an edge note never
  replaces the shape. Pin every derived quantity a test could assert: each
  returned Vec's length relative to the inputs, how returned items relate
  to input items, and whether initial/default collections start empty or
  seeded.";

/// Deterministic toolchain file: crate name `app`, std-only, offline.
fn scaffold(path: &str) -> Option<String> {
    if path == "Cargo.toml" {
        Some(
            r#"```toml
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
```"#
                .to_string(),
        )
    } else {
        None
    }
}
