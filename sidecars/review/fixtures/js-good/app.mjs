// Clean ES module fixture: passes node --check.
export function greet(name) {
  return `hello, ${name}`;
}

export function main() {
  console.log(greet("review"));
}
