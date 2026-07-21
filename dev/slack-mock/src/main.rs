//! A fake Slack for local development. **Not shipped, and excluded from the
//! coverage gate** — this is tooling, not product.
//!
//! Phase 4 grows this into a real web UI that serves `chat.postMessage`,
//! `chat.update` and `auth.test`, and renders both messages *and threads*, so
//! the end-to-end threading behaviour can be seen without a Slack workspace.
//!
//! Phase 0 is a stub: it exists so `compose.yaml` has something to start and so
//! the workspace member is real.

fn main() {
    println!("slack-mock stub: Phase 0 placeholder, no HTTP server yet.");

    // Park rather than exit. A compose service that exits immediately shows as
    // `Exited` in `podman compose ps`, which makes `just up` claim success over
    // a stack that is half down — a confusing thing to hand a newcomer. Once
    // Phase 4 puts a real server here this is replaced by the accept loop.
    std::thread::park();
}
