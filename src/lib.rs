//! sdirstat library surface. The raw io_uring + statx primitives are Linux/x86_64 only; on other
//! platforms the binary uses the portable std backend (see src/main.rs `read_meta`).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub mod uring;
