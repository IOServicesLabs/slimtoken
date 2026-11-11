// Fake axe-core UMD bundle: defines window.axe with a run() that reports
// no violations. Used by the audit-ext selftest to prove that --axe passes
// a clean route.
(function (root, factory) {
  root.axe = factory();
})(typeof window !== "undefined" ? window : this, function () {
  return { run: async () => ({ violations: [] }) };
});
