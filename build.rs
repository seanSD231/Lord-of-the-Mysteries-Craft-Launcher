fn main() {
    // Only the GUI binary needs Tauri's build step; the CLI is plain Rust.
    #[cfg(feature = "gui")]
    tauri_build::build();
}
