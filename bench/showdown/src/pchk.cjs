// The persistent type checker of the showdown's `pchk` switch (see `pchk.rs`).
//
// One process for one benchmark run. It keeps a TypeScript builder program
// for the project in the current directory, so a check after a file changed
// re-checks only the files that the change affects (about 0.35 s for the last
// page of a 45-file app, against about 1.5 s for a cold `tsc --noEmit`).
//
// Protocol: one JSON object per line on stdin, one per line on stdout.
//   {"op":"changed","paths":["lib/a.ts", ...]}  files were written or removed
//                                              (no answer; a check of the new
//                                              state starts in the background)
//   {"op":"check","id":7}                      answer {"id":7,"ok":bool,"out":text,"ms":n}
// `out` is what `tsc --noEmit -p .` prints for the same files: the same
// diagnostics in the same order and form (`file(line,col): error TSn: text`).
'use strict';
const path = require('path');
const readline = require('readline');
const { performance } = require('perf_hooks');
const ts = require(process.argv[2]);
const root = process.cwd();

// Parsed source files by absolute path. Library files stay for the whole run.
const cache = new Map();
let versions = 0;
let host = null;
let hostKey = '';
let builder;
let dirty = true;
let last = null;
let timer = null;

function makeHost(options) {
  const made = ts.createCompilerHost(options);
  const base = made.getSourceFile.bind(made);
  made.getSourceFile = (fileName, languageVersion, onError, shouldCreate) => {
    const key = path.resolve(fileName);
    if (cache.has(key)) return cache.get(key);
    const file = base(fileName, languageVersion, onError, shouldCreate);
    if (file) file.version = String(++versions);
    cache.set(key, file);
    return file;
  };
  return made;
}

const formatHost = {
  getCanonicalFileName: (name) => name,
  getCurrentDirectory: () => root,
  getNewLine: () => '\n',
};

// The diagnostics of `tsc -p .` (`emitFilesAndReportErrors`): the semantic
// ones only when there is no syntax, option or global error.
function check() {
  const started = performance.now();
  let fatal = null;
  const config = ts.getParsedCommandLineOfConfigFile(
    path.join(root, 'tsconfig.json'),
    { noEmit: true },
    { ...ts.sys, onUnRecoverableConfigFileDiagnostic: (d) => { fatal = d; } },
  );
  if (!config) {
    const text = fatal ? ts.formatDiagnostics([fatal], formatHost) : 'error: no tsconfig.json\n';
    return { ok: false, out: text, ms: performance.now() - started };
  }
  const options = { ...config.options, noEmit: true, incremental: false, tsBuildInfoFile: undefined };
  const key = JSON.stringify(options);
  if (key !== hostKey) {
    host = makeHost(options);
    hostKey = key;
    builder = undefined;
  }
  builder = ts.createSemanticDiagnosticsBuilderProgram(
    config.fileNames, options, host, builder, ts.getConfigFileParsingDiagnostics(config),
  );
  const all = builder.getConfigFileParsingDiagnostics().slice();
  const configCount = all.length;
  all.push(...builder.getSyntacticDiagnostics());
  if (all.length === configCount) {
    all.push(...builder.getOptionsDiagnostics());
    all.push(...builder.getGlobalDiagnostics());
    if (all.length === configCount) {
      all.push(...builder.getSemanticDiagnostics());
    }
  }
  const diagnostics = ts.sortAndDeduplicateDiagnostics(all);
  const errors = diagnostics.filter((d) => d.category === ts.DiagnosticCategory.Error);
  return {
    ok: errors.length === 0,
    out: ts.formatDiagnostics(diagnostics, formatHost),
    ms: performance.now() - started,
  };
}

function current() {
  if (dirty || !last) {
    dirty = false;
    try {
      last = check();
    } catch (error) {
      last = { ok: false, out: `error: the checker failed: ${String(error && error.stack || error).split('\n')[0]}\n`, ms: 0, failed: true };
      builder = undefined;
    }
  }
  return last;
}

function schedule() {
  if (timer) return;
  timer = setTimeout(() => {
    timer = null;
    if (dirty) current();
  }, 25);
}

const input = readline.createInterface({ input: process.stdin });
input.on('line', (line) => {
  let message;
  try {
    message = JSON.parse(line);
  } catch {
    return;
  }
  if (message.op === 'changed') {
    for (const name of message.paths || []) cache.delete(path.resolve(root, name));
    dirty = true;
    schedule();
  } else if (message.op === 'check') {
    const waited = dirty;
    const result = current();
    process.stdout.write(JSON.stringify({ id: message.id, ok: result.ok, out: result.out, ms: waited ? result.ms : 0, failed: !!result.failed }) + '\n');
  }
});
input.on('close', () => process.exit(0));
schedule();
