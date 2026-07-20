/// Manual sanity check: run with piped initialize frame to verify round-trip.
/// Usage: echo '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-01-01"}}' | cargo run -p atomcode --example acp_stdio_agent
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    atomcode::acp::serve_stdio(atomcode::acp::AcpServeOptions::default()).await
}
