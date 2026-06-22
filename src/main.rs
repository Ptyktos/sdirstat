//! sdirstat — a headless filesystem indexer. The directory tree IS a Web (node = own bytes,
//! edge = parent→child); the size fold is the B/U accumulator (`subtree = own + Σ children`),
//! one reverse pass. Output is a `fromTerm` at an α: a self-contained interactive HTML treemap
//! (the explorable web GUI), a nested JSON tree, or the QDirStat cache format. Zero-dependency.
//!
//! usage:
//!   sdirstat <root> [-o out.html]        # self-contained treemap web GUI (default)
//!   sdirstat <root> --json [-o tree.json]
//!   sdirstat <root> --cache [-o out.qdirstat.cache]
//!   flags: --max-depth N (default 40)  --top K (children kept per dir, default 80)

use sdirstat::hash;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use sdirstat::uring;
use std::collections::{HashMap, HashSet};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};

struct Node {
    parent: usize,
    name: String,
    own: u64, // chosen metric (allocated default), folded into `sub`
    sub: u64,
    size: u64, // st_size (apparent) — the cache 'size' field per spec
    blocks: u64, // st_blocks — for the sparse 'blocks:' field
    mtime: i64,
    uid: u32,
    gid: u32,
    mode: u32,
    nlink: u64,
    is_dir: bool,
    is_link: bool,
}

impl Node {
    fn empty() -> Node {
        Node { parent: 0, name: String::new(), own: 0, sub: 0, size: 0, blocks: 0,
               mtime: 0, uid: 0, gid: 0, mode: 0, nlink: 1, is_dir: false, is_link: false }
    }
}

#[derive(Clone, Copy)]
struct Cfg {
    max_depth: u32,
    top: usize,
    threads: usize, // 0 = auto (available_parallelism)
    apparent: bool, // false = allocated (st_blocks×512, like du/baobab); true = st_size
    no_stat: bool,  // skip the per-entry stat — to isolate readdir+userspace cost (benchmarking only)
    iouring: bool,  // use the multi-threaded io_uring batched-statx backend
    max_entries: usize, // OOM guard: stop descending past this many entries (0/flag → usize::MAX = unlimited)
    one_fs: bool,   // --one-file-system: don't cross mount boundaries (du -x) — skips /proc,/sys,/mnt on /
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const IOURING_QD: u32 = 256; // per-thread io_uring queue depth (in-flight statx)

// Default entry ceiling — an OOM guard, not a silent truncation: when the walk hits it, it stops
// descending and `main` warns LOUDLY that the scan is incomplete (a whole `/` is ~8–12M entries, so
// the old 8M default silently truncated it). ~120 B/node, so ~4 GB of nodes here; raise with
// `--max-entries N` (0 = unlimited) when scanning a bigger tree on a box with the RAM for it.
const DEFAULT_MAX_ENTRIES: usize = 32_000_000;

struct Rec {
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
struct FileMeta {
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
fn read_meta(m: &std::fs::Metadata, apparent: bool) -> FileMeta {
    use std::os::unix::fs::MetadataExt;
    FileMeta {
        own: if apparent { m.len() } else { m.blocks() * 512 },
        size: m.len(), blocks: m.blocks(), mtime: m.mtime(),
        uid: m.uid(), gid: m.gid(), mode: m.mode(), nlink: m.nlink(), dev: m.dev(), ino: m.ino(),
    }
}
#[cfg(not(unix))]
fn read_meta(m: &std::fs::Metadata, _apparent: bool) -> FileMeta {
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
fn path_dev(p: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).map(|m| m.dev()).unwrap_or(0)
}
#[cfg(not(unix))]
fn path_dev(_p: &Path) -> u64 {
    0
}

/// Fill a root Node's own stat from the path (du counts the top dir itself). Cross-platform.
fn fill_root(node: &mut Node, root: &Path, apparent: bool) {
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
fn parallel_scan(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
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
fn assemble(n: usize, recs: &[Mutex<Vec<Rec>>], root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
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
fn parallel_scan_iouring(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    let counter = AtomicUsize::new(1);
    let stack: Mutex<Vec<(PathBuf, usize, u32)>> = Mutex::new(vec![(root.to_path_buf(), 0, 1)]);
    let active = AtomicUsize::new(1);
    let cv = Condvar::new();
    let root_dev = path_dev(root); // --one-file-system: descend only into children on this device
    let seen: Mutex<HashSet<(u64, u64)>> = Mutex::new(HashSet::new());
    let nthreads = if cfg.threads > 0 {
        cfg.threads
    } else {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8)
    };
    let recs: Vec<Mutex<Vec<Rec>>> = (0..nthreads).map(|_| Mutex::new(Vec::new())).collect();

    std::thread::scope(|sc| {
        for t in 0..nthreads {
            let (stack, active, cv, counter, recs, cfg, seen) =
                (&stack, &active, &cv, &counter, &recs, &cfg, &seen);
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
                        let mut s = stack.lock().unwrap();
                        loop {
                            if let Some(it) = s.pop() { break Some(it); }
                            if active.load(Ordering::SeqCst) == 0 { break None; }
                            s = cv.wait(s).unwrap();
                        }
                    };
                    let Some((dir, pid, depth)) = item else { break };
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
                    let mut s = stack.lock().unwrap();
                    let nc = childdirs.len();
                    for c in childdirs { s.push(c); }
                    active.fetch_add(nc, Ordering::SeqCst);
                    active.fetch_sub(1, Ordering::SeqCst);
                    cv.notify_all();
                }
                *recs[t].lock().unwrap() = local;
            });
        }
    });
    assemble(counter.load(Ordering::Relaxed), &recs, root, root_name, cfg)
}

fn human(b: u64) -> String {
    const U: [&str; 6] = ["B", "K", "M", "G", "T", "P"];
    let (mut v, mut i) = (b as f64, 0usize);
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

fn json_escape(s: &str, out: &mut String) {
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
fn emit_json(i: usize, kids: &hash::Csr, nodes: &[Node], cfg: &Cfg, acc: &mut hash::Acc) {
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
fn type_stats_into(nodes: &[Node], acc: &mut hash::Acc) {
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
fn scan_backend(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    let _ = cfg.iouring;
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    if cfg.iouring {
        return parallel_scan_iouring(root, root_name, cfg);
    }
    parallel_scan(root, root_name, cfg)
}

/// Fold a scanned arena → the /scan response body `{scan_ms, entries, tree, types}`, plus (total, n).
fn finish(mut nodes: Vec<Node>, scan_ms: f64, cfg: &Cfg) -> (String, u64, usize) {
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

/// Scan a local path → (response JSON, total bytes, entries). Shared by /scan and report-save.
fn scan_response(root: &str, cfg: &Cfg) -> (String, u64, usize) {
    let t0 = std::time::Instant::now();
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| Path::new(root).to_path_buf());
    let nodes = scan_backend(Path::new(root), canon.to_string_lossy().into_owned(), cfg);
    finish(nodes, t0.elapsed().as_secs_f64() * 1e3, cfg)
}

/// One query parameter from `…?a=1&b=2`, URL-decoded.
fn qparam(target: &str, key: &str) -> Option<String> {
    let q = target.splitn(2, '?').nth(1)?;
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key { Some(url_decode(v)) } else { None }
    })
}

// ── sources: mounts (local + network) + rclone remotes (S3/WebDAV/SMB/SFTP/…) ──

fn push_src(out: &mut String, first: &mut bool, path: &str, fstype: &str, kind: &str, label: &str) {
    if !*first { out.push(','); }
    *first = false;
    out.push_str("{\"path\":\"");
    json_escape(path, out);
    out.push_str("\",\"fstype\":\"");
    json_escape(fstype, out);
    out.push_str("\",\"kind\":\"");
    out.push_str(kind);
    out.push_str("\",\"label\":\"");
    json_escape(label, out);
    out.push_str("\"}");
}

/// Scannable sources: quick locations, PVE/Docker storage, and real mounts (local + network).
fn mounts_json() -> String {
    let mut out = String::from("[");
    let mut first = true;
    if let Ok(home) = std::env::var("HOME") {
        push_src(&mut out, &mut first, &home, "", "home", "Home");
    }
    push_src(&mut out, &mut first, "/", "", "local", "Root /");
    for (p, l) in [("/var/lib/docker", "Docker storage"), ("/var/lib/vz", "Proxmox VE storage"), ("/mnt/pve", "Proxmox VE mounts")] {
        if Path::new(p).is_dir() {
            push_src(&mut out, &mut first, p, "", "backup", l);
        }
    }
    if let Ok(m) = std::fs::read_to_string("/proc/mounts") {
        for line in m.lines() {
            let f: Vec<&str> = line.split(' ').collect();
            if f.len() < 3 {
                continue;
            }
            let (mp, fstype) = (f[1], f[2]);
            let kind = match fstype {
                "cifs" | "smb3" | "smbfs" | "nfs" | "nfs4" => "network",
                t if t.starts_with("fuse.") => {
                    if mp.contains("gvfs") { "network" } else { "local" }
                }
                "ext4" | "xfs" | "btrfs" | "vfat" | "exfat" | "ntfs" | "zfs" | "f2fs" => "local",
                _ => continue,
            };
            let mp = mp.replace("\\040", " ");
            push_src(&mut out, &mut first, &mp, fstype, kind, &mp);
        }
    }
    out.push(']');
    out
}

/// `rclone listremotes` → JSON array of configured remote names (e.g. `"s3:"`, `"nas:"`).
fn remotes_json() -> String {
    let mut arr = String::from("[");
    if let Ok(o) = Command::new("rclone").arg("listremotes").output() {
        if o.status.success() {
            let mut first = true;
            for r in String::from_utf8_lossy(&o.stdout).lines().map(str::trim).filter(|l| !l.is_empty()) {
                if !first { arr.push(','); }
                first = false;
                arr.push('"');
                json_escape(r, &mut arr);
                arr.push('"');
            }
        }
    }
    arr.push(']');
    arr
}

/// Build a Node arena from a flat (size, "a/b/file") listing — rclone output / a cache import.
fn build_tree_from_flat(root_name: &str, lines: impl Iterator<Item = (u64, String)>) -> Vec<Node> {
    let mut root = Node::empty();
    root.name = root_name.to_string();
    root.is_dir = true;
    let mut nodes = vec![root];
    let mut dirmap: HashMap<String, usize> = HashMap::new();
    for (size, path) in lines {
        let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if comps.is_empty() {
            continue;
        }
        let mut parent = 0usize;
        let mut acc = String::new();
        for (i, comp) in comps.iter().enumerate() {
            if !acc.is_empty() { acc.push('/'); }
            acc.push_str(comp);
            if i + 1 == comps.len() {
                let mut nd = Node::empty();
                nd.parent = parent;
                nd.name = (*comp).to_string();
                nd.own = size; nd.sub = size; nd.size = size; nd.blocks = (size + 511) / 512;
                nodes.push(nd);
            } else if let Some(&id) = dirmap.get(&acc) {
                parent = id;
            } else {
                let id = nodes.len();
                let mut nd = Node::empty();
                nd.parent = parent;
                nd.name = (*comp).to_string();
                nd.is_dir = true;
                nodes.push(nd);
                dirmap.insert(acc.clone(), id);
                parent = id;
            }
        }
    }
    nodes
}

/// Scan an rclone remote (S3/WebDAV/SMB/SFTP/…) via `rclone lsf` → the same response shape.
fn rclone_response(remote: &str, cfg: &Cfg) -> Result<(String, u64, usize), String> {
    let t0 = std::time::Instant::now();
    let out = Command::new("rclone")
        .args(["lsf", "-R", "--files-only", "--format", "sp", "--separator", "\t", remote])
        .output()
        .map_err(|e| format!("rclone: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let lines = text.lines().filter_map(|l| {
        let (s, p) = l.split_once('\t')?;
        Some((s.trim().parse::<u64>().ok()?, p.to_string()))
    });
    Ok(finish(build_tree_from_flat(remote, lines), t0.elapsed().as_secs_f64() * 1e3, cfg))
}

// ── reports: persisted scans (save · list · open · import) ──

fn reports_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|_| PathBuf::from("."));
    let d = base.join("sdirstat/reports");
    let _ = std::fs::create_dir_all(&d);
    d
}

fn list_reports() -> String {
    let mut out = String::from("[");
    if let Ok(s) = std::fs::read_to_string(reports_dir().join("index.ndjson")) {
        let mut first = true;
        for line in s.lines().filter(|l| !l.trim().is_empty()) {
            if !first { out.push(','); }
            first = false;
            out.push_str(line);
        }
    }
    out.push(']');
    out
}

fn get_report(id: &str) -> Option<String> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        return None;
    }
    std::fs::read_to_string(reports_dir().join(format!("{id}.json"))).ok()
}

/// Persist a scan response as a report; returns the meta JSON line.
fn save_report(label: &str, src: &str, source: &str, response: &str, total: u64, entries: usize) -> String {
    let dir = reports_dir();
    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let id = format!("r{ms}");
    let mut meta = format!("{{\"id\":\"{id}\",\"path\":\"");
    json_escape(src, &mut meta);
    meta.push_str("\",\"label\":\"");
    json_escape(if label.is_empty() { src } else { label }, &mut meta);
    meta.push_str(&format!("\",\"time\":{},\"total\":{total},\"entries\":{entries},\"source\":\"{source}\"}}", ms / 1000));
    let file = format!("{{\"meta\":{meta},\"data\":{response}}}");
    let _ = std::fs::write(dir.join(format!("{id}.json")), file);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("index.ndjson")) {
        let _ = writeln!(f, "{meta}");
    }
    meta
}

fn url_unescape(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
            out.push(h(b[i + 1]) * 16 + h(b[i + 2]));
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn cache_size(s: &str) -> u64 {
    let s = s.trim();
    let (num, mult): (&str, u64) = match s.as_bytes().last() {
        Some(b'K') => (&s[..s.len() - 1], 1024),
        Some(b'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };
    num.parse::<u64>().unwrap_or(0) * mult
}

fn ensure_cache_dir(abspath: &str, nodes: &mut Vec<Node>, dirmap: &mut HashMap<String, usize>) -> usize {
    if let Some(&id) = dirmap.get(abspath) {
        return id;
    }
    let mut parent = 0usize;
    let mut acc = String::new();
    for comp in abspath.split('/').filter(|s| !s.is_empty()) {
        acc.push('/');
        acc.push_str(comp);
        if let Some(&id) = dirmap.get(&acc) {
            parent = id;
        } else {
            let id = nodes.len();
            let mut nd = Node::empty();
            nd.parent = parent;
            nd.name = comp.to_string();
            nd.is_dir = true;
            nodes.push(nd);
            dirmap.insert(acc.clone(), id);
            parent = id;
        }
    }
    parent
}

/// Parse a QDirStat cache file (v1/v2) into a Node arena — the interop import path.
fn parse_qdirstat_cache(text: &str) -> Vec<Node> {
    let mut root = Node::empty();
    root.is_dir = true;
    root.name = "/".to_string();
    let mut nodes = vec![root];
    let mut dirmap: HashMap<String, usize> = HashMap::new();
    let mut cur = 0usize;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let mut it = line.split_whitespace();
        let ty = match it.next() { Some(t) => t, None => continue };
        let raw = match it.next() { Some(p) => p, None => continue };
        let size = it.next().map(cache_size).unwrap_or(0);
        if ty.eq_ignore_ascii_case("D") {
            cur = ensure_cache_dir(&url_unescape(raw), &mut nodes, &mut dirmap);
            nodes[cur].own = size;
            nodes[cur].sub = size;
        } else {
            let name = url_unescape(raw);
            let (parent, leaf) = if name.starts_with('/') {
                let p = Path::new(&name);
                let dir = p.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
                let leaf = p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| name.clone());
                (ensure_cache_dir(&dir, &mut nodes, &mut dirmap), leaf)
            } else {
                (cur, name)
            };
            let mut nd = Node::empty();
            nd.parent = parent;
            nd.name = leaf;
            nd.own = size; nd.sub = size; nd.size = size;
            nd.is_link = ty.eq_ignore_ascii_case("L");
            nodes.push(nd);
        }
    }
    nodes
}

/// Import a server-side file as a report: a QDirStat cache, or a sdirstat `--json` tree.
fn import_file(file: &str, label: &str) -> Result<String, String> {
    let content = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    let t = content.trim_start();
    let cfg = Cfg { max_depth: 1 << 20, top: 1 << 20, threads: 1, apparent: false, no_stat: false, iouring: false, max_entries: usize::MAX, one_fs: false };
    if t.starts_with("[qdirstat") {
        let (resp, total, n) = finish(parse_qdirstat_cache(&content), 0.0, &cfg);
        Ok(save_report(label, file, "cache", &resp, total, n))
    } else if t.starts_with('{') {
        // a sdirstat --json tree: {"n":…,"v":N,…}. Wrap as a scan response, lift the root total.
        let total = t.split("\"v\":").nth(1)
            .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).find(|x| !x.is_empty()))
            .and_then(|x| x.parse().ok())
            .unwrap_or(0);
        let resp = format!("{{\"scan_ms\":0,\"entries\":0,\"tree\":{},\"types\":[]}}", t);
        Ok(save_report(label, file, "json", &resp, total, 0))
    } else {
        Err("unrecognized file (expected a QDirStat cache or a sdirstat JSON tree)".into())
    }
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let h = |c: u8| (c as char).to_digit(16).unwrap_or(0) as u8;
                out.push(h(b[i + 1]) * 16 + h(b[i + 2]));
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn http_write(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
}

/// Open a path with the OS default handler (xdg-open / open / explorer).
#[cfg(target_os = "linux")]
fn open_with(path: &str) -> Result<(), String> {
    Command::new("xdg-open").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}
#[cfg(target_os = "macos")]
fn open_with(path: &str) -> Result<(), String> {
    Command::new("open").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}
#[cfg(target_os = "windows")]
fn open_with(path: &str) -> Result<(), String> {
    Command::new("explorer").arg(path).spawn().map(|_| ()).map_err(|e| e.to_string())
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn open_with(_path: &str) -> Result<(), String> { Err("open not supported on this platform".into()) }

/// Move a path to the system trash / Recycle Bin — reversible.
#[cfg(target_os = "linux")]
fn trash(path: &str) -> Result<String, String> {
    let st = Command::new("gio").arg("trash").arg("--").arg(path).status().map_err(|e| e.to_string())?;
    if st.success() { Ok("moved to trash".into()) } else { Err("gio trash failed".into()) }
}
#[cfg(target_os = "macos")]
fn trash(path: &str) -> Result<String, String> {
    let script = format!("tell application \"Finder\" to delete POSIX file \"{}\"", path.replace('"', "\\\""));
    let st = Command::new("osascript").arg("-e").arg(script).status().map_err(|e| e.to_string())?;
    if st.success() { Ok("moved to trash".into()) } else { Err("trash failed".into()) }
}
#[cfg(target_os = "windows")]
fn trash(path: &str) -> Result<String, String> {
    let p = path.replace('\'', "''");
    let ps = format!(
        "Add-Type -AssemblyName Microsoft.VisualBasic; if (Test-Path -PathType Container '{0}') {{ \
         [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory('{0}','OnlyErrorDialogs','SendToRecycleBin') }} else {{ \
         [Microsoft.VisualBasic.FileIO.FileSystem]::DeleteFile('{0}','OnlyErrorDialogs','SendToRecycleBin') }}", p);
    let st = Command::new("powershell").args(["-NoProfile", "-Command", &ps]).status().map_err(|e| e.to_string())?;
    if st.success() { Ok("moved to trash".into()) } else { Err("trash failed".into()) }
}
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn trash(_path: &str) -> Result<String, String> { Err("trash not supported on this platform".into()) }

/// A file action — reversible by default. `trash` goes to the system trash/Recycle Bin (recoverable);
/// `open`/`reveal` use the OS default handler. No hard-delete: trash is reversible and the user empties
/// it themselves. The server is localhost-only and the UI confirms before trashing.
fn run_action(op: &str, path: &str) -> Result<String, String> {
    let p = Path::new(path);
    let home = std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")).unwrap_or_default();
    if path.is_empty() || !p.is_absolute() || p.parent().is_none() || path == home {
        return Err("refused: unsafe or invalid path".into());
    }
    if !p.exists() {
        return Err("path does not exist".into());
    }
    match op {
        "trash" => trash(path),
        "open" => open_with(path).map(|_| "opened".into()),
        "reveal" => {
            let dir = p.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
            open_with(&dir).map(|_| "revealed".into())
        }
        _ => Err("unknown action".into()),
    }
}

fn handle_conn(mut stream: TcpStream, base: Cfg) {
    let mut buf = [0u8; 8192];
    let n = match stream.read(&mut buf) {
        Ok(n) if n > 0 => n,
        _ => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let line0 = req.lines().next().unwrap_or("");
    let method = line0.split_whitespace().next().unwrap_or("GET");
    let target = line0.split_whitespace().nth(1).unwrap_or("/");

    if target.starts_with("/act") {
        if method != "POST" {
            http_write(&mut stream, "405 Method Not Allowed", "text/plain", b"POST only");
            return;
        }
        let q = target.splitn(2, '?').nth(1).unwrap_or("");
        let (mut op, mut path) = (String::new(), String::new());
        for kv in q.split('&') {
            let mut it = kv.splitn(2, '=');
            match (it.next(), it.next()) {
                (Some("op"), Some(v)) => op = url_decode(v),
                (Some("path"), Some(v)) => path = url_decode(v),
                _ => {}
            }
        }
        let body = match run_action(&op, &path) {
            Ok(m) => format!("{{\"ok\":true,\"msg\":\"{m}\"}}"),
            Err(e) => format!("{{\"ok\":false,\"err\":\"{}\"}}", e.replace('"', "'")),
        };
        http_write(&mut stream, "200 OK", "application/json", body.as_bytes());
        return;
    }
    if target == "/" || target.starts_with("/?") || target == "/index.html" {
        http_write(&mut stream, "200 OK", "text/html; charset=utf-8", include_str!("app.html").as_bytes());
        return;
    }
    if target.starts_with("/scan") {
        let mut cfg = base;
        let path = qparam(target, "path").unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        if let Some(t) = qparam(target, "top").and_then(|v| v.parse().ok()) { cfg.top = t; }
        if let Some(d) = qparam(target, "depth").and_then(|v| v.parse().ok()) { cfg.max_depth = d; }
        let (body, _, _) = scan_response(&path, &cfg);
        http_write(&mut stream, "200 OK", "application/json", body.as_bytes());
        return;
    }
    // ── sources ──
    if target.starts_with("/mounts") {
        http_write(&mut stream, "200 OK", "application/json", mounts_json().as_bytes());
        return;
    }
    if target.starts_with("/remotes") {
        http_write(&mut stream, "200 OK", "application/json", remotes_json().as_bytes());
        return;
    }
    if target.starts_with("/remote/scan") {
        let body = match rclone_response(&qparam(target, "remote").unwrap_or_default(), &base) {
            Ok((b, _, _)) => b,
            Err(e) => format!("{{\"error\":\"{}\"}}", e.replace('"', "'")),
        };
        http_write(&mut stream, "200 OK", "application/json", body.as_bytes());
        return;
    }
    // ── reports ──
    if target.starts_with("/report/get") {
        match get_report(&qparam(target, "id").unwrap_or_default()) {
            Some(j) => http_write(&mut stream, "200 OK", "application/json", j.as_bytes()),
            None => http_write(&mut stream, "404 Not Found", "application/json", br#"{"error":"not found"}"#),
        }
        return;
    }
    if target.starts_with("/report/save") {
        let label = qparam(target, "label").unwrap_or_default();
        let saved = if let Some(r) = qparam(target, "remote") {
            rclone_response(&r, &base).ok().map(|(b, t, n)| save_report(&label, &r, "rclone", &b, t, n))
        } else {
            let p = qparam(target, "path").unwrap_or_default();
            let (b, t, n) = scan_response(&p, &base);
            Some(save_report(&label, &p, "fs", &b, t, n))
        };
        let body = match saved {
            Some(meta) => format!("{{\"ok\":true,\"meta\":{meta}}}"),
            None => r#"{"ok":false,"err":"scan failed"}"#.into(),
        };
        http_write(&mut stream, "200 OK", "application/json", body.as_bytes());
        return;
    }
    if target.starts_with("/report/import") {
        let body = match import_file(&qparam(target, "file").unwrap_or_default(), &qparam(target, "label").unwrap_or_default()) {
            Ok(meta) => format!("{{\"ok\":true,\"meta\":{meta}}}"),
            Err(e) => format!("{{\"ok\":false,\"err\":\"{}\"}}", e.replace('"', "'")),
        };
        http_write(&mut stream, "200 OK", "application/json", body.as_bytes());
        return;
    }
    if target.starts_with("/reports") {
        http_write(&mut stream, "200 OK", "application/json", list_reports().as_bytes());
        return;
    }
    http_write(&mut stream, "404 Not Found", "text/plain", b"not found");
}

/// `sdirstat serve [port]` — the live local app: scans any folder on demand, serves the full GUI.
/// Bound to 127.0.0.1 only (the scan endpoint reads the filesystem — never expose it to the network).
fn serve(port: u16, cfg: Cfg) {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind 127.0.0.1");
    eprintln!("sdirstat — full GUI live at http://127.0.0.1:{port}   (Ctrl-C to stop)");
    for stream in listener.incoming().flatten() {
        std::thread::spawn(move || handle_conn(stream, cfg));
    }
}

const USAGE: &str = "\
sdirstat — parallel disk-usage analyzer (treemap/sunburst web GUI + QDirStat cache)

USAGE
  sdirstat <path> [options]      scan a directory (default: writes report.html)
  sdirstat serve [-p PORT]       live web GUI at http://127.0.0.1:PORT (default 8080)

OUTPUT (default: a self-contained HTML treemap report)
  --json                emit a nested JSON tree instead
  --cache               emit a QDirStat v2.0 cache file (drop-in for qdirstat-cache-writer)
  --total               print only the grand total (scan + fold, no serialization)
  -o FILE               output path (default: report.html / tree.json / out.qdirstat.cache)

SCAN
  --threads N           worker threads (default: CPU count; 1 = single-threaded)
  --max-depth N         maximum recursion depth (default 40)
  --max-entries N       OOM-guard entry ceiling (default 32M; 0 = unlimited). Hitting it warns
                        and leaves the scan INCOMPLETE — raise it for a whole-/ scan
  --top K               children kept per directory in pruned output (default 80)
  --apparent            count apparent size (st_size) instead of allocated blocks
  -x, --one-file-system stay on the root filesystem (du -x): skips /proc,/sys,/dev and other mounts
  --iouring             io_uring batched-statx backend (Linux x86_64; for cold/SSD scans)
  -p, --port N          port for `serve` (default 8080)
  -h, --help            show this help

Sizes are allocated (st_blocks×512, like du/baobab) with hardlink dedup by default.
The GUI binds 127.0.0.1 only. Full docs: README.md · SECURITY.md\n";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return;
    }
    let mut root = ".".to_string();
    let mut out = String::new();
    let mut mode = "html";
    let mut port: u16 = 8080;
    let mut cfg = Cfg { max_depth: 40, top: 80, threads: 0, apparent: false, no_stat: false, iouring: false, max_entries: DEFAULT_MAX_ENTRIES, one_fs: false };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "serve" => mode = "serve",
            "--port" | "-p" => port = it.next().and_then(|s| s.parse().ok()).unwrap_or(8080),
            "--json" => mode = "json",
            "--cache" => mode = "cache",
            "--total" => mode = "total",
            "--no-stat" => cfg.no_stat = true,
            "--iouring" => cfg.iouring = true,
            "--apparent" => cfg.apparent = true,
            "-o" => out = it.next().cloned().unwrap_or_default(),
            "--max-depth" => cfg.max_depth = it.next().and_then(|s| s.parse().ok()).unwrap_or(40),
            "--top" => cfg.top = it.next().and_then(|s| s.parse().ok()).unwrap_or(80),
            "--threads" => cfg.threads = it.next().and_then(|s| s.parse().ok()).unwrap_or(0),
            "--max-entries" => {
                let v = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(DEFAULT_MAX_ENTRIES);
                cfg.max_entries = if v == 0 { usize::MAX } else { v }; // 0 = unlimited
            }
            "--one-file-system" | "-x" => cfg.one_fs = true,
            s if !s.starts_with('-') => root = s.to_string(),
            _ => {}
        }
    }
    if mode == "serve" {
        serve(port, cfg);
        return;
    }
    if out.is_empty() {
        out = match mode {
            "json" => "tree.json".into(),
            "cache" => "out.qdirstat.cache".into(),
            _ => "report.html".into(),
        };
    }

    // Per-phase wall-clock, always on: each pipeline seam (scan → fold → children → emit → write)
    // is timed separately so the dominant phase is visible in one run, no profiler needed.
    let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e3;

    // ── 1. scan → arena (parallel Web walk) ──
    let t_scan = std::time::Instant::now();
    let canon = std::fs::canonicalize(&root).unwrap_or_else(|_| Path::new(&root).to_path_buf());
    let mut nodes = scan_backend(Path::new(&root), canon.to_string_lossy().into_owned(), &cfg);
    let scan_ms = ms(t_scan);

    // ── 2. the size fold (B/U accumulator), one reverse pass (parent_idx < child_idx) ──
    let t_fold = std::time::Instant::now();
    for i in (1..nodes.len()).rev() {
        let s = nodes[i].sub;
        let p = nodes[i].parent;
        nodes[p].sub += s;
    }
    let total = nodes[0].sub;
    let fold_ms = ms(t_fold);
    eprintln!("scanned {} entries · {} · scan {scan_ms:.0} ms · fold {fold_ms:.0} ms", nodes.len(), human(total));
    // No silent caps: if the walk stopped descending at the entry ceiling, the tree (and the total)
    // is INCOMPLETE — say so loudly, don't hand back a partial number as if it were the answer.
    if nodes.len() >= cfg.max_entries {
        eprintln!("⚠ WARNING: hit the {}-entry ceiling — the scan is INCOMPLETE (stopped descending). \
                   Totals are a lower bound. Raise it with --max-entries N (0 = unlimited).", cfg.max_entries);
    }

    // scan + fold only (the fair comparison vs total-only tools like diskus/du -s) — no serialize
    if mode == "total" {
        eprintln!("phases: scan {scan_ms:.1} · fold {fold_ms:.1} ms");
        println!("{total}\t{}", human(total));
        return;
    }

    // children adjacency — CSR (the Graph face on the array carrier), not n little vecs
    let t_children = std::time::Instant::now();
    let kids = hash::Csr::from_parents(nodes.len(), |i| nodes[i].parent);
    let children_ms = ms(t_children);

    // ── 3. emit (fromTerm at the chosen α) ──
    if mode == "cache" {
        let t_emit = std::time::Instant::now(); // emit_cache serializes and writes the file in one pass
        emit_cache(&out, &kids, &nodes);
        eprintln!("wrote {out} (QDirStat cache format)");
        eprintln!("phases: scan {scan_ms:.1} · fold {fold_ms:.1} · children {children_ms:.1} · emit+write {:.1} ms", ms(t_emit));
        return;
    }
    let t_emit = std::time::Instant::now();
    let mut acc = hash::Acc::with_capacity(nodes.len().saturating_mul(64).max(1 << 20));
    emit_json(0, &kids, &nodes, &cfg, &mut acc);
    let emit_ms = ms(t_emit);
    if mode == "json" {
        let t_write = std::time::Instant::now();
        std::fs::write(&out, acc.as_slice()).expect("write json");
        eprintln!("wrote {out} ({} KB)", acc.len() / 1024);
        eprintln!("phases: scan {scan_ms:.1} · fold {fold_ms:.1} · children {children_ms:.1} · emit {emit_ms:.1} · write {:.1} ms", ms(t_write));
        return;
    }
    // html: split the viewer template at the data marker and stream prefix + data + suffix straight
    // to the file — no whole-document `.replace` (was a second 168 MB copy). The small SCANMS/
    // NENTRIES substitutions happen on the ~28 KB template only, so a stray "__SCANMS__" inside a
    // filename can no longer be rewritten.
    let t_write = std::time::Instant::now();
    let tmpl = include_str!("viewer.html")
        .replace("__SCANMS__", &format!("{scan_ms:.0}"))
        .replace("__NENTRIES__", &nodes.len().to_string());
    let (pre, post) = tmpl.split_once("/*__DATA__*/").expect("viewer.html data marker");
    let mut f = std::io::BufWriter::new(std::fs::File::create(&out).expect("create html"));
    f.write_all(pre.as_bytes()).expect("write html");
    f.write_all(acc.as_slice()).expect("write html");
    f.write_all(post.as_bytes()).expect("write html");
    f.flush().expect("flush html");
    let kb = (pre.len() + acc.len() + post.len()) / 1024;
    eprintln!("wrote {out} ({kb} KB) — open it in a browser");
    eprintln!("phases: scan {scan_ms:.1} · fold {fold_ms:.1} · children {children_ms:.1} · emit {emit_ms:.1} · inject+write {:.1} ms", ms(t_write));
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
fn emit_cache(out: &str, kids: &hash::Csr, nodes: &[Node]) {
    // Cache files run to hundreds of MB on a real tree; size the accumulator generously up front
    // so the fold never re-allocates (one buffer, the append monoid).
    let mut acc = hash::Acc::with_capacity(nodes.len().saturating_mul(96).max(1 << 16));
    acc.str("[qdirstat 2.0 cache file]\n# Generated by sdirstat\n# Do not edit!\n#\n");
    cache_dir(&mut acc, 0, &nodes[0].name.clone(), kids, nodes);
    std::fs::write(out, acc.as_slice()).expect("write cache");
}

/// The mandatory V2.0 tail after type+path: size, uid, gid, perm, mtime (+ optional blocks/links),
/// folded straight into the accumulator (was `format!("\t{}\t{}\t{}\t0{:o}\t0x{:x}", …)`).
fn cache_tail(acc: &mut hash::Acc, n: &Node) {
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
fn cache_dir(acc: &mut hash::Acc, i: usize, path: &str, kids: &hash::Csr, nodes: &[Node]) {
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
