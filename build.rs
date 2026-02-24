//! Build script that compiles Slint UI definitions into Rust at compile time.

fn main() {
    let style = std::env::var("SLINT_STYLE").unwrap_or_else(|_| "fluent-dark".to_string());
    let config = slint_build::CompilerConfiguration::new().with_style(style);
    slint_build::compile_with_config("src/roqtune.slint", config).unwrap();
}
