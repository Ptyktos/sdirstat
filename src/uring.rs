//! Raw io_uring + directory syscalls (x86_64 Linux), zero-dependency. The one cut to the metal
//! (Axiom XI): everything below is `syscall` instructions + the io_uring shared-ring ABI, no libc.
//!
//! The ring batches `IORING_OP_STATX` (dirfd-relative): write a deep batch of SQEs, ONE
//! `io_uring_enter`, reap the completions — N metadata reads in flight at once.

#![allow(clippy::missing_safety_doc)]

#[cfg(target_arch = "x86_64")]
mod sc {
    use core::arch::asm;
    #[inline]
    pub unsafe fn sc6(n: i64, a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) -> i64 {
        let r: i64;
        asm!("syscall",
            inlateout("rax") n => r,
            in("rdi") a, in("rsi") b, in("rdx") c, in("r10") d, in("r8") e, in("r9") f,
            out("rcx") _, out("r11") _, options(nostack, preserves_flags));
        r
    }
    pub unsafe fn sc3(n: i64, a: i64, b: i64, c: i64) -> i64 { sc6(n, a, b, c, 0, 0, 0) }
}
use sc::*;

const OPENAT: i64 = 257;
const CLOSE: i64 = 3;
const GETDENTS64: i64 = 217;
const MMAP: i64 = 9;
const IO_URING_SETUP: i64 = 425;
const IO_URING_ENTER: i64 = 426;

const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const O_RDONLY: i64 = 0;
const O_DIRECTORY: i64 = 0o200000;
const O_NOFOLLOW: i64 = 0o400000;
const PROT_RW: i64 = 3;
const MAP_SHARED: i64 = 1;
const MAP_POPULATE: i64 = 0x8000;
const MAP_FAILED: i64 = -1;

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x0800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;
const IORING_ENTER_GETEVENTS: i64 = 1;
const IORING_FEAT_SINGLE_MMAP: u32 = 1;
const IORING_OP_STATX: u8 = 21;
const STATX_BASIC_STATS: u32 = 0x7ff;
const SQE_SIZE: usize = 64;

pub const STATX_SIZEOF: usize = 256;
pub const DT_DIR: u8 = 4;

// statx output field byte-offsets
const STX_NLINK: usize = 16;
const STX_UID: usize = 20;
const STX_GID: usize = 24;
const STX_MODE: usize = 28;
const STX_INO: usize = 32;
const STX_SIZE: usize = 40;
const STX_BLOCKS: usize = 48;
const STX_MTIME_SEC: usize = 112;
const STX_DEV_MAJOR: usize = 136;
const STX_DEV_MINOR: usize = 140;

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFLNK: u32 = 0o120000;

#[inline] unsafe fn rd_u16(p: *const u8, o: usize) -> u16 { (p.add(o) as *const u16).read_unaligned() }
#[inline] unsafe fn rd_u32(p: *const u8, o: usize) -> u32 { (p.add(o) as *const u32).read_unaligned() }
#[inline] unsafe fn rd_i64(p: *const u8, o: usize) -> i64 { (p.add(o) as *const i64).read_unaligned() }
#[inline] unsafe fn rd_u64(p: *const u8, o: usize) -> u64 { (p.add(o) as *const u64).read_unaligned() }
#[inline] unsafe fn rdv_u32(p: *const u8, o: usize) -> u32 { (p.add(o) as *const u32).read_volatile() }
#[inline] unsafe fn wrv_u32(p: *mut u8, o: usize, v: u32) { (p.add(o) as *mut u32).write_volatile(v) }
#[inline] unsafe fn fence() { core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst) }

// ─────────────────────────── decoded stat (the full surface) ───────────────────────────
#[derive(Clone, Copy, Default)]
pub struct Stat {
    pub size: u64,
    pub blocks: u64,
    pub mtime: i64,
    pub ino: u64,
    pub dev: u64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub nlink: u64,
    pub is_dir: bool,
    pub is_link: bool,
}
pub unsafe fn decode_statx(p: *const u8) -> Stat {
    let mode = rd_u16(p, STX_MODE) as u32;
    let fmt = mode & S_IFMT;
    Stat {
        size: rd_u64(p, STX_SIZE),
        blocks: rd_u64(p, STX_BLOCKS),
        mtime: rd_i64(p, STX_MTIME_SEC),
        ino: rd_u64(p, STX_INO),
        dev: ((rd_u32(p, STX_DEV_MAJOR) as u64) << 32) | rd_u32(p, STX_DEV_MINOR) as u64,
        uid: rd_u32(p, STX_UID),
        gid: rd_u32(p, STX_GID),
        mode,
        nlink: rd_u32(p, STX_NLINK) as u64,
        is_dir: fmt == S_IFDIR,
        is_link: fmt == S_IFLNK,
    }
}

// ─────────────────────────── directory syscalls ───────────────────────────
/// openat a directory (O_DIRECTORY|O_NOFOLLOW) — `path` must be NUL-terminated. <0 on error.
pub unsafe fn open_dir(path: &[u8]) -> i64 {
    sc6(OPENAT, AT_FDCWD, path.as_ptr() as i64, O_RDONLY | O_DIRECTORY | O_NOFOLLOW, 0, 0, 0)
}
pub unsafe fn close_fd(fd: i64) { sc3(CLOSE, fd, 0, 0); }

/// Read all entries of `dfd` via getdents64. Returns (name-without-NUL, d_type), skipping "."/"..".
pub unsafe fn read_dir(dfd: i64, buf: &mut [u8]) -> Vec<(Vec<u8>, u8)> {
    let mut out = Vec::new();
    loop {
        let n = sc3(GETDENTS64, dfd, buf.as_mut_ptr() as i64, buf.len() as i64);
        if n <= 0 { break; }
        let mut off = 0usize;
        while off < n as usize {
            let rec = buf.as_ptr().add(off);
            let reclen = rd_u16(rec, 16) as usize;
            let d_type = *rec.add(18);
            let name = rec.add(19);
            let mut len = 0;
            while *name.add(len) != 0 { len += 1; }
            off += reclen;
            if (len == 1 && *name == b'.') || (len == 2 && *name == b'.' && *name.add(1) == b'.') {
                continue;
            }
            out.push((std::slice::from_raw_parts(name, len).to_vec(), d_type));
        }
    }
    out
}

// ─────────────────────────── the ring ───────────────────────────
pub struct Ring {
    fd: i64,
    sq: *mut u8,
    cq: *mut u8,
    sqes: *mut u8,
    o_sq_tail: usize,
    o_sq_mask: usize,
    o_sq_array: usize,
    o_cq_head: usize,
    o_cq_tail: usize,
    o_cq_mask: usize,
    o_cqes: usize,
}

impl Ring {
    pub unsafe fn setup(qd: u32) -> Ring {
        let mut params = [0u8; 120];
        let fd = sc3(IO_URING_SETUP, qd as i64, params.as_mut_ptr() as i64, 0);
        if fd < 0 { panic!("io_uring_setup failed: {fd}"); }
        let p = params.as_ptr();
        let sq_entries = rd_u32(p, 0);
        let cq_entries = rd_u32(p, 4);
        let features = rd_u32(p, 20);
        let o_sq_array = rd_u32(p, 40 + 24) as usize;
        let o_sq_tail = rd_u32(p, 40 + 4) as usize;
        let o_sq_mask = rd_u32(p, 40 + 8) as usize;
        let o_cq_head = rd_u32(p, 80) as usize;
        let o_cq_tail = rd_u32(p, 80 + 4) as usize;
        let o_cq_mask = rd_u32(p, 80 + 8) as usize;
        let o_cqes = rd_u32(p, 80 + 20) as usize;

        let sq_ring_sz = o_sq_array + sq_entries as usize * 4;
        let cq_ring_sz = o_cqes + cq_entries as usize * 16;
        let single = features & IORING_FEAT_SINGLE_MMAP != 0;
        let map_sz = if single { sq_ring_sz.max(cq_ring_sz) } else { sq_ring_sz };

        let sq = sc6(MMAP, 0, map_sz as i64, PROT_RW, MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_SQ_RING);
        if sq == MAP_FAILED { panic!("mmap SQ failed"); }
        let cq = if single { sq } else {
            let c = sc6(MMAP, 0, cq_ring_sz as i64, PROT_RW, MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_CQ_RING);
            if c == MAP_FAILED { panic!("mmap CQ failed"); }
            c
        };
        let sqes = sc6(MMAP, 0, sq_entries as i64 * SQE_SIZE as i64, PROT_RW, MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_SQES);
        if sqes == MAP_FAILED { panic!("mmap SQEs failed"); }

        Ring { fd, sq: sq as *mut u8, cq: cq as *mut u8, sqes: sqes as *mut u8,
               o_sq_tail, o_sq_mask, o_sq_array, o_cq_head, o_cq_tail, o_cq_mask, o_cqes }
    }

    /// Queue a dirfd-relative STATX SQE at ring slot `slot` (path NUL-terminated). No syscall yet.
    pub unsafe fn queue_statx(&self, slot: u32, dirfd: i64, path: *const u8, buf: *mut u8, ud: u64) {
        let s = self.sqes.add(slot as usize * SQE_SIZE);
        std::ptr::write_bytes(s, 0, SQE_SIZE);
        *s = IORING_OP_STATX;
        *(s.add(4) as *mut i32) = dirfd as i32;
        *(s.add(8) as *mut u64) = buf as u64;
        *(s.add(16) as *mut u64) = path as u64;
        *(s.add(24) as *mut u32) = STATX_BASIC_STATS;
        *(s.add(28) as *mut u32) = AT_SYMLINK_NOFOLLOW;
        *(s.add(32) as *mut u64) = ud;
        let mask = rdv_u32(self.sq, self.o_sq_mask);
        let tail = rdv_u32(self.sq, self.o_sq_tail);
        wrv_u32(self.sq, self.o_sq_array + (tail & mask) as usize * 4, slot);
        fence();
        wrv_u32(self.sq, self.o_sq_tail, tail.wrapping_add(1));
        fence();
    }

    pub unsafe fn enter(&self, submit: u32, wait: u32) -> i64 {
        sc6(IO_URING_ENTER, self.fd, submit as i64, wait as i64, IORING_ENTER_GETEVENTS, 0, 0)
    }

    /// Drain available CQEs; `f(user_data, res)` each. Returns count.
    pub unsafe fn reap(&self, mut f: impl FnMut(u64, i32)) -> u32 {
        let mask = rdv_u32(self.cq, self.o_cq_mask);
        let mut head = rdv_u32(self.cq, self.o_cq_head);
        let tail = rdv_u32(self.cq, self.o_cq_tail);
        fence();
        let mut got = 0;
        while head != tail {
            let cqe = self.cq.add(self.o_cqes + (head & mask) as usize * 16);
            f(rd_u64(cqe, 0), rd_u32(cqe, 8) as i32);
            head = head.wrapping_add(1);
            got += 1;
        }
        wrv_u32(self.cq, self.o_cq_head, head);
        fence();
        got
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        unsafe { close_fd(self.fd); }
    }
}
