// Shared helpers for the slimtoken-triage pack. Zero dependencies; Node 22+.

// Parse CLI args into { positional, flags }. Flags are --name value or
// --name=value. A flag with no value gets the string "true".
export function parseArgs(args) {
  const positional = [];
  const flags = {};
  for (let i = 0; i < args.length; i += 1) {
    const arg = args[i];
    if (arg.startsWith("--")) {
      const eq = arg.indexOf("=");
      if (eq >= 0) {
        flags[arg.slice(2, eq)] = arg.slice(eq + 1);
      } else if (i + 1 < args.length && !args[i + 1].startsWith("--")) {
        flags[arg.slice(2)] = args[i + 1];
        i += 1;
      } else {
        flags[arg.slice(2)] = "true";
      }
    } else {
      positional.push(arg);
    }
  }
  return { positional, flags };
}

export function parsePositiveInt(value, flagName) {
  const n = Number.parseInt(value, 10);
  if (!Number.isInteger(n) || n < 1) {
    return { error: `--${flagName} must be a positive integer (got '${value}')` };
  }
  return { value: n };
}

// Check the model environment. Returns the missing variable names, or [].
export function missingEnv(names) {
  return names.filter((name) => !process.env[name] || process.env[name].length === 0);
}

// Take at most the first `max` lines of a string.
export function excerpt(text, max = 10) {
  return String(text || "")
    .split(/\r?\n/)
    .slice(0, max)
    .join("\n");
}
