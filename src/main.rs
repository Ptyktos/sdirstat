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
use sdirstat::internal::*;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};


/// Scan a local path → (response JSON, total bytes, entries). Shared by /scan and report-save.
fn scan_response(root: &str, cfg: &Cfg) -> (String, u64, usize) {
    let t0 = std::time::Instant::now();
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| Path::new(root).to_path_buf());
    let nodes = scan_backend(Path::new(root), canon.to_string_lossy().into_owned(), cfg);
    finish(nodes, t0.elapsed().as_secs_f64() * 1e3, cfg)
}

// ── the scan cache — O(1) retrieval at the path coordinate (NullWebRetrieval: "an O(1) navigation,
//    not a search"). A scanned path's response is cached, keyed by the path (HashTrinity `get`, O(1)).
//    Validation reads ONE coordinate — the scanned directory's own mtime — in O(1): unchanged ⇒ certify
//    the cache (no re-walk); changed ⇒ rescan. This is "read each flaw at its OWN coordinate," NOT an
//    enumeration: a change inside a SUBdirectory is caught when you navigate INTO it (that child path is
//    its own cache key, validated by its own mtime, O(1) per step) — or eagerly via the Rescan button.
//    `force` (Rescan) skips the check. mtime catches add/remove/rename at the coordinate; in-place file
//    growth that leaves the dir mtime is the Rescan case.
struct Cached {
    body: String,
    root_mtime: i128, // the scanned directory's own mtime — the single O(1) coordinate a revisit reads
    touched: std::time::Instant,
}
fn scan_cache() -> &'static Mutex<HashMap<String, Cached>> {
    static C: OnceLock<Mutex<HashMap<String, Cached>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}
const SCAN_CACHE_CAP: usize = 32;

/// A directory's mtime in **nanoseconds** — fine enough to catch a sub-second change (a same-second
/// `st_mtime` in seconds would miss it). `None` if the path is gone — a deletion is itself a flaw
/// (`≠ Some(snapshot)`), so the cache invalidates. Snapshot and recheck both go through here, so the
/// two readings are always comparable.
#[cfg(unix)]
fn mtime_of(p: &str) -> Option<i128> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(p).ok().map(|m| m.mtime() as i128 * 1_000_000_000 + m.mtime_nsec() as i128)
}
#[cfg(not(unix))]
fn mtime_of(p: &str) -> Option<i128> {
    std::fs::symlink_metadata(p).ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i128)
}

/// Scan a path through the cache. `force` (the Rescan button) skips the check and re-walks.
/// The hit path is O(1): one `stat` of the path's own directory + a `HashMap` get — no enumeration.
fn scan_cached(path: &str, cfg: &Cfg, force: bool) -> String {
    // Key by (canonical path, emit shape): different `top`/`depth`/metric views render to different
    // bodies, so they cache as distinct coordinates.
    let canon = std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string());
    let key = format!("{canon}|t{}|d{}|a{}", cfg.top, cfg.max_depth, cfg.apparent as u8);
    let now_mtime = mtime_of(&canon); // O(1): one stat of the scanned directory itself — the coordinate

    if !force {
        let hit = {
            let cache = scan_cache().lock().unwrap();
            cache.get(&key).and_then(|e| {
                if Some(e.root_mtime) == now_mtime { Some(e.body.clone()) } else { None }
            })
        };
        if let Some(body) = hit {
            if let Some(e) = scan_cache().lock().unwrap().get_mut(&key) {
                e.touched = std::time::Instant::now();
            }
            return body; // O(1) certify — the coordinate's mtime is unchanged
        }
    }

    // Cold, a flaw, or forced: full scan WITHOUT holding the lock, then recache. The ONLY overhead
    // added to a cold scan is `now_mtime` above (one stat) — O(1), no extra pass over the arena.
    let t0 = std::time::Instant::now();
    let croot = std::fs::canonicalize(path).unwrap_or_else(|_| Path::new(path).to_path_buf());
    let nodes = scan_backend(Path::new(path), croot.to_string_lossy().into_owned(), cfg);
    let (body, _, _) = finish(nodes, t0.elapsed().as_secs_f64() * 1e3, cfg);
    let mut cache = scan_cache().lock().unwrap();
    if cache.len() >= SCAN_CACHE_CAP && !cache.contains_key(&key) {
        // Bounded eviction over ≤ SCAN_CACHE_CAP entries (a constant) — off the hot path, on insert
        // overflow only; the scan/revisit paths above never loop.
        if let Some(k) = cache.iter().min_by_key(|(_, v)| v.touched).map(|(k, _)| k.clone()) {
            cache.remove(&k); // evict least-recently-touched
        }
    }
    cache.insert(key, Cached { body: body.clone(), root_mtime: now_mtime.unwrap_or(i128::MIN), touched: std::time::Instant::now() });
    body
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
        let force = qparam(target, "force").as_deref() == Some("1");
        let body = scan_cached(&path, &cfg, force);
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

/// An OS-assigned free localhost port (bind :0, read it back, release it). Falls back to 8080.
fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .unwrap_or(8080)
}

/// Is `bin` an executable on `PATH`? (Keeps the zero-dep ethic — no `which` crate.)
fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

/// `sdirstat gui` — the desktop-launcher entry point: serve the GUI on a private loopback port and
/// open it as a standalone **app window** (not a browser tab). Blocks until the window is closed.
/// This is what the installed `.desktop` runs, so the binary alone is a usable desktop app.
fn gui(cfg: Cfg) {
    let port = free_port();
    let server = std::thread::spawn(move || serve(port, cfg));
    let url = format!("http://127.0.0.1:{port}/");
    // wait for the server to bind (well under a second) before opening the window
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(5) {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    if !open_app_window(&url) {
        // No app-mode browser: a tab was opened instead — keep serving until the process is killed.
        let _ = server.join();
    }
}

/// Open `url` as a dedicated app window. Prefers a chromium-family browser in `--app` mode (a clean
/// frameless window; `--class=sdirstat` so it groups under the .desktop entry) with a throwaway
/// profile so it is our own process to wait on. Returns true if such a window ran (and has since
/// closed); false if it fell back to the default browser (a tab), which does not block.
fn open_app_window(url: &str) -> bool {
    let chromium = ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "brave-browser"]
        .into_iter()
        .find(|b| on_path(b));
    if let Some(browser) = chromium {
        let prof = std::env::temp_dir().join(format!("sdirstat-gui-{}", std::process::id()));
        let status = Command::new(browser)
            .arg(format!("--app={url}"))
            .arg("--class=sdirstat")
            .arg("--no-first-run")
            .arg(format!("--user-data-dir={}", prof.display()))
            .status();
        let _ = std::fs::remove_dir_all(&prof);
        return status.map(|s| s.success()).unwrap_or(false);
    }
    let _ = Command::new("xdg-open").arg(url).status();
    false
}

/// `sdirstat install-desktop` — register a clickable menu entry for the GUI under the current user
/// (no root, no package needed). Writes an icon and a `.desktop` launcher that runs `<this binary>
/// gui`, so a single downloaded binary becomes a first-class desktop application.
#[cfg(target_os = "linux")]
fn install_desktop() -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    let home = std::env::var("HOME")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set"))?;
    let data = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
    let apps = format!("{data}/applications");
    let icondir = format!("{data}/icons/hicolor/scalable/apps");
    std::fs::create_dir_all(&apps)?;
    std::fs::create_dir_all(&icondir)?;
    std::fs::write(format!("{icondir}/sdirstat.svg"), include_str!("../packaging/sdirstat.svg"))?;
    let entry = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=sdirstat\n\
         GenericName=Disk Usage Analyzer\n\
         Comment=Explore disk usage as an interactive treemap or sunburst\n\
         Exec=\"{exe}\" gui\n\
         Icon=sdirstat\n\
         Terminal=false\n\
         Categories=Utility;System;Filesystem;\n\
         Keywords=disk;usage;treemap;sunburst;du;space;analyzer;\n\
         StartupWMClass=sdirstat\n",
        exe = exe.display()
    );
    let path = format!("{apps}/sdirstat.desktop");
    std::fs::write(&path, entry)?;
    let _ = Command::new("update-desktop-database").arg(&apps).status();
    println!(
        "installed: {path}\nicon:      {icondir}/sdirstat.svg\n\nLaunch \"sdirstat\" from your application menu, or run: {} gui",
        exe.display()
    );
    Ok(())
}

/// `sdirstat uninstall-desktop` — remove the user `.desktop` entry and icon from install-desktop.
#[cfg(target_os = "linux")]
fn uninstall_desktop() -> std::io::Result<()> {
    let home = std::env::var("HOME")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "HOME not set"))?;
    let data = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
    let apps = format!("{data}/applications");
    let desktop = format!("{apps}/sdirstat.desktop");
    let icon = format!("{data}/icons/hicolor/scalable/apps/sdirstat.svg");
    let _ = std::fs::remove_file(&desktop);
    let _ = std::fs::remove_file(&icon);
    let _ = Command::new("update-desktop-database").arg(&apps).status();
    println!("removed: {desktop}");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn install_desktop() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "install-desktop is Linux-only (XDG .desktop). Use `sdirstat gui` to open the app window.",
    ))
}
#[cfg(not(target_os = "linux"))]
fn uninstall_desktop() -> std::io::Result<()> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "uninstall-desktop is Linux-only."))
}

const USAGE: &str = "\
sdirstat — parallel disk-usage analyzer (treemap/sunburst web GUI + QDirStat cache)

USAGE
  sdirstat <path> [options]      scan a directory (default: writes report.html)
  sdirstat serve [-p PORT]       live web GUI at http://127.0.0.1:PORT (default 8080)
  sdirstat gui                   open the GUI as a standalone desktop app window
  sdirstat install-desktop       add a clickable \"sdirstat\" app to your menu (Linux, no root)
  sdirstat uninstall-desktop     remove that menu entry

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
            "gui" => mode = "gui",
            "install-desktop" => mode = "install-desktop",
            "uninstall-desktop" => mode = "uninstall-desktop",
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
    if mode == "gui" {
        gui(cfg);
        return;
    }
    if mode == "install-desktop" {
        if let Err(e) = install_desktop() {
            eprintln!("install-desktop failed: {e}");
            std::process::exit(1);
        }
        return;
    }
    if mode == "uninstall-desktop" {
        if let Err(e) = uninstall_desktop() {
            eprintln!("uninstall-desktop failed: {e}");
            std::process::exit(1);
        }
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

