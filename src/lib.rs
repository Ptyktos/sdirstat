//! sdirstat library surface — currently the raw io_uring + directory syscall primitives,
//! shared by the binary's integrated scanner and the standalone `iouring_scan` bin.
pub mod uring;
