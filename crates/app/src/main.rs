//! Binary entry point for `alertthread`.
//!
//! Deliberately thin: wiring and signal handling only. Everything with a
//! decision in it lives in the library, which is what makes excluding this file
//! from the coverage gate honest rather than convenient. See the coverage
//! policy in `ROADMAP.md`.

fn main() {
    println!("{}", alertthread::build_identity());
    println!("Phase 0 scaffolding: the relay itself is not implemented yet.");
}
