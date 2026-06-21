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

use sdirstat::uring;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
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
}

const IOURING_QD: u32 = 256; // per-thread io_uring queue depth (in-flight statx)

const HARD_CAP: usize = 8_000_000;

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

/// The parallel Web walk — `pmapAt` over the directory web, schedule-free (`Web.preduce_schedule_free`):
/// a shared work-queue of directories drained by N workers. Node ids are handed out by one atomic
/// counter, and a dir's id is always allocated *before* it is processed, so parent_id < child_id
/// still holds — the size fold stays one reverse pass, independent of which thread saw what when.
fn parallel_scan(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    let counter = AtomicUsize::new(1); // 0 = root
    let stack: Mutex<Vec<(PathBuf, usize, u32)>> = Mutex::new(vec![(root.to_path_buf(), 0, 1)]);
    let active = AtomicUsize::new(1);
    let cv = Condvar::new();
    // hardlink dedup: a multiply-linked inode's blocks are counted once (like du/qdirstat).
    // Only touched for nlink>1 entries, so the lock is cold for the overwhelming majority.
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
            sc.spawn(move || {
                let mut local: Vec<Rec> = Vec::new();
                loop {
                    // pop a directory, or exit when the whole web is drained
                    let item = {
                        let mut s = stack.lock().unwrap();
                        loop {
                            if let Some(it) = s.pop() {
                                break Some(it);
                            }
                            if active.load(Ordering::SeqCst) == 0 {
                                break None;
                            }
                            s = cv.wait(s).unwrap();
                        }
                    };
                    let Some((dir, pid, depth)) = item else { break };
                    let mut childdirs: Vec<(PathBuf, usize, u32)> = Vec::new();
                    if depth < cfg.max_depth && counter.load(Ordering::Relaxed) < HARD_CAP {
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
                                let rec = match ent.metadata() {
                                    Ok(m) => {
                                        // metric: allocated (st_blocks×512, like du/baobab) by default,
                                        // st_size with --apparent. Hardlinked inode's blocks counted once.
                                        let mut own = if cfg.apparent { m.len() } else { m.blocks() * 512 };
                                        if !is_dir && m.nlink() > 1
                                            && !seen.lock().unwrap().insert((m.dev(), m.ino()))
                                        {
                                            own = 0;
                                        }
                                        Rec { id, parent: pid, name, own, size: m.len(), blocks: m.blocks(),
                                              mtime: m.mtime(), uid: m.uid(), gid: m.gid(), mode: m.mode(),
                                              nlink: m.nlink(), is_dir, is_link }
                                    }
                                    Err(_) => Rec { id, parent: pid, name, own: 0, size: 0, blocks: 0,
                                              mtime: 0, uid: 0, gid: 0, mode: 0, nlink: 1, is_dir, is_link },
                                };
                                local.push(rec);
                                if is_dir && !is_link {
                                    childdirs.push((ent.path(), id, depth + 1));
                                }
                            }
                        }
                    }
                    let mut s = stack.lock().unwrap();
                    let n = childdirs.len();
                    for c in childdirs {
                        s.push(c);
                    }
                    active.fetch_add(n, Ordering::SeqCst); // children queued
                    active.fetch_sub(1, Ordering::SeqCst); // this dir done
                    cv.notify_all();
                }
                *recs[t].lock().unwrap() = local;
            });
        }
    });

    // assemble the dense arena (ids are 0..n, each written exactly once)
    let n = counter.load(Ordering::Relaxed);
    let mut nodes: Vec<Node> = Vec::with_capacity(n);
    nodes.resize_with(n, Node::empty);
    // the root directory's own inode blocks count too (du counts the top dir itself)
    let mut root_node = Node::empty();
    root_node.name = root_name;
    root_node.is_dir = true;
    if let Ok(m) = std::fs::symlink_metadata(root) {
        root_node.own = if cfg.apparent { m.len() } else { m.blocks() * 512 };
        root_node.sub = root_node.own;
        root_node.size = m.len();
        root_node.blocks = m.blocks();
        root_node.mtime = m.mtime();
        root_node.uid = m.uid();
        root_node.gid = m.gid();
        root_node.mode = m.mode();
        root_node.nlink = m.nlink();
    }
    nodes[0] = root_node;
    for r in &recs {
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

/// Place the per-thread Recs into the dense arena by id; stat the root dir itself (du counts it).
fn assemble(n: usize, recs: &[Mutex<Vec<Rec>>], root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    let mut nodes: Vec<Node> = Vec::with_capacity(n);
    nodes.resize_with(n, Node::empty);
    let mut root_node = Node::empty();
    root_node.name = root_name;
    root_node.is_dir = true;
    if let Ok(m) = std::fs::symlink_metadata(root) {
        root_node.own = if cfg.apparent { m.len() } else { m.blocks() * 512 };
        root_node.sub = root_node.own;
        root_node.size = m.len();
        root_node.blocks = m.blocks();
        root_node.mtime = m.mtime();
        root_node.uid = m.uid();
        root_node.gid = m.gid();
        root_node.mode = m.mode();
        root_node.nlink = m.nlink();
    }
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
fn parallel_scan_iouring(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    let counter = AtomicUsize::new(1);
    let stack: Mutex<Vec<(PathBuf, usize, u32)>> = Mutex::new(vec![(root.to_path_buf(), 0, 1)]);
    let active = AtomicUsize::new(1);
    let cv = Condvar::new();
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

                    if depth < cfg.max_depth && counter.load(Ordering::Relaxed) < HARD_CAP {
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
                                        if st.is_dir && !st.is_link && depth + 1 < cfg.max_depth {
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

/// Emit the pruned nested tree as JSON: {"n":name,"v":bytes,"d":1|0,"c":[...]}.
/// Per directory keep the `top` largest children; bucket the remainder into one "… (k more)"
/// leaf so totals stay exact and the tree stays explorable-but-bounded.
fn emit_json(i: usize, children: &[Vec<usize>], nodes: &[Node], cfg: &Cfg, out: &mut String) {
    out.push_str("{\"n\":\"");
    json_escape(&nodes[i].name, out);
    out.push_str(&format!("\",\"v\":{},\"d\":{}", nodes[i].sub, if nodes[i].is_dir { 1 } else { 0 }));
    let kids = &children[i];
    if !kids.is_empty() {
        let mut ks: Vec<usize> = kids.clone();
        ks.sort_by(|&a, &b| nodes[b].sub.cmp(&nodes[a].sub));
        out.push_str(",\"c\":[");
        let keep = ks.len().min(cfg.top);
        let mut first = true;
        for &c in ks.iter().take(keep) {
            if !first {
                out.push(',');
            }
            first = false;
            emit_json(c, children, nodes, cfg, out);
        }
        if ks.len() > keep {
            let rest: u64 = ks.iter().skip(keep).map(|&c| nodes[c].sub).sum();
            let n = ks.len() - keep;
            if !first {
                out.push(',');
            }
            out.push_str(&format!("{{\"n\":\"… ({n} more)\",\"v\":{rest},\"d\":0}}"));
        }
        out.push(']');
    }
    out.push('}');
}

/// per-extension size + count over the whole scan (QDirStat's File Type Statistics), as a JSON array.
fn type_stats_json(nodes: &[Node]) -> String {
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
    let mut out = String::from("[");
    for (i, (ext, (sz, cnt))) in tv.iter().take(60).enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str("{\"e\":\"");
        json_escape(ext, &mut out);
        out.push_str(&format!("\",\"v\":{sz},\"c\":{cnt}}}"));
    }
    out.push(']');
    out
}

/// Dispatch to the chosen scan backend (std fstatat vs io_uring batched statx), same `Vec<Node>`.
fn scan_backend(root: &Path, root_name: String, cfg: &Cfg) -> Vec<Node> {
    if cfg.iouring {
        parallel_scan_iouring(root, root_name, cfg)
    } else {
        parallel_scan(root, root_name, cfg)
    }
}

/// Scan a path and return (tree JSON, type-stats JSON, entry count, scan ms). Shared by serve.
fn scan_to_json(root: &str, cfg: &Cfg) -> (String, String, usize, f64) {
    let t0 = std::time::Instant::now();
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| Path::new(root).to_path_buf());
    let nodes = scan_backend(Path::new(root), canon.to_string_lossy().into_owned(), cfg);
    // fold + children
    let mut nodes = nodes;
    for i in (1..nodes.len()).rev() {
        let s = nodes[i].sub;
        let p = nodes[i].parent;
        nodes[p].sub += s;
    }
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for i in 1..nodes.len() {
        let p = nodes[i].parent;
        children[p].push(i);
    }
    let scan_ms = t0.elapsed().as_secs_f64() * 1e3;
    let mut json = String::with_capacity(1 << 20);
    emit_json(0, &children, &nodes, cfg, &mut json);
    let types = type_stats_json(&nodes);
    (json, types, nodes.len(), scan_ms)
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

/// A file action — reversible by default. `trash` uses `gio trash` (recoverable from the system
/// trash); `open`/`reveal` shell out to `xdg-open`. No hard-delete: trash is reversible and the
/// user empties it themselves. The server is localhost-only and the UI confirms before trashing.
fn run_action(op: &str, path: &str) -> Result<String, String> {
    let home = std::env::var("HOME").unwrap_or_default();
    if !path.starts_with('/') || path == "/" || path == home {
        return Err("refused: unsafe or invalid path".into());
    }
    if !Path::new(path).exists() {
        return Err("path does not exist".into());
    }
    match op {
        "trash" => {
            let st = Command::new("gio").arg("trash").arg("--").arg(path).status().map_err(|e| e.to_string())?;
            if st.success() { Ok("moved to trash".into()) } else { Err("gio trash failed".into()) }
        }
        "open" => {
            Command::new("xdg-open").arg(path).spawn().map_err(|e| e.to_string())?;
            Ok("opened".into())
        }
        "reveal" => {
            let dir = Path::new(path).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| "/".into());
            Command::new("xdg-open").arg(&dir).spawn().map_err(|e| e.to_string())?;
            Ok("revealed".into())
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
        let q = target.splitn(2, '?').nth(1).unwrap_or("");
        let mut path = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let mut cfg = base;
        for kv in q.split('&') {
            let mut it = kv.splitn(2, '=');
            match (it.next(), it.next()) {
                (Some("path"), Some(v)) => path = url_decode(v),
                (Some("top"), Some(v)) => cfg.top = v.parse().unwrap_or(cfg.top),
                (Some("depth"), Some(v)) => cfg.max_depth = v.parse().unwrap_or(cfg.max_depth),
                _ => {}
            }
        }
        let (json, types, n, ms) = scan_to_json(&path, &cfg);
        let body = format!("{{\"scan_ms\":{ms:.0},\"entries\":{n},\"tree\":{json},\"types\":{types}}}");
        http_write(&mut stream, "200 OK", "application/json", body.as_bytes());
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

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut root = ".".to_string();
    let mut out = String::new();
    let mut mode = "html";
    let mut port: u16 = 8080;
    let mut cfg = Cfg { max_depth: 40, top: 80, threads: 0, apparent: false, no_stat: false, iouring: false };
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

    // ── 1. scan → arena (parallel Web walk) ──
    let t0 = std::time::Instant::now();
    let canon = std::fs::canonicalize(&root).unwrap_or_else(|_| Path::new(&root).to_path_buf());
    let mut nodes = scan_backend(Path::new(&root), canon.to_string_lossy().into_owned(), &cfg);

    // ── 2. the size fold (B/U accumulator), one reverse pass (parent_idx < child_idx) ──
    for i in (1..nodes.len()).rev() {
        let s = nodes[i].sub;
        let p = nodes[i].parent;
        nodes[p].sub += s;
    }
    let total = nodes[0].sub;
    let scan_ms = t0.elapsed().as_secs_f64() * 1e3;
    eprintln!("scanned {} entries · {} · {:.0} ms", nodes.len(), human(total), scan_ms);

    // scan + fold only (the fair comparison vs total-only tools like diskus/du -s) — no serialize
    if mode == "total" {
        println!("{total}\t{}", human(total));
        return;
    }

    // children lists
    let mut children: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
    for i in 1..nodes.len() {
        let p = nodes[i].parent;
        children[p].push(i);
    }

    // ── 3. emit (fromTerm at the chosen α) ──
    if mode == "cache" {
        emit_cache(&out, &children, &nodes);
        eprintln!("wrote {out} (QDirStat cache format)");
        return;
    }
    let mut json = String::with_capacity(1 << 20);
    emit_json(0, &children, &nodes, &cfg, &mut json);
    if mode == "json" {
        std::fs::write(&out, &json).expect("write json");
        eprintln!("wrote {out} ({} KB)", json.len() / 1024);
        return;
    }
    // html: inject the data into the self-contained viewer template
    let html = include_str!("viewer.html")
        .replace("/*__DATA__*/", &json)
        .replace("__SCANMS__", &format!("{scan_ms:.0}"))
        .replace("__NENTRIES__", &nodes.len().to_string());
    std::fs::write(&out, &html).expect("write html");
    eprintln!("wrote {out} ({} KB) — open it in a browser", html.len() / 1024);
}

/// QDirStat cache format V2.0 emit — full fidelity (size = st_size, uid, gid, octal perm, hex mtime,
/// plus the optional `blocks:` for sparse files and `links:` for hardlinks). Dirs get an absolute
/// path; their files follow as base names (pre-order grouping, the relative-name space saver). Type
/// is D / L (symlink) / F (everything else).
fn emit_cache(out: &str, children: &[Vec<usize>], nodes: &[Node]) {
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).expect("create"));
    writeln!(f, "[qdirstat 2.0 cache file]").unwrap();
    writeln!(f, "# Generated by sdirstat").unwrap();
    writeln!(f, "# Do not edit!").unwrap();
    writeln!(f, "#").unwrap();
    fn esc(s: &str) -> String {
        s.bytes()
            .flat_map(|b| if b <= 0x20 || b == b'%' { format!("%{b:02X}").into_bytes() } else { vec![b] })
            .map(|b| b as char)
            .collect()
    }
    // the mandatory V2.0 tail after type+path: size, uid, gid, perm, mtime (+ optional blocks/links).
    fn tail(n: &Node) -> String {
        let perm = n.mode & 0o7777;
        let mut s = format!("\t{}\t{}\t{}\t0{:o}\t0x{:x}", n.size, n.uid, n.gid, perm, n.mtime);
        if !n.is_dir && n.blocks > 0 && n.blocks * 512 < n.size {
            s += &format!("\tblocks: {}", n.blocks); // sparse file
        }
        if !n.is_dir && n.nlink > 1 {
            s += &format!("\tlinks: {}", n.nlink); // hardlinked
        }
        s
    }
    fn ty(n: &Node) -> &'static str {
        if n.is_dir { "D" } else if n.is_link { "L" } else { "F" }
    }
    fn rec(i: usize, path: &str, children: &[Vec<usize>], nodes: &[Node], f: &mut impl Write) {
        writeln!(f, "D {}{}", esc(path), tail(&nodes[i])).unwrap();
        for &c in &children[i] {
            if !nodes[c].is_dir {
                writeln!(f, "{} {}{}", ty(&nodes[c]), esc(&nodes[c].name), tail(&nodes[c])).unwrap();
            }
        }
        for &c in &children[i] {
            if nodes[c].is_dir {
                rec(c, &format!("{}/{}", path.trim_end_matches('/'), nodes[c].name), children, nodes, f);
            }
        }
    }
    rec(0, &nodes[0].name.clone(), children, nodes, &mut f);
}
