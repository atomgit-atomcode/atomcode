/// Manual sanity check: run with piped initialize frame to verify round-trip.
/// Usage: echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-01-01"}}' | cargo run -p atomcode-acp --example stdio_agent
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    atomcode_acp::serve_stdio(atomcode_acp::AcpServeOptions::default()).await
}
