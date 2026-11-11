// Fake axe-core UMD bundle: defines window.axe with a run() that reports
// one serious-impact violation. Used by the audit-ext selftest to prove
// that --axe fails a route on serious impact.
(function (root, factory) {
  root.axe = factory();
})(typeof window !== "undefined" ? window : this, function () {
  return {
    run: async () => ({
      violations: [
        { id: "color-contrast", impact: "serious", help: "Elements must have sufficient color contrast", nodes: [{}] },
      ],
    }),
  };
});
