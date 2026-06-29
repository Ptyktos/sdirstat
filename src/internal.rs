//! Internal scan/fold/emit core — moved out of the binary so the library can expose a real API
//! (`crate::scan`, `crate::Tree`, `crate::Config`). NOT a stable API; std-only, zero dependencies.
//! This is a verbatim lift of the previous in-binary core: same algorithms, same complexity.
#![allow(clippy::all)]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::ffi::OsStr;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::os::unix::ffi::OsStrExt;
use crate::hash;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use crate::uring;

pub struct Node {
    pub parent: usize,
    pub name: String,
    pub own: u64, // chosen metric (allocated default), folded into `sub`
    pub sub: u64,
    pub size: u64, // st_size (apparent) — the cache 'size' field per spec
    pub blocks: u64, // st_blocks — for the sparse 'blocks:' field
    pub mtime: i64,
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub nlink: u64,
    pub is_dir: bool,
    pub is_link: bool,
}

impl Node {
    pub fn empty() -> Node {
        Node { parent: 0, name: String::new(), own: 0, sub: 0, size: 0, blocks: 0,
               mtime: 0, uid: 0, gid: 0, mode: 0, nlink: 1, is_dir: false, is_link: false }
    }
}

#[derive(Clone, Copy)]
pub struct Cfg {
    pub max_depth: u32,
    pub top: usize,
    pub threads: usize, // 0 = auto (available_parallelism)
    pub apparent: bool, // false = allocated (st_blocks×512, like du/baobab); true = st_size
    pub no_stat: bool,  // skip the per-entry stat — to isolate readdir+userspace cost (benchmarking only)
    pub iouring: bool,  // use the multi-threaded io_uring batched-statx backend
    pub max_entries: usize, // OOM guard: stop descending past this many entries (0/flag → usize::MAX = unlimited)
    pub one_fs: bool,   // --one-file-system: don't cross mount boundaries (du -x) — skips /proc,/sys,/mnt on /
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const IOURING_QD: u32 = 256; // per-thread io_uring queue depth (in-flight statx)

// Default entry ceiling — an OOM guard, not a silent truncation: when the walk hits it, it stops
// descending and `main` warns LOUDLY that the scan is incomplete (a whole `/` is ~8–12M entries, so
// the old 8M default silently truncated it). ~120 B/node, so ~4 GB of nodes here; raise with
// `--max-entries N` (0 = unlimited) when scanning a bigger tree on a box with the RAM for it.
pub const DEFAULT_MAX_ENTRIES: usize = 32_000_000;

pub struct Rec {
    id: usize,
    parent: usize,
    name: String,
    own: u64,
    size: u64,
    blocks: u64,
    mtime: i64,
    uid: u32,
    gid: u32,
    mode: u32,
    nlink: u64,
    is_dir: bool,
    is_link: bool,
}

/// Cross-platform per-entry metadata. Unix reads the full stat surface (blocks/uid/gid/perm/links);
/// other platforms fall back to `std` (size + mtime only — allocated ≈ apparent, no hardlink dedup).
pub struct FileMeta {
    own: u64,
    size: u64,
    blocks: u64,
    mtime: i64,
    uid: u32,
    gid: u32,
    mode: u32,
    nlink: u64,
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
pub fn read_meta(m: &std::fs::Metadata, apparent: bool) -> FileMeta {
    use std::os::unix::fs::MetadataExt;
    FileMeta {
        own: if apparent { m.len() } else { m.blocks() * 512 },
        size: m.len(), blocks: m.blocks(), mtime: m.mtime(),
        uid: m.uid(), gid: m.gid(), mode: m.mode(), nlink: m.nlink(), dev: m.dev(), ino: m.ino(),
    }
}
#[cfg(not(unix))]
pub fn read_meta(m: &std::fs::Metadata, _apparent: bool) -> FileMeta {
    let size = m.len();
    let mtime = m.modified().ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    FileMeta {
        own: size, size, blocks: (size + 511) / 512, mtime, uid: 0, gid: 0,
        mode: if m.is_dir() { 0o040000 } else { 0o100000 }, nlink: 1, dev: 0, ino: 0,
    }
}

/// The device id of a path (`st_dev`) — the mount-boundary key for `--one-file-system`. On non-unix
/// there is no `st_dev`, so it is 0 and `--one-file-system` is a no-op (everything reads as same-fs).
#[cfg(unix)]
pub fn path_dev(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).map(|m| m.dev()).unwrap_or(0)
}
#[cfg(not(unix))]
pub fn path_dev(_p: &Path) -> u64 {
    0
}

/// Fill a root Node's own stat from the path (du counts the top dir itself). Cross-platform.
pub fn fill_root(node: &mut Node, root: &Path, apparent: bool) {
    if let Ok(m) = std::fs::symlink_metadata(root) {
        let fm = read_meta(&m, apparent);
        node.own = fm.own;
        node.sub = fm.own;
        node.size = fm.size;
        node.blocks = fm.blocks;
        node.mtime = fm.mtime;
        node.uid = fm.uid;
        node.gid = fm.gid;
        node.mode = fm.mode;
        node.nlink = fm.nlink;
    }
}

/// The parallel Web walk — `pmapAt` over the directory web, schedule-free (`Web.preduce_schedule_free`).
/// Each worker owns a `VecDeque` frontier (push/pop its own back end, DFS) and only reaches into
/// another worker's deque when its own runs dry — work-stealing, so the single global work-stack lock
/// (the coupling every thread hit twice per dir) is gone: N rarely-contended locks, not 1 always-
/// contended one. Node ids are handed out by one atomic counter, and a dir's id is always allocated
/// *before* it is processed, so parent_id < child_id still holds — the size fold stays one reverse
/// pass, independent of which thread saw what when. Termination rides `active` (count of dirs queued-
/// or-in-flight): it reaches 0 only once every pushed dir has been processed, so no work can strand.
pub fn parallel_scan(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    use std::collections::VecDeque;
    let counter = AtomicUsize::new(1); // 0 = root
    let active = AtomicUsize::new(1);
    let root_dev = path_dev(root); // --one-file-system: descend only into children on this device
    // hardlink dedup: a multiply-linked inode's blocks are counted once (like du/qdirstat).
    // Only touched for nlink>1 entries, so the lock is cold for the overwhelming majority.
    let seen: Mutex<HashSet<(u64, u64)>> = Mutex::new(HashSet::new());
    let nthreads = if cfg.threads > 0 {
        cfg.threads
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8)
    };
    // per-thread frontiers (the sharded work-stack) + per-thread output buckets
    let deques: Vec<Mutex<VecDeque<(PathBuf, usize, u32)>>> =
        (0..nthreads).map(|_| Mutex::new(VecDeque::new())).collect();
    deques[0].lock().unwrap().push_back((root.to_path_buf(), 0, 1)); // seed the root on worker 0
    let recs: Vec<Mutex<Vec<Rec>>> = (0..nthreads).map(|_| Mutex::new(Vec::new())).collect();

    std::thread::scope(|sc| {
        for t in 0..nthreads {
            let (deques, active, counter, recs, cfg, seen) =
                (&deques, &active, &counter, &recs, &cfg, &seen);
            sc.spawn(move || {
                let mut local: Vec<Rec> = Vec::new();
                loop {
                    // get a dir: own frontier (DFS, back), else steal one shallow item (front) from a
                    // victim. Nothing anywhere + drained ⇒ exit; nothing yet ⇒ yield and retry.
                    let item = {
                        let mut got = deques[t].lock().unwrap().pop_back();
                        if got.is_none() {
                            for off in 1..nthreads {
                                let v = (t + off) % nthreads;
                                if let Some(x) = deques[v].lock().unwrap().pop_front() {
                                    got = Some(x);
                                    break;
                                }
                            }
                        }
                        got
                    };
                    let Some((dir, pid, depth)) = item else {
                        if active.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        std::thread::yield_now();
                        continue;
                    };
                    let mut childdirs: Vec<(PathBuf, usize, u32)> = Vec::new();
                    if depth < cfg.max_depth && counter.load(Ordering::Relaxed) < cfg.max_entries {
                        if let Ok(rd) = std::fs::read_dir(&dir) {
                            for ent in rd.flatten() {
                                let Ok(ft) = ent.file_type() else { continue };
                                let is_dir = ft.is_dir();
                                let is_link = ft.is_symlink();
                                let id = counter.fetch_add(1, Ordering::Relaxed);
                                let name = ent.file_name().to_string_lossy().into_owned();
                                if cfg.no_stat {
                                    // benchmark probe: readdir + userspace only, no stat syscall
                                    local.push(Rec { id, parent: pid, name, own: 0, size: 0, blocks: 0,
                                        mtime: 0, uid: 0, gid: 0, mode: 0, nlink: 1, is_dir, is_link });
                                    if is_dir && !is_link { childdirs.push((ent.path(), id, depth + 1)); }
                                    continue;
                                }
                                // metadata() = lstat (no symlink follow): the entry's own stat.
                                let mut descend = is_dir && !is_link;
                                let rec = match ent.metadata() {
                                    Ok(m) => {
                                        // allocated (st_blocks×512, like du/baobab) by default, st_size with
                                        // --apparent; a hardlinked inode's blocks counted once (unix only).
                                        let fm = read_meta(&m, cfg.apparent);
                                        // --one-file-system: a child on a different device is a mount point —
                                        // count its own dir entry but don't cross into it (du -x). This is what
                                        // keeps a `/` scan out of /proc, /sys, /dev and off the other disks.
                                        if cfg.one_fs && fm.dev != root_dev {
                                            descend = false;
                                        }
                                        let mut own = fm.own;
                                        if !is_dir && fm.nlink > 1
                                            && !seen.lock().unwrap().insert((fm.dev, fm.ino))
                                        {
                                            own = 0;
                                        }
                                        Rec { id, parent: pid, name, own, size: fm.size, blocks: fm.blocks,
                                              mtime: fm.mtime, uid: fm.uid, gid: fm.gid, mode: fm.mode,
                                              nlink: fm.nlink, is_dir, is_link }
                                    }
                                    Err(_) => Rec { id, parent: pid, name, own: 0, size: 0, blocks: 0,
                                              mtime: 0, uid: 0, gid: 0, mode: 0, nlink: 1, is_dir, is_link },
                                };
                                local.push(rec);
                                if descend {
                                    childdirs.push((ent.path(), id, depth + 1));
                                }
                            }
                        }
                    }
                    let n = childdirs.len();
                    {
                        let mut d = deques[t].lock().unwrap();
                        for c in childdirs {
                            d.push_back(c); // children land on MY frontier (worked DFS, or stolen)
                        }
                    }
                    active.fetch_add(n, Ordering::SeqCst); // children queued
                    active.fetch_sub(1, Ordering::SeqCst); // this dir done — no notify; idle workers steal/spin
                }
                *recs[t].lock().unwrap() = local;
            });
        }
    });

    // assemble the dense arena (ids are 0..n, each written exactly once)
    assemble(counter.load(Ordering::Relaxed), &recs, root, root_name, cfg)
}

/// Place the per-thread Recs into the dense arena by id; stat the root dir itself (du counts it).
pub fn assemble(n: usize, recs: &[Mutex<Vec<Rec>>], root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::with_capacity(n);
    nodes.resize_with(n, Node::empty);
    let mut root_node = Node::empty();
    root_node.name = root_name;
    root_node.is_dir = true;
    fill_root(&mut root_node, root, cfg.apparent);
    nodes[0] = root_node;
    for r in recs {
        for rec in r.lock().unwrap().drain(..) {
            nodes[rec.id] = Node {
                parent: rec.parent, name: rec.name, own: rec.own, sub: rec.own,
                size: rec.size, blocks: rec.blocks, mtime: rec.mtime, uid: rec.uid,
                gid: rec.gid, mode: rec.mode, nlink: rec.nlink, is_dir: rec.is_dir, is_link: rec.is_link,
            };
        }
    }
    nodes
}

/// The io_uring backend: same parallel Web walk + arena as `parallel_scan`, but each worker drives
/// its own io_uring ring, batching **dirfd-relative `statx`** at depth `IOURING_QD`. The repeated
/// per-entry syscall is merged into batched submissions; aggregate in-flight = threads × QD, the deep
/// queue depth that saturates an SSD's random-read parallelism cold. Captures the full stat surface.
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub fn parallel_scan_iouring(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    use std::collections::VecDeque;
    let counter = AtomicUsize::new(1);
    let active = AtomicUsize::new(1);
    let root_dev = path_dev(root); // --one-file-system: descend only into children on this device
    let seen: Mutex<HashSet<(u64, u64)>> = Mutex::new(HashSet::new());
    let nthreads = if cfg.threads > 0 {
        cfg.threads
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8)
    };
    // per-thread frontiers + work-stealing (same as the std backend — no global stack/Condvar)
    let deques: Vec<Mutex<VecDeque<(PathBuf, usize, u32)>>> =
        (0..nthreads).map(|_| Mutex::new(VecDeque::new())).collect();
    deques[0].lock().unwrap().push_back((root.to_path_buf(), 0, 1));
    let recs: Vec<Mutex<Vec<Rec>>> = (0..nthreads).map(|_| Mutex::new(Vec::new())).collect();

    std::thread::scope(|sc| {
        for t in 0..nthreads {
            let (deques, active, counter, recs, cfg, seen) =
                (&deques, &active, &counter, &recs, &cfg, &seen);
            sc.spawn(move || unsafe {
                let ring = uring::Ring::setup(IOURING_QD);
                let qd = IOURING_QD as usize;
                let mut sxbuf: Vec<[u8; uring::STATX_SIZEOF]> = vec![[0; uring::STATX_SIZEOF]; qd];
                let mut slot_entry = vec![0usize; qd];
                let mut free: Vec<u32> = (0..IOURING_QD).rev().collect();
                let mut dbuf = vec![0u8; 1 << 16];
                let mut comp: Vec<(u32, i32)> = Vec::new();
                let mut local: Vec<Rec> = Vec::new();

                loop {
                    let item = {
                        let mut got = deques[t].lock().unwrap().pop_back();
                        if got.is_none() {
                            for off in 1..nthreads {
                                let v = (t + off) % nthreads;
                                if let Some(x) = deques[v].lock().unwrap().pop_front() {
                                    got = Some(x);
                                    break;
                                }
                            }
                        }
                        got
                    };
                    let Some((dir, pid, depth)) = item else {
                        if active.load(Ordering::SeqCst) == 0 {
                            break;
                        }
                        std::thread::yield_now();
                        continue;
                    };
                    let mut childdirs: Vec<(PathBuf, usize, u32)> = Vec::new();

                    if depth < cfg.max_depth && counter.load(Ordering::Relaxed) < cfg.max_entries {
                        let mut cpath = dir.as_os_str().as_bytes().to_vec();
                        cpath.push(0);
                        let dfd = uring::open_dir(&cpath);
                        if dfd >= 0 {
                            let entries = uring::read_dir(dfd, &mut dbuf);
                            let k = entries.len();
                            let base = counter.fetch_add(k, Ordering::Relaxed);
                            let mut cnames: Vec<Vec<u8>> = Vec::with_capacity(k);
                            let mut rl: Vec<Rec> = Vec::with_capacity(k);
                            for (i, (name, _dt)) in entries.iter().enumerate() {
                                let mut c = name.clone();
                                c.push(0);
                                cnames.push(c);
                                rl.push(Rec {
                                    id: base + i, parent: pid, name: String::from_utf8_lossy(name).into_owned(),
                                    own: 0, size: 0, blocks: 0, mtime: 0, uid: 0, gid: 0, mode: 0,
                                    nlink: 1, is_dir: false, is_link: false,
                                });
                            }
                            // batch dirfd-relative statx at deep QD
                            let (mut next, mut done, mut to_submit) = (0usize, 0usize, 0u32);
                            while done < k {
                                while next < k && !free.is_empty() {
                                    let slot = free.pop().unwrap();
                                    slot_entry[slot as usize] = next;
                                    ring.queue_statx(slot, dfd, cnames[next].as_ptr(),
                                                     sxbuf[slot as usize].as_mut_ptr(), slot as u64);
                                    to_submit += 1;
                                    next += 1;
                                    if to_submit >= 128 { ring.enter(to_submit, 0); to_submit = 0; }
                                }
                                ring.enter(to_submit, 1);
                                to_submit = 0;
                                comp.clear();
                                ring.reap(|ud, res| comp.push((ud as u32, res)));
                                for &(slot, res) in &comp {
                                    let i = slot_entry[slot as usize];
                                    if res == 0 {
                                        let st = uring::decode_statx(sxbuf[slot as usize].as_ptr());
                                        let mut own = if cfg.apparent { st.size } else { st.blocks * 512 };
                                        if !st.is_dir && st.nlink > 1
                                            && !seen.lock().unwrap().insert((st.dev, st.ino)) { own = 0; }
                                        let r = &mut rl[i];
                                        r.own = own; r.size = st.size; r.blocks = st.blocks;
                                        r.mtime = st.mtime; r.uid = st.uid; r.gid = st.gid;
                                        r.mode = st.mode; r.nlink = st.nlink;
                                        r.is_dir = st.is_dir; r.is_link = st.is_link;
                                        if st.is_dir && !st.is_link && depth + 1 < cfg.max_depth
                                            && (!cfg.one_fs || st.dev == root_dev)
                                        {
                                            let mut cp = dir.clone();
                                            cp.push(OsStr::from_bytes(&entries[i].0));
                                            childdirs.push((cp, base + i, depth + 1));
                                        }
                                    }
                                    free.push(slot);
                                    done += 1;
                                }
                            }
                            uring::close_fd(dfd);
                            local.extend(rl);
                        }
                    }
                    let nc = childdirs.len();
                    {
                        let mut d = deques[t].lock().unwrap();
                        for c in childdirs { d.push_back(c); }
                    }
                    active.fetch_add(nc, Ordering::SeqCst);
                    active.fetch_sub(1, Ordering::SeqCst);
                }
                *recs[t].lock().unwrap() = local;
            });
        }
    });
    assemble(counter.load(Ordering::Relaxed), &recs, root, root_name, cfg)
}

pub fn human(b: u64) -> String {
    const U: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let (mut v, mut i) = (b as f64, 0usize);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

pub fn json_escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
}

/// Emit the pruned nested tree as JSON: {"n":name,"v":bytes,"d":1|0,"c":[...]}, folded into the
/// `hash::Acc` byte accumulator (the B/U append merge) — no `format!`, no per-node escape String.
/// Per directory keep the `top` largest children; bucket the remainder into one "… (k more)"
/// leaf so totals stay exact and the tree stays explorable-but-bounded.
pub fn emit_json(i: usize, kids: &hash::Csr, nodes: &[Node], cfg: &Cfg, acc: &mut hash::Acc) {
    acc.bytes(b"{\"n\":\"").esc_json(&nodes[i].name)
        .bytes(b"\",\"v\":").u64(nodes[i].sub)
        .bytes(b",\"d\":").byte(if nodes[i].is_dir { b'1' } else { b'0' });
    let cs = kids.of(i);
    if !cs.is_empty() {
        let mut ks: Vec<usize> = cs.to_vec();
        ks.sort_by(|&a, &b| nodes[b].sub.cmp(&nodes[a].sub));
        acc.bytes(b",\"c\":[");
        let keep = ks.len().min(cfg.top);
        ks.iter().take(keep).enumerate().for_each(|(j, &c)| {
            if j > 0 { acc.byte(b','); }
            emit_json(c, kids, nodes, cfg, acc);
        });
        if ks.len() > keep {
            let rest: u64 = ks.iter().skip(keep).map(|&c| nodes[c].sub).sum();
            let n = ks.len() - keep;
            if keep > 0 { acc.byte(b','); }
            acc.str("{\"n\":\"… (").u64(n as u64).str(" more)\",\"v\":").u64(rest).str(",\"d\":0}");
        }
        acc.byte(b']');
    }
    acc.byte(b'}');
}

/// per-extension size + count over the whole scan (QDirStat's File Type Statistics), folded into the
/// accumulator as a JSON array.
pub fn type_stats_into(nodes: &[Node], acc: &mut hash::Acc) {
    let mut types: HashMap<&str, (u64, u64)> = HashMap::new();
    for nd in nodes {
        if nd.is_dir || nd.is_link {
            continue;
        }
        // extension = text after the last interior dot; else "(no ext)"
        let ext = match nd.name.rfind('.') {
            Some(p) if p > 0 && p + 1 < nd.name.len() => &nd.name[p + 1..],
            _ => "(no ext)",
        };
        let e = types.entry(ext).or_insert((0, 0));
        e.0 += nd.sub;
        e.1 += 1;
    }
    let mut tv: Vec<(&str, (u64, u64))> = types.into_iter().collect();
    tv.sort_by(|a, b| b.1 .0.cmp(&a.1 .0));
    acc.byte(b'[');
    tv.iter().take(60).enumerate().for_each(|(i, (ext, (sz, cnt)))| {
        if i > 0 { acc.byte(b','); }
        acc.bytes(b"{\"e\":\"").esc_json(ext).bytes(b"\",\"v\":").u64(*sz).bytes(b",\"c\":").u64(*cnt).byte(b'}');
    });
    acc.byte(b']');
}

/// Dispatch to the chosen scan backend (std fstatat vs io_uring batched statx), same `Vec<Node>`.
/// The io_uring backend is Linux/x86_64 only; everywhere else `--iouring` is a no-op (std backend).
pub fn scan_backend(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    let _ = cfg.iouring;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if cfg.iouring {
        return parallel_scan_iouring(root, root_name, cfg);
    }
    parallel_scan(root, root_name, cfg)
}

/// Fold a scanned arena → the /scan response body `{scan_ms, entries, tree, types}`, plus (total, n).
pub fn finish(mut nodes: Vec<Node>, scan_ms: f64, cfg: &Cfg) -> (String, u64, usize) {
    for i in (1..nodes.len()).rev() {
        let s = nodes[i].sub;
        let p = nodes[i].parent;
        nodes[p].sub += s;
    }
    let total = nodes.first().map(|n| n.sub).unwrap_or(0);
    let n = nodes.len();
    let kids = hash::Csr::from_parents(n, |i| nodes[i].parent);
    let mut acc = hash::Acc::with_capacity(n.saturating_mul(64).max(1 << 16));
    acc.str("{\"scan_ms\":").u64(scan_ms.round() as u64).str(",\"entries\":").u64(n as u64).str(",\"tree\":");
    emit_json(0, &kids, &nodes, cfg, &mut acc);
    acc.str(",\"types\":");
    type_stats_into(&nodes, &mut acc);
    acc.byte(b'}');
    (acc.into_string(), total, n)
}

/// QDirStat cache format V2.0 emit — full fidelity (size = st_size, uid, gid, octal perm, hex mtime,
/// plus the optional `blocks:` for sparse files and `links:` for hardlinks). Dirs get an absolute
/// path; their files follow as base names (pre-order grouping, the relative-name space saver). Type
/// is D / L (symlink) / F (everything else).
///
/// This is the emit face routed through the primitive core (`hash::Acc`): one fold over the
/// directory web into one byte accumulator (the B/U append merge), then one `write_all` — the
/// single cut to the metal. No per-node `format!`/`writeln!` (the old zoo, the 1.1 s hot spot);
/// integers go in via the radix unfold, the tree walk is the catamorphism `cache_dir`.
pub fn emit_cache(out: &str, kids: &hash::Csr, nodes: &[Node]) {
    // Cache files run to hundreds of MB on a real tree; size the accumulator generously up front
    // so the fold never re-allocates (one buffer, the append monoid).
    let mut acc = hash::Acc::with_capacity(nodes.len().saturating_mul(96).max(1 << 16));
    acc.str("[qdirstat 2.0 cache file]\n# Generated by sdirstat\n# Do not edit!\n#\n");
    cache_dir(&mut acc, 0, &nodes[0].name.clone(), kids, nodes);
    std::fs::write(out, acc.as_slice()).expect("write cache");
}

/// The mandatory V2.0 tail after type+path: size, uid, gid, perm, mtime (+ optional blocks/links),
/// folded straight into the accumulator (was `format!("\t{}\t{}\t{}\t0{:o}\t0x{:x}", …)`).
pub fn cache_tail(acc: &mut hash::Acc, n: &Node) {
    acc.byte(b'\t').u64(n.size)
        .byte(b'\t').u64(n.uid as u64)
        .byte(b'\t').u64(n.gid as u64)
        .bytes(b"\t0").oct((n.mode & 0o7777) as u64) // the leading '0' the old `0{:o}` printed
        .bytes(b"\t0x").hex(n.mtime as u64);
    if !n.is_dir && n.blocks > 0 && n.blocks * 512 < n.size {
        acc.bytes(b"\tblocks: ").u64(n.blocks); // sparse file
    }
    if !n.is_dir && n.nlink > 1 {
        acc.bytes(b"\tlinks: ").u64(n.nlink); // hardlinked
    }
}

/// The catamorphism over the directory web — the eliminator, the algebra is the only freedom
/// (`Song/Algebra.lean` `fromTerm`). Each step couples only this node to its direct children (the
/// two `for_each` folds: leaves, then dirs); the tree's height is *reach* (flat), not coupling
/// depth — so this respects the depth-2 rule (`TrinityDepth`: one coupling = depth 2). Pre-order:
/// the dir line, then its non-dir children as base names, then recurse into child dirs.
pub fn cache_dir(acc: &mut hash::Acc, i: usize, path: &str, kids: &hash::Csr, nodes: &[Node]) {
    acc.bytes(b"D ").esc(path);
    cache_tail(acc, &nodes[i]);
    acc.byte(b'\n');
    kids.of(i).iter().filter(|&&c| !nodes[c].is_dir).for_each(|&c| {
        let n = &nodes[c];
        acc.byte(if n.is_link { b'L' } else { b'F' }).byte(b' ').esc(&n.name);
        cache_tail(acc, n);
        acc.byte(b'\n');
    });
    kids.of(i).iter().filter(|&&c| nodes[c].is_dir).for_each(|&c| {
        // child absolute path = parent + '/' + name (the append monoid on the String carrier; was
        // `format!("{}/{}", …)`). Per-dir, not per-entry, so off the hot path.
        let mut sub = String::with_capacity(path.len() + 1 + nodes[c].name.len());
        sub.push_str(path.trim_end_matches('/'));
        sub.push('/');
        sub.push_str(&nodes[c].name);
        cache_dir(acc, c, &sub, kids, nodes);
    });
}
