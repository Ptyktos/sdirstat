// sdirstat — native desktop shell.
//
// The whole app is a thin wrapper: it launches the bundled `sdirstat` binary as a sidecar
// (`sdirstat serve --port <free>`), waits for the loopback server to come up, then opens a
// native window pointed at it. Because the window's origin *is* the loopback server, the
// existing GUI (`app.html`, served at `/`) and its `fetch('/scan')` / `/act` calls work
// unchanged — no frontend rewrite, and the zero-dependency core stays untouched.
#![cfg_attr(all(not(debug_assertions), target_os = "windows"), windows_subsystem = "windows")]

use std::net::{TcpListener, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_shell::process::CommandChild;
use tauri_plugin_shell::ShellExt;

/// Holds the spawned `sdirstat serve` child so it can be terminated when the app exits.
struct Server(Mutex<Option<CommandChild>>);

/// An OS-assigned free localhost port: bind to :0, read the port back, release it.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("reserve a localhost port")
}

/// Block until the serve loop is accepting connections on `port` (it binds well under a second).
fn wait_ready(port: u16) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(Server(Mutex::new(None)))
        .setup(|app| {
            let port = free_port();

            // Launch the bundled scanner as a sidecar, serving the GUI on loopback only.
            let (_rx, child) = app
                .shell()
                .sidecar("sdirstat")
                .expect("sdirstat sidecar configured in tauri.conf.json (externalBin)")
                .args(["serve", "--port", &port.to_string()])
                .spawn()
                .expect("spawn `sdirstat serve`");
            app.state::<Server>().0.lock().unwrap().replace(child);

            wait_ready(port);

            let url = format!("http://127.0.0.1:{port}/");
            WebviewWindowBuilder::new(app, "main", WebviewUrl::External(url.parse().unwrap()))
                .title("sdirstat — disk usage")
                .inner_size(1180.0, 760.0)
                .min_inner_size(720.0, 480.0)
                .build()?;
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("build sdirstat desktop")
        .run(|handle, event| {
            // Kill the serve sidecar whenever the app exits — closing the window, quitting, or any
            // graceful shutdown. (A SIGKILL of this process can still orphan the child; that's the
            // one path Tauri can't intercept.)
            if let tauri::RunEvent::Exit = event {
                if let Some(child) = handle.state::<Server>().0.lock().unwrap().take() {
                    let _ = child.kill();
                }
            }
        });
}
