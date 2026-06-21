//! iouring_scan — a zero-dep io_uring batched-statx directory scanner (deep queue depth).
//!
//! The repeated action a normal walk pays is N separate `statx` syscalls — one kernel round-trip
//! per entry. This merges them: write a batch of `IORING_OP_STATX` SQEs into one ring, fire ONE
//! `io_uring_enter`, reap the completions. Warm that cuts syscall mode-switches; cold the deep queue
//! depth keeps many random metadata reads in flight at once, saturating the SSD (the lever to
//! approach its read ceiling — `statx` at QD-1 is latency-bound, at QD-256 it's bandwidth-bound).
//!
//! Pure std + raw `syscall` (x86_64). The metal touched at exactly one place (Axiom XI).
//!
//! usage: iouring_scan [<dir>] [--qd N]   (no dir → run the ring self-test only)

#![allow(clippy::missing_safety_doc)]

use std::time::Instant;

// ─────────────────────────── raw syscalls (x86_64 Linux) ───────────────────────────
#[cfg(target_arch = "x86_64")]
mod sys {
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
    pub const OPENAT: i64 = 257;
    pub const CLOSE: i64 = 3;
    pub const GETDENTS64: i64 = 217;
    pub const MMAP: i64 = 9;
    pub const IO_URING_SETUP: i64 = 425;
    pub const IO_URING_ENTER: i64 = 426;
}
use sys::*;

// ─────────────────────────── constants (kernel ABI) ───────────────────────────
const AT_FDCWD: i64 = -100;
const AT_SYMLINK_NOFOLLOW: u32 = 0x100;
const O_RDONLY: i64 = 0;
const O_DIRECTORY: i64 = 0o200000;
const PROT_READ: i64 = 1;
const PROT_WRITE: i64 = 2;
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
const DT_DIR: u8 = 4;

const STX_SIZE: usize = 40; // u64 (unused for the fold but kept for the self-test)
const STX_BLOCKS: usize = 48; // u64
const STATX_SIZEOF: usize = 256;
const SQE_SIZE: usize = 64;

unsafe fn rd_u16(p: *const u8, off: usize) -> u16 { (p.add(off) as *const u16).read_volatile() }
unsafe fn rd_u32(p: *const u8, off: usize) -> u32 { (p.add(off) as *const u32).read_volatile() }
unsafe fn wr_u32(p: *mut u8, off: usize, v: u32) { (p.add(off) as *mut u32).write_volatile(v) }
unsafe fn rd_u64(p: *const u8, off: usize) -> u64 { (p.add(off) as *const u64).read_volatile() }
unsafe fn fence() { core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst) }

// ─────────────────────────── the ring ───────────────────────────
struct Ring {
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
    unsafe fn setup(qd: u32) -> Ring {
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

        let sq = sc6(MMAP, 0, map_sz as i64, PROT_READ | PROT_WRITE,
                     MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_SQ_RING);
        if sq == MAP_FAILED { panic!("mmap SQ failed"); }
        let cq = if single { sq } else {
            let c = sc6(MMAP, 0, cq_ring_sz as i64, PROT_READ | PROT_WRITE,
                        MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_CQ_RING);
            if c == MAP_FAILED { panic!("mmap CQ failed"); }
            c
        };
        let sqes = sc6(MMAP, 0, sq_entries as i64 * SQE_SIZE as i64, PROT_READ | PROT_WRITE,
                       MAP_SHARED | MAP_POPULATE, fd, IORING_OFF_SQES);
        if sqes == MAP_FAILED { panic!("mmap SQEs failed"); }

        Ring { fd, sq: sq as *mut u8, cq: cq as *mut u8, sqes: sqes as *mut u8,
               o_sq_tail, o_sq_mask, o_sq_array, o_cq_head, o_cq_tail, o_cq_mask, o_cqes }
    }

    /// Write a STATX SQE at index `slot`, then publish it (array[tail&mask]=slot; tail++).
    /// No syscall — the batch is submitted later by `enter`.
    unsafe fn queue_statx(&self, slot: u32, dirfd: i64, path: *const u8, buf: *mut u8, ud: u64) {
        let s = self.sqes.add(slot as usize * SQE_SIZE);
        std::ptr::write_bytes(s, 0, SQE_SIZE);
        *s = IORING_OP_STATX; // opcode @0
        *(s.add(4) as *mut i32) = dirfd as i32; // fd @4
        *(s.add(8) as *mut u64) = buf as u64; // addr2 @8 -> statx buffer
        *(s.add(16) as *mut u64) = path as u64; // addr @16 -> path
        *(s.add(24) as *mut u32) = STATX_BASIC_STATS; // len @24 -> mask
        *(s.add(28) as *mut u32) = AT_SYMLINK_NOFOLLOW; // op_flags @28 -> AT flags
        *(s.add(32) as *mut u64) = ud; // user_data @32
        // publish into the SQ ring
        let mask = rd_u32(self.sq, self.o_sq_mask);
        let tail = rd_u32(self.sq, self.o_sq_tail);
        wr_u32(self.sq, self.o_sq_array + (tail & mask) as usize * 4, slot);
        fence();
        wr_u32(self.sq, self.o_sq_tail, tail.wrapping_add(1));
        fence();
    }

    /// One `io_uring_enter`: submit `submit` queued SQEs, wait for `wait` completions.
    unsafe fn enter(&self, submit: u32, wait: u32) -> i64 {
        sc6(IO_URING_ENTER, self.fd, submit as i64, wait as i64, IORING_ENTER_GETEVENTS, 0, 0)
    }

    /// Drain all available CQEs; `f(user_data, res)` per completion. Returns count reaped.
    unsafe fn reap(&self, mut f: impl FnMut(u64, i32)) -> u32 {
        let mask = rd_u32(self.cq, self.o_cq_mask);
        let mut head = rd_u32(self.cq, self.o_cq_head);
        let tail = rd_u32(self.cq, self.o_cq_tail);
        fence();
        let mut got = 0;
        while head != tail {
            let cqe = self.cq.add(self.o_cqes + (head & mask) as usize * 16);
            f(rd_u64(cqe, 0), rd_u32(cqe, 8) as i32);
            head = head.wrapping_add(1);
            got += 1;
        }
        wr_u32(self.cq, self.o_cq_head, head);
        fence();
        got
    }
}

// ─────────────────────────── the scanner ───────────────────────────
struct Slot {
    statx: [u8; STATX_SIZEOF],
    path: [u8; 4096],
    is_dir: bool,
    dirpath_idx: usize,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = String::new();
    let mut qd: u32 = 256;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--qd" => qd = it.next().and_then(|s| s.parse().ok()).unwrap_or(256),
            s if !s.starts_with('-') => root = s.to_string(),
            _ => {}
        }
    }

    // ── milestone 1: ring self-test — statx /usr/bin/env via io_uring, compare to std ──
    unsafe {
        let ring = Ring::setup(8);
        let mut buf = [0u8; STATX_SIZEOF];
        let path = b"/usr/bin/env\0";
        ring.queue_statx(0, AT_FDCWD, path.as_ptr(), buf.as_mut_ptr(), 0);
        if ring.enter(1, 1) < 0 { panic!("io_uring_enter failed"); }
        let mut res = -1i32;
        ring.reap(|_ud, rr| res = rr);
        let sz = rd_u64(buf.as_ptr(), STX_SIZE);
        let std_sz = std::fs::symlink_metadata("/usr/bin/env").map(|m| m.len()).unwrap_or(0);
        println!("[self-test] io_uring statx /usr/bin/env: res={res} size={sz} (std={std_sz})  {}",
                 if res == 0 && sz == std_sz { "✓ MATCH" } else { "✗ MISMATCH" });
        if root.is_empty() {
            println!("(no dir given — self-test only)");
            return;
        }
    }

    // ── milestone 2: the deep-QD walk ──
    let t0 = Instant::now();
    let (total, n, peak) = unsafe { scan(&root, qd) };
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    println!("io_uring scan {root}: {n} entries · {} · {:.0} ms · peak in-flight {peak} · {:.2} M entries/s",
             human(total), ms, n as f64 / (ms / 1e3) / 1e6);
}

unsafe fn scan(root: &str, qd: u32) -> (u64, u64, u32) {
    let ring = Ring::setup(qd);
    let mut slots: Vec<Slot> = (0..qd).map(|_| Slot {
        statx: [0; STATX_SIZEOF], path: [0; 4096], is_dir: false, dirpath_idx: 0,
    }).collect();
    let mut free: Vec<u32> = (0..qd).rev().collect();
    let mut inflight: u32 = 0;
    let mut to_submit: u32 = 0;
    let mut peak: u32 = 0;
    let mut total: u64 = 0;
    let mut n_entries: u64 = 0;

    let mut dirpaths: Vec<Vec<u8>> = vec![root.as_bytes().to_vec()];
    let mut dir_queue: Vec<usize> = vec![0];

    macro_rules! drain {
        () => {{
            ring.reap(|ud, res| {
                let slot = ud as usize;
                if res == 0 {
                    total += rd_u64(slots[slot].statx.as_ptr(), STX_BLOCKS) * 512;
                    if slots[slot].is_dir { dir_queue.push(slots[slot].dirpath_idx); }
                }
                free.push(slot as u32);
                inflight -= 1;
            });
        }};
    }

    loop {
        // submit any pending batch + reap completions (which enqueue discovered subdirs)
        if to_submit > 0 { ring.enter(to_submit, 0); to_submit = 0; }
        drain!();
        let dpi = match dir_queue.pop() {
            Some(d) => d,
            None if inflight == 0 => break,             // no dirs AND nothing in flight → done
            None => { ring.enter(0, 1); drain!(); continue; } // wait for in-flight to enqueue dirs
        };
        let dp = dirpaths[dpi].clone();
        let mut cpath = dp.clone();
        cpath.push(0);
        let dfd = sc6(OPENAT, AT_FDCWD, cpath.as_ptr() as i64, O_RDONLY | O_DIRECTORY, 0, 0, 0);
        if dfd < 0 { continue; }
        let mut dbuf = vec![0u8; 1 << 16];
        loop {
            let nread = sc3(GETDENTS64, dfd, dbuf.as_mut_ptr() as i64, dbuf.len() as i64);
            if nread <= 0 { break; }
            let mut off = 0usize;
            while off < nread as usize {
                let rec = dbuf.as_ptr().add(off);
                let reclen = rd_u16(rec, 16) as usize;
                let d_type = *rec.add(18);
                let name = rec.add(19);
                let nlen = cstr_len(name);
                off += reclen;
                if (nlen == 1 && *name == b'.')
                    || (nlen == 2 && *name == b'.' && *name.add(1) == b'.') { continue; }
                n_entries += 1;

                // get a free slot — submit the pending batch + wait for completions if starved
                while free.is_empty() {
                    let s = to_submit;
                    to_submit = 0;
                    ring.enter(s, 1);
                    drain!();
                }
                let slot = free.pop().unwrap();
                let mut full = dp.clone();
                if full.last() != Some(&b'/') { full.push(b'/'); }
                full.extend_from_slice(std::slice::from_raw_parts(name, nlen));
                let s = &mut slots[slot as usize];
                let pl = full.len().min(4095);
                s.path[..pl].copy_from_slice(&full[..pl]);
                s.path[pl] = 0;
                s.is_dir = d_type == DT_DIR;
                if s.is_dir { dirpaths.push(full); s.dirpath_idx = dirpaths.len() - 1; }
                let pp = s.path.as_ptr();
                let bp = s.statx.as_mut_ptr();
                ring.queue_statx(slot, AT_FDCWD, pp, bp, slot as u64);
                to_submit += 1;
                inflight += 1;
                peak = peak.max(inflight);
                if to_submit >= 64 { let s = to_submit; to_submit = 0; ring.enter(s, 0); drain!(); }
            }
        }
        sc3(CLOSE, dfd, 0, 0);
    }
    (total, n_entries, peak)
}

unsafe fn cstr_len(p: *const u8) -> usize { let mut n = 0; while *p.add(n) != 0 { n += 1; } n }
fn human(b: u64) -> String {
    const U: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let (mut v, mut i) = (b as f64, 0usize);
    while v >= 1024.0 && i < 5 { v /= 1024.0; i += 1; }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}
