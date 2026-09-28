//! The trial proxy binary.
//!
//! No database. Its own deployment. It executes a fallback chain using only the
//! caller's credentials and never MM-owned provider keys. Package 2d fills this in.

fn main() {
    // Skeleton entry point. Package 2d wires the listener, the recommend client, and
    // the chain executor.
}
