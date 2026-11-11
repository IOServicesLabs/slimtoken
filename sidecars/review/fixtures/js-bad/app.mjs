// Broken fixture: unbalanced brace, fails node --check.
export function broken() {
  const x = 1;
  if (x > 0) {
    return x;
// missing closing braces on purpose
