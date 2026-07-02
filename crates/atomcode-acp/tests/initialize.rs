// Minimal smoke: serve_stdio must exist and AcpServeOptions must be constructible.
#[test]
fn serve_stdio_symbol_exists() {
    // Compile-time proof the public surface exists; behavior covered in Task 11.
    let _f: fn(atomcode_acp::AcpServeOptions) -> _ = atomcode_acp::serve_stdio;
    let _opts = atomcode_acp::AcpServeOptions { engine: None, provider: None, auto_approve: false };
}
