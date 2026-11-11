// Broken fixture: calls an unknown function, fails cargo check / clippy.
pub fn broken() -> i32 {
    no_such_function()
}
