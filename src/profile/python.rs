//! Python 3.10 + pytest: the profile the planner was first tuned on.

use super::Profile;
use crate::architect::Manifest;

pub static PYTHON: Profile = Profile {
    name: "python",
    markers: &[],
    comment: "#",
    example_module: "module.py",
    example_export: "def fn(arg: str) -> dict",
    example_test: "tests/test_all.py",
    suite: "pytest",
    planner_rules: "- Target Python 3.10: match/case is fine; NEVER reuse a quote character
  inside an f-string, no 3.12-only syntax.",
    edge_rule: EDGE_RULE,
    fragment_note: "",
    listed_module_exts: &[".py"],
    module_head: "Program contracts — every module in this program. Call the other \
             modules only through these signatures:\n{all}\n\n\
             Rules:\n- Output exactly one fenced code block and nothing else — no \
             explanation, no headers.\n- Compact style: one-line docstrings, no \
             boilerplate comments.\n- Standard library only; Python 3.10 compatible; \
             never shadow builtins.\n- Invalid input raises ValueError unless the \
             contract says otherwise.\n- Return EXACTLY the fields named in an \
             export's return-shape annotation — no extra or missing keys (tests \
             assert shapes exactly).\n- CLI commands accept exactly the flags \
             the contract's grammar annotation names.\n- If this file is a CLI \
             entry point, main \
             RETURNS the exit code (0 success, 2 usage error) — never sys.exit.\n\n",
    test_head: "Module contracts — use ONLY these signatures; {visibility}:\n{all}\n\n\
             Rules:\n- Output exactly one fenced code block and nothing else.\n\
             - Cover happy paths, invalid input (ValueError paths), and \
             persistence/reload behavior where applicable.\n- If the contract \
             includes a CLI entry point returning an exit code, assert the \
             return value for invalid usage, e.g. `assert cli.main([\"add\"]) \
             == 2` — never write placeholder or tautological tests; every test \
             must exercise a real signature from the contracts.\n- Where an \
             export annotates its return shape, assert that shape exactly (full \
             dict equality); for CLI tests, drive the annotated flag grammar \
             character for character.\n- Use \
             tmp_path for files; keep tests compact.\n{silent}\n",
    // Python modules and suites are syntax-validated in-run when the run has
    // a validator, so a broken artifact repairs instead of failing at
    // external pytest. The bench validator is Python-only.
    validates: |path| path.ends_with(".py"),
    // A/B (`--architect-density`): the built-in head already bans
    // boilerplate comments; this tier bans docstrings and non-contract
    // comments everywhere and re-export facades. Contract `#` annotations
    // (edge rules) are load-bearing and stay.
    density_rule: "- Density: no docstrings anywhere and no comments except \
                 the contract `#` annotations on exports; no comments inside \
                 test functions; do not re-export or wrap another module's \
                 function — import it from its defining module at the use \
                 site.\n\n",
    // Python contracts pin dict shapes inline, so there is no shared-type
    // duplication class to pin.
    type_ownership_rule: "",
    is_testable: |_| true,
    // Orchestrators (engine.py, cli.py) consume as much as entry points do,
    // and a module's role is not visible from its path.
    is_consumer: |path| !path.starts_with("tests/"),
    shard_path: |index, module| {
        let name = module.rsplit('/').next().unwrap_or("part");
        format!("tests/test_{index:02}_{}.py", name.trim_end_matches(".py"))
    },
    scaffold: |_| None,
    normalize_skeleton: package_if_shadowing,
    normalize_manifest: package_if_shadowing,
    readme_sections,
};

/// Contracts pin edge behavior (`ArchitectOptions::edges`). Module and test
/// workers are written blind to each other from these contracts; unstated
/// edge behavior is where they disagree (DeepSeek harness: every own-test
/// failure was an edge semantic, e.g. zero length -> [] vs ValueError).
const EDGE_RULE: &str = "
- After each def, a `#` comment pinning its edge behavior in a few words:
  empty/zero input result, which invalid inputs raise which exception,
  missing-key/not-found result, and exact bounds (e.g.
  `def make(n: int) -> list  # n==0 -> []; n<0 -> ValueError`). A def
  returning dict or list[dict] starts that comment with its `{key: type}`
  shape and puts the edges after it (e.g. `def tally(rows: list) -> dict  #
  {total: float, count: int}; rows==[] -> {total: 0.0, count: 0}`) — an edge
  note never replaces the shape.";

fn readme_sections(manifest: &Manifest) -> String {
    let entry = manifest.files.iter().find(|file| {
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        matches!(name, "cli.py" | "main.py" | "__main__.py" | "app.py")
    });
    let run = match entry {
        Some(file) => format!("python {} --help", file.path),
        None => "python -c \"import ...\"  # import the modules above".to_string(),
    };
    format!("\n## Run\n\n```\n{run}\n```\n\n## Tests\n\n```\npython -m pytest -q\n```\n```")
}

/// Standard-library module names a project module must not shadow: a
/// top-level `site.py` replaces Python's own `site` at interpreter start-up
/// (sitegen objective: every harness run's suite died on import).
const STDLIB_NAMES: &[&str] = &[
    "abc",
    "array",
    "ast",
    "base64",
    "bisect",
    "builtins",
    "calendar",
    "cmd",
    "code",
    "collections",
    "copy",
    "csv",
    "dataclasses",
    "datetime",
    "decimal",
    "email",
    "enum",
    "functools",
    "glob",
    "hashlib",
    "heapq",
    "html",
    "http",
    "inspect",
    "io",
    "json",
    "keyword",
    "locale",
    "logging",
    "mailbox",
    "math",
    "mimetypes",
    "numbers",
    "operator",
    "os",
    "parser",
    "pathlib",
    "pickle",
    "platform",
    "profile",
    "queue",
    "random",
    "re",
    "secrets",
    "select",
    "shelve",
    "shutil",
    "signal",
    "site",
    "socket",
    "ssl",
    "stat",
    "statistics",
    "string",
    "struct",
    "subprocess",
    "sys",
    "tempfile",
    "test",
    "textwrap",
    "threading",
    "time",
    "timeit",
    "token",
    "tokenize",
    "trace",
    "types",
    "typing",
    "unittest",
    "uuid",
    "warnings",
    "wave",
    "xml",
    "zipfile",
];

/// Package directory used when the objective's module names collide.
const COLLISION_PACKAGE: &str = "app";

/// If any top-level Python module would shadow a standard-library module,
/// move every non-test module under `app/` (one consistent package, so the
/// imports workers derive from the paths stay coherent). Idempotent.
fn package_if_shadowing(manifest: &mut Manifest) {
    let top_level = |path: &str| !path.contains('/') && path.ends_with(".py");
    let shadows = manifest.files.iter().any(|file| {
        top_level(&file.path) && STDLIB_NAMES.contains(&file.path.trim_end_matches(".py"))
    });
    if !shadows {
        return;
    }
    for file in &mut manifest.files {
        if top_level(&file.path) && !file.path.starts_with("test_") {
            file.path = format!("{COLLISION_PACKAGE}/{}", file.path);
        }
    }
    tracing::warn!(
        package = COLLISION_PACKAGE,
        "module names shadow the standard library; modules moved into a package"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architect::ManifestFile;

    #[test]
    fn stdlib_shadowing_modules_move_into_a_package() {
        let file = |path: &str| ManifestFile {
            path: path.into(),
            purpose: "p".into(),
            exports: vec![],
            uses: Vec::new(),
        };
        let mut manifest = Manifest {
            files: vec![file("markdown.py"), file("site.py"), file("cli.py")],
            test_files: vec![file("tests/test_all.py")],
            readme: false,
        };
        package_if_shadowing(&mut manifest);
        let paths: Vec<_> = manifest.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["app/markdown.py", "app/site.py", "app/cli.py"]);
        assert_eq!(manifest.test_files[0].path, "tests/test_all.py");
        package_if_shadowing(&mut manifest);
        assert_eq!(manifest.files[1].path, "app/site.py", "idempotent");

        let mut clean = Manifest {
            files: vec![file("engine.py"), file("stats.py")],
            test_files: vec![],
            readme: false,
        };
        package_if_shadowing(&mut clean);
        assert_eq!(clean.files[0].path, "engine.py", "no collision, no move");
    }
}
