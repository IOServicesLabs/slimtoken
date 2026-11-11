// Bad target for the slimtoken-fuzz selftest: imports fine but does not
// export a fuzz function, so the pack must exit 2.
export const somethingElse = 42;
