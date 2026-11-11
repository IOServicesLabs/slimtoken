// compare.mjs — paired-arm comparison and the regression verdict.
//
// The A/B bar (no claim without a pair): arm B must match or
// improve arm A on every gated metric, or the comparison is a
// regression. Gated metrics:
//   - wall:   B.wall_ms must be <= A.wall_ms (when both are present)
//   - gates:  every gate A recorded as ok:true must be ok:true in B
//             (a gate A never passed imposes no bar; a gate missing or
//             non-true in B is a regression when A had it true)
// Token counts are reported as deltas but are not gated: token burn is
// a judge concern, not a pass/fail bar.
export function compareArms(rowA, rowB) {
  const regressions = [];

  if (rowA.wall_ms !== null && rowA.wall_ms !== undefined
      && rowB.wall_ms !== null && rowB.wall_ms !== undefined) {
    if (rowB.wall_ms > rowA.wall_ms) {
      regressions.push({
        metric: "wall_ms",
        detail: `wall ${rowB.wall_ms} ms > ${rowA.wall_ms} ms (+${rowB.wall_ms - rowA.wall_ms} ms)`,
      });
    }
  }

  const gatesA = rowA.gates && typeof rowA.gates === "object" ? rowA.gates : {};
  const gatesB = rowB.gates && typeof rowB.gates === "object" ? rowB.gates : {};
  for (const [gate, okA] of Object.entries(gatesA)) {
    if (okA !== true) continue;
    if (gatesB[gate] !== true) {
      regressions.push({
        metric: `gate:${gate}`,
        detail: `gate "${gate}" was ok:true in A but ${gatesB[gate] === undefined ? "missing" : String(gatesB[gate])} in B`,
      });
    }
  }

  return {
    ok: regressions.length === 0,
    regressions,
  };
}

// Delta of B over A, with a percent when A is non-zero. Null when
// either side is missing.
export function delta(a, b) {
  if ((a === null || a === undefined) || (b === null || b === undefined)) return null;
  const diff = b - a;
  const pct = a !== 0 ? (diff / a) * 100 : null;
  return { diff, pct };
}
