// Minimal TOML subset parser for loop.toml.
//
// The loop config only needs: top-level `key = "string"` pairs, comments,
// and one array of tables (`[[gates]]`) whose entries hold `key = "string"`
// pairs. Both TOML basic strings ("...", with escapes) and TOML literal
// strings ('...', no escapes) are supported. Anything else is a parse
// error, reported with the 1-based line number.
export function parseLoopToml(text, source = "loop.toml") {
  const root = {};
  let current = root;
  const lines = String(text).split(/\r?\n/);

  for (let i = 0; i < lines.length; i++) {
    const line = stripComment(lines[i]).trim();
    if (line === "") continue;

    const table = line.match(/^\[\[\s*([A-Za-z0-9_.-]+)\s*\]\]$/);
    if (table) {
      if (table[1] !== "gates") {
        throw tomlError(source, i, `unsupported table '[[${table[1]}]]' (only [[gates]] is allowed)`);
      }
      const gate = {};
      if (!Array.isArray(root.gates)) root.gates = [];
      root.gates.push(gate);
      current = gate;
      continue;
    }

    const kv = line.match(/^([A-Za-z0-9_-]+)\s*=\s*(.+)$/);
    if (!kv) {
      throw tomlError(source, i, `cannot parse '${lines[i].trim()}' (expected key = "value")`);
    }
    current[kv[1]] = parseValue(kv[2].trim(), source, i);
  }

  if (!Array.isArray(root.gates)) root.gates = [];
  return root;
}

function parseValue(raw, source, i) {
  if (raw.startsWith('"')) return parseBasicString(raw, source, i);
  if (raw.startsWith("'")) {
    if (raw.length < 2 || !raw.endsWith("'")) {
      throw tomlError(source, i, `unterminated literal string: ${raw}`);
    }
    return raw.slice(1, -1);
  }
  throw tomlError(source, i, `unsupported value '${raw}' (only strings are allowed)`);
}

// TOML basic string: double quotes, backslash escapes, no raw newlines.
function parseBasicString(raw, source, i) {
  let out = "";
  let j = 1;
  while (j < raw.length) {
    const ch = raw[j];
    if (ch === '"') {
      const rest = raw.slice(j + 1).trim();
      if (rest !== "") throw tomlError(source, i, `trailing characters after string: ${rest}`);
      return out;
    }
    if (ch === "\\") {
      const esc = raw[j + 1];
      const map = { '"': '"', "\\": "\\", n: "\n", r: "\r", t: "\t" };
      if (!(esc in map)) throw tomlError(source, i, `bad escape '\\${esc}'`);
      out += map[esc];
      j += 2;
      continue;
    }
    out += ch;
    j += 1;
  }
  throw tomlError(source, i, "unterminated string");
}

// Strip a trailing # comment, but not one inside a string.
function stripComment(line) {
  let inBasic = false;
  let inLiteral = false;
  for (let i = 0; i < line.length; i++) {
    const ch = line[i];
    if (inBasic) {
      if (ch === "\\") i += 1;
      else if (ch === '"') inBasic = false;
      continue;
    }
    if (inLiteral) {
      if (ch === "'") inLiteral = false;
      continue;
    }
    if (ch === '"') inBasic = true;
    else if (ch === "'") inLiteral = true;
    else if (ch === "#") return line.slice(0, i);
  }
  return line;
}

function tomlError(source, i, message) {
  return new Error(`${source}:${i + 1}: ${message}`);
}
