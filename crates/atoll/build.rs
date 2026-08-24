//! Compile the Slint markup into Rust, reachable through `slint::include_modules!()`.

fn main() {
    slint_build::compile("ui/atoll.slint").expect("the Slint markup failed to compile");
}
