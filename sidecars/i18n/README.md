# slimtoken-i18n

i18n parity sidecar for the SlimToken engine. This pack is a side
installable: the engine stays bare, and a host adds this pack when it
runs translation waves. Zero runtime dependencies. Node 22 or higher.
Zero model tokens: the gate is pure JSON parsing.

## What is in the pack

| Piece | Role |
|---|---|
| `check` (`lib/check.mjs`) | Parity gate: key-set parity, no empty values, placeholder parity across locales, untranslated warnings. |

## Install

```
npm install slimtoken-i18n
```

Or run it straight from a checkout:

```
node sidecars/i18n/bin/slimtoken-i18n.mjs help
```

## Use

```
slimtoken-i18n check <dir> [--source <locale>] [--out DIR]
```

`<dir>` holds the locale files. The gate detects three layouts (first
find wins per locale name):

- Flat: `en.json`, `fr.json` directly in `<dir>`.
- Nested: `locales/en.json`, `locales/fr.json` under `<dir>`.
- Per-locale dirs: `en/messages.json`, `fr/messages.json` (locale =
  directory name).

Locale = filename stem (`en.json` -> `en`). Nested objects flatten to
dot paths (`nav.cart`); non-string leaves are ignored.

Hard checks (exit 1):

- Key-set parity: every locale must carry every key from the union of
  all keys. `missing key '<k>' in <locale>`.
- Empty values: `empty value for '<k>' in <locale>`.
- Placeholder parity: for each key, the placeholder set must match the
  source locale. Placeholders: ICU `{name}` (an ICU type suffix such as
  `{count, number}` is ignored), printf `%s` / `%d` (`%%` is a literal
  percent, not a placeholder), and mustache `{{var}}`.
  `placeholder mismatch for '<k>': <locale> has {..}, source has {..}`.

Warnings (never fail): a value identical to the source locale's value
for the same key. `untranslated '<k>' in <locale>`.

`--source <locale>` names the reference locale for placeholder and
untranslated comparison. Default: the first locale in alphabetical
order. `--out DIR` writes `report.md` (one glance for a human) and
`i18n.json` (machine twin: `{ok, locales: [{locale, missing, empty,
placeholderMismatches, untranslated}], summary}`; field names are
stable, new fields append at the end).

Exit codes: 0 pass (warnings allowed), 1 findings, 2 usage error or no
locale files found.

## Host wiring (SlimToken engine)

The engine owns gates; the host owns the judge (see `EMBEDDING.md` at
the repo root and `sidecars/README.md`). Typical wiring:

1. Engine runs a translation wave: one draft task per locale per
   string, all in parallel, from the source locale.
2. Host runs `slimtoken-i18n check` on the locale dir.
3. The pack proves the mechanical invariants (parity, no empty values,
   placeholder agreement) for free — zero tokens.
4. The host judges translation quality with a language-capable judge
   and reads `i18n.json` / `report.md`. The pack never judges
   translation quality; it only proves the structure.

## Development

The canonical home of this pack is `sidecars/i18n/` in the slimtoken
repo. Keep zero runtime dependencies. Keep the gate zero-token (pure
JSON parsing, no model calls, no network). The `selftest` script in
`package.json` must stay green:

```
node test/selftest.mjs
```
