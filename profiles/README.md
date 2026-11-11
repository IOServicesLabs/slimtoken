# Profile library

This directory holds file-loaded stack profiles. A profile is a
JSON file. The engine loads every `*.json` file here with `--profile-dir`.

```bash
slimtoken run --architect --objective "Build a FastAPI inventory service" \
  --profile-dir profiles
```

The objective says "FastAPI", so auto detection selects the `fastapi`
profile from this directory. A host can also select a profile by name:

```bash
slimtoken run --architect --objective "..." --profile-dir profiles --profile fastapi
```

## File shape

```json
{
  "base": "python",
  "name": "fastapi",
  "markers": ["fastapi"],
  "planner_rules": "..."
}
```

| Field | Meaning |
|---|---|
| `base` | Required. A built-in profile (`python`, `nextjs`, `rust`). Behavior hooks come from it: validators, testability, scaffold, fixups. |
| `name` | Optional. Defaults to the file stem. Select it with `--profile`. |
| `markers` | Optional. Whole words in the objective that select this profile in auto detection. File profiles are checked before built-ins. |
| all string fields | Optional. Override the base profile's string (see `ProfileFile` in `src/profile/mod.rs`). Omitted fields keep the base value. |

## Rules

- Load order is sorted path order. A name loaded later wins in `named`.
- A file profile may redefine a built-in name. It then shadows the built-in.
- `--profile-file PATH` still wins over `--profile NAME` for selection.
- An edited file changes the manifest-cache key, so cached manifests do not
  go stale.
