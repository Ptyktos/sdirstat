//! sdirstat library surface. `hash` is the primitive core (the HashTrinity + the B/U byte
//! accumulator, ported from sanguine/song) that the emitters route their state through. The raw
//! io_uring + statx primitives are Linux/x86_64 only; on other platforms the binary uses the
//! portable std backend (see src/main.rs `read_meta`).
pub mod hash;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod uring;
