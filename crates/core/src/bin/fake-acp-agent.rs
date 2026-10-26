//! A scripted ACP agent for tests (see `chaos_core::fake`).

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = chaos_core::fake::binary_main().await {
        eprintln!("fake-acp-agent: {e}");
        std::process::exit(1);
    }
}
