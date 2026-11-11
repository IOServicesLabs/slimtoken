//! TypeScript / Next.js App Router + Vitest: same structure as Python
//! (contracts first, one parallel wave, sharded suites) with its own
//! conventions, rendered toolchain files and route-file fixups.

use super::{keep, Profile};
use crate::architect::Manifest;

pub static NEXTJS: Profile = Profile {
    name: "nextjs",
    markers: &["next.js", "nextjs", "typescript", "react", ".tsx"],
    comment: "//",
    example_module: "lib/module.ts",
    example_export: "export function fn(arg: string): Result",
    example_test: "tests/all.test.ts",
    suite: "Vitest",
    planner_rules: "- Target strict TypeScript with Next.js 16 App Router (`app/`
  directory, React 19 function components) unless the objective says
  otherwise: `.ts` for logic, `.tsx` for components and pages. In Next 16,
  route `params`/`searchParams` props are Promises (await them). React 19
  has no global `JSX` namespace: never write `JSX.Element`; leave component
  return types inferred.
- Put ALL domain logic (game rules, scoring, scheduling, persistence) in pure
  `lib/*.ts` modules with no React imports; components only call lib exports.
  Tests exercise lib modules directly.
- Include `package.json` (dependencies next, react, react-dom; devDependencies
  typescript, @types/react, @types/react-dom, @types/node, vitest; scripts dev,
  build, start, test=`vitest run`), `tsconfig.json` and `next.config.mjs` as
  files with exports []. Imports between project files use relative paths.
- In TypeScript exports, a `// ...` comment after the signature is the
  annotation (instead of `# ...`).
- Route files (`app/**/page.tsx`, `layout.tsx`) export ONLY their default
  component (a layout may also export `metadata`); reusable components go in
  `components/*.tsx`.
- Styling contract: the global stylesheet's exports list EVERY CSS class the
  app uses, one per entry with its role (e.g. `.nav  // top bar, horizontal
  flex links`, `.card  // bordered panel`; up to 16 entries — the stylesheet
  is exempt from the 4-export limit); components use only those class names,
  character for character.",
    edge_rule: EDGE_RULE,
    fragment_note: " (a stylesheet instead lists up to 16 `.class  // role` entries, \
                 every class the app uses)",
    listed_module_exts: &[],
    module_head: "Program contracts — every file in this program. Call the other \
             modules only through these signatures:\n{all}\n\n\
             Rules:\n- Output exactly one fenced code block and nothing else — \
             no explanation, no headers.\n- Strict TypeScript; compact style, no \
             boilerplate comments.\n- Imports between project files use relative \
             paths; use only packages listed in package.json.\n- Invalid input \
             throws an Error (RangeError/TypeError) unless the contract says \
             otherwise.\n- Return EXACTLY the fields named in an export's \
             return-shape annotation — no extra or missing keys (tests assert \
             shapes exactly).\n- lib/ modules are pure: no React imports, no \
             window/document/localStorage access at import time (guard browser \
             APIs with `typeof window !== \"undefined\"`).\n- Components and \
             pages that use state, effects or event handlers start with \
             \"use client\".\n- Route files (app/**/page.tsx, layout.tsx) export \
             ONLY the default component (layouts may add `metadata`); helper \
             components stay local and unexported.\n- React 19: no global `JSX` \
             namespace — never write `JSX.Element`; leave component return types \
             inferred.\n- CSS classes: use exactly the class names the stylesheet's \
             contract lists; the stylesheet defines every one of them.\n- \
             Config/JSON files: emit the file content verbatim in the fenced \
             block.\n\n",
    test_head: "Module contracts — use ONLY these signatures; {visibility}:\n{all}\n\n\
             Rules:\n- Output exactly one fenced code block and nothing else.\n\
             - Vitest: `import { describe, it, expect } from \"vitest\"`; the \
             suite lives in tests/ at the project root, so import modules as \
             `../lib/<name>`.\n- Test the pure lib/ modules only: no DOM, no \
             React rendering; pass explicit timestamps/seeds where a contract \
             takes them.\n- Cover happy paths, invalid input (thrown errors) and \
             edge cases; never write placeholder or tautological tests.\n- Where \
             an export annotates its return shape, assert it exactly \
             (toEqual).\n- Keep tests compact.\n{silent}\n",
    // A/B (`--architect-density`): the built-in head already bans
    // boilerplate comments; this tier bans docstrings and non-contract
    // comments everywhere and re-export facades. Contract `//` annotations
    // (edge rules) are load-bearing and stay.
    density_rule: "- Density: no docstrings or JSDoc anywhere and no comments \
                 except the contract `//` annotations on exports; no \
                 comments inside test functions; do not re-export or wrap \
                 another module's function — import it from its defining \
                 module at the use site.\n\n",
    // the Digest manifest defined `Article` in both lib/types.ts and
    // lib/articles.ts and they drifted (`notes?: string` vs `notes: string`)
    // — 11-12 tsc errors in every replay arm. One owner, import elsewhere.
    type_ownership_rule: "- Every interface/type alias is defined in exactly \
                 ONE owner file (a lib/types.ts when the objective names \
                 shared shapes, else the domain module that owns it) and \
                 listed among that file's exports; every other file imports \
                 it from the owner by relative path. NEVER re-declare a \
                 structurally identical type in a second file.\n",
    validates: |_| false,
    is_testable,
    is_consumer: |path| path.starts_with("app/") || path.starts_with("components/"),
    shard_path: |index, module| {
        let name = module.rsplit('/').next().unwrap_or("part");
        let stem = name.trim_end_matches(".tsx").trim_end_matches(".ts");
        format!("tests/{index:02}_{stem}.test.ts")
    },
    scaffold,
    normalize_skeleton: keep,
    normalize_manifest: restrict_route_exports,
    readme_sections: |_| {
        "\n## Run\n\n```\nnpm install\nnpm run dev\n```\n\n## Tests\n\n```\nnpm test\n```\n```"
            .to_string()
    },
};

/// `PYTHON`'s edge rule in TypeScript syntax.
const EDGE_RULE: &str = "
- After each export, a `//` comment pinning its edge behavior in a few words:
  empty/zero input result, which invalid inputs throw which error,
  missing-key/not-found result, and exact bounds (e.g.
  `export function make(n: number): number[]  // n===0 -> []; n<0 -> throws RangeError`).
  An export returning an object type starts that comment with its
  `{key: type}` shape and puts the edges after it — an edge note never
  replaces the shape. Pin every derived quantity a test could assert: each
  returned array's length relative to the inputs (e.g. `sequence.length ===
  length`, not nextLength), how returned items relate to input items, and
  whether initial/default collections start empty or seeded (e.g. `decks:
  the built-in decks; cards: []`).";

/// Suites cover the pure logic modules only: components, routes and config
/// need a DOM or a framework runtime that a contract-only test writer
/// cannot drive.
fn is_testable(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    path.ends_with(".ts")
        && !name.ends_with(".d.ts")
        && !name.contains("config")
        && !path.starts_with("app/")
}

/// Deterministic Next.js toolchain files (fenced, like a worker's output).
/// Toolchain config is rendered, not generated: a worker-written tsconfig
/// enabled noUncheckedIndexedAccess and broke 8 files written to standard
/// strictness (nextjs-memory-2), and model-pinned versions were an
/// advisory-flagged next@14.2.5. Caret ranges let npm resolve the newest
/// patched release in each line. Next 16 / React 19 (current as of 2026-10);
/// TypeScript stays on 5.x because Next's build-time type check drives the
/// TypeScript JS API, which the native 7.x compiler does not ship yet.
fn scaffold(path: &str) -> Option<String> {
    let body = match path {
        "package.json" => {
            r#"```json
{
  "name": "app",
  "version": "0.1.0",
  "private": true,
  "scripts": {
    "dev": "next dev",
    "build": "next build",
    "start": "next start",
    "test": "vitest run"
  },
  "dependencies": {
    "next": "^16.4.3",
    "react": "^19.3.1",
    "react-dom": "^19.3.1"
  },
  "devDependencies": {
    "@types/node": "^24.19.2",
    "@types/react": "^19.3.0",
    "@types/react-dom": "^19.3.0",
    "typescript": "^5.9.3",
    "vitest": "^5.0.5"
  }
}
```"#
        }
        "tsconfig.json" => {
            r#"```json
{
  "compilerOptions": {
    "target": "ES2017",
    "lib": ["dom", "dom.iterable", "esnext"],
    "allowJs": true,
    "skipLibCheck": true,
    "strict": true,
    "noEmit": true,
    "esModuleInterop": true,
    "module": "esnext",
    "moduleResolution": "bundler",
    "resolveJsonModule": true,
    "isolatedModules": true,
    "jsx": "react-jsx",
    "incremental": true,
    "plugins": [{ "name": "next" }],
    "paths": { "@/*": ["./*"] }
  },
  "include": [
    "next-env.d.ts",
    "**/*.ts",
    "**/*.tsx",
    ".next/types/**/*.ts",
    ".next/dev/types/**/*.ts"
  ],
  "exclude": ["node_modules", ".check"]
}
```"#
        }
        "next.config.mjs" | "next.config.js" => {
            "```js\n/** @type {import('next').NextConfig} */\nconst nextConfig = {};\n\nexport default nextConfig;\n```"
        }
        _ => return None,
    };
    Some(body.to_string())
}

/// Exports a Next.js App Router route file may have besides its default.
const ROUTE_EXPORTS: &[&str] = &[
    "metadata",
    "generateMetadata",
    "viewport",
    "generateViewport",
    "generateStaticParams",
    "revalidate",
    "dynamic",
    "runtime",
];

/// Next.js rejects any other named export from `page`/`layout` files at
/// build time (`next build`: "Property 'SessionTable' is incompatible with
/// index signature"). A contract listing one makes the worker export it, so
/// drop those exports: the worker then keeps the helper local to the file.
fn restrict_route_exports(manifest: &mut Manifest) {
    for file in &mut manifest.files {
        // React 19 dropped the global `JSX` namespace; contracts annotated
        // `: JSX.Element` and every component copied it (TS2503 in 8 files,
        // nextjs-memory-3). Inferred return types are correct and portable.
        for export in &mut file.exports {
            *export = export.replace("): JSX.Element", ")");
        }
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        let route = file.path.starts_with("app/")
            && ["page.tsx", "layout.tsx", "page.ts", "layout.ts"].contains(&name);
        if !route {
            continue;
        }
        file.exports.retain(|export| {
            let head = export.split("//").next().unwrap_or(export);
            head.contains("export default")
                || head
                    .split(|ch: char| !ch.is_alphanumeric())
                    .any(|word| ROUTE_EXPORTS.contains(&word))
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::architect::ManifestFile;

    #[test]
    fn route_files_keep_only_default_and_framework_exports() {
        let file = |path: &str, exports: &[&str]| ManifestFile {
            path: path.into(),
            purpose: "x".into(),
            exports: exports.iter().map(|e| e.to_string()).collect(),
            uses: Vec::new(),
        };
        let mut manifest =
            Manifest {
                files: vec![
                file(
                    "app/progress/page.tsx",
                    &[
                        "export default function ProgressPage(): JSX.Element",
                        "export function SessionTable(props: {sessions: Session[]}): JSX.Element",
                    ],
                ),
                file(
                    "app/layout.tsx",
                    &[
                        "export const metadata: Metadata  // {title}",
                        "export default function RootLayout(): JSX.Element",
                    ],
                ),
                file("components/table.tsx", &["export function SessionTable(): JSX.Element"]),
            ],
                test_files: vec![],
                readme: false,
            };
        restrict_route_exports(&mut manifest);
        assert_eq!(manifest.files[0].exports.len(), 1);
        assert!(manifest.files[0].exports[0].contains("export default"));
        assert!(!manifest.files[0].exports[0].contains("JSX"));
        assert_eq!(manifest.files[1].exports.len(), 2, "metadata stays");
        assert_eq!(manifest.files[2].exports.len(), 1, "non-route untouched");
    }
}
