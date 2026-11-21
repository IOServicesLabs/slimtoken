//! Starts the two apps so a person can look at them: installs dependencies
//! if necessary, starts each app's dev server on a free port, and opens both
//! in the browser. The servers stop when the benchmark exits.

use crate::arms::shell;
use crate::events::Arm;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
enum Status {
    Installing,
    Starting,
    Ready(String),
    Failed(String),
}

pub struct Launcher {
    /// Dev-server command; `{port}` is replaced by a free port.
    template: String,
    status: Arc<Mutex<[Option<Status>; 2]>>,
    pids: Arc<Mutex<Vec<u32>>>,
}

pub fn free_port() -> Option<u16> {
    TcpListener::bind("127.0.0.1:0")
        .ok()
        .and_then(|listener| listener.local_addr().ok())
        .map(|address| address.port())
}

pub fn port_open(port: u16) -> bool {
    let targets: [SocketAddr; 2] = [
        ([127, 0, 0, 1], port).into(),
        (std::net::Ipv6Addr::LOCALHOST, port).into(),
    ];
    targets
        .iter()
        .any(|target| TcpStream::connect_timeout(target, Duration::from_millis(300)).is_ok())
}

fn open_browser(url: &str) {
    let mut process = if cfg!(windows) {
        let mut process = std::process::Command::new("cmd");
        process.args(["/C", "start", "", url]);
        process
    } else if cfg!(target_os = "macos") {
        let mut process = std::process::Command::new("open");
        process.arg(url);
        process
    } else {
        let mut process = std::process::Command::new("xdg-open");
        process.arg(url);
        process
    };
    let _ = process
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// The desktop without the task bar, in the units the browser's window
/// flags take: (left, top, width, height).
#[cfg(windows)]
fn work_area() -> Option<(i32, i32, i32, i32)> {
    #[repr(C)]
    struct Rect {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
    }
    #[link(name = "user32")]
    extern "system" {
        fn SystemParametersInfoW(action: u32, param: u32, out: *mut Rect, flags: u32) -> i32;
    }
    const SPI_GETWORKAREA: u32 = 0x0030;
    let mut rect = Rect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `rect` is a valid RECT for the call to fill.
    let ok = unsafe { SystemParametersInfoW(SPI_GETWORKAREA, 0, &mut rect, 0) };
    (ok != 0 && rect.right > rect.left && rect.bottom > rect.top).then(|| {
        (
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        )
    })
}

#[cfg(not(windows))]
fn work_area() -> Option<(i32, i32, i32, i32)> {
    None
}

/// The window of one arm on a desktop of `area`: the single stream takes the
/// left half and SlimToken the right half, as on the benchmark screen.
fn half(area: (i32, i32, i32, i32), arm: Arm) -> (i32, i32, i32, i32) {
    let (left, top, width, height) = area;
    let half = width / 2;
    match arm {
        Arm::Single => (left, top, half, height),
        Arm::Slim => (left + half, top, width - half, height),
    }
}

/// A Chromium browser that takes `--window-position` and `--window-size`.
fn tiling_browser() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("SHOWDOWN_BROWSER") {
        return Some(PathBuf::from(path));
    }
    let roots = ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"];
    let tails = [
        "Microsoft/Edge/Application/msedge.exe",
        "Google/Chrome/Application/chrome.exe",
    ];
    tails
        .iter()
        .flat_map(|tail| roots.iter().map(move |root| (root, tail)))
        .filter_map(|(root, tail)| {
            std::env::var_os(root).map(|root| PathBuf::from(root).join(tail))
        })
        .find(|path| path.is_file())
}

/// Open the app of `arm` in its own window on its half of the screen (no
/// tabs or address bar: the app alone). `SHOWDOWN_TILE=0` turns this off.
/// Each arm has its own browser profile under the temp directory: a browser
/// that is already open ignores the window flags, and the two apps must not
/// share their stored data. `false`: not done; open the default browser.
fn open_tiled(url: &str, arm: Arm) -> bool {
    if std::env::var("SHOWDOWN_TILE").is_ok_and(|value| value == "0") {
        return false;
    }
    let (Some(area), Some(browser)) = (work_area(), tiling_browser()) else {
        return false;
    };
    let (left, top, width, height) = half(area, arm);
    let profile = std::env::temp_dir().join(format!("showdown-view-{}", arm.dir_name()));
    std::process::Command::new(browser)
        .arg(format!("--app={url}"))
        .arg(format!("--window-position={left},{top}"))
        .arg(format!("--window-size={width},{height}"))
        .arg(format!("--user-data-dir={}", profile.display()))
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-sync",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

impl Launcher {
    pub fn new(template: &str) -> Self {
        Self {
            template: template.to_string(),
            status: Arc::new(Mutex::new([None, None])),
            pids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn started(&self, arm: Arm) -> bool {
        self.status.lock().expect("launch status")[arm.index()].is_some()
    }

    /// Start one app in the background. Does nothing if it already started.
    pub fn start(&self, arm: Arm, dir: PathBuf) {
        if self.started(arm) {
            return;
        }
        let set = {
            let status = Arc::clone(&self.status);
            move |value: Status| {
                status.lock().expect("launch status")[arm.index()] = Some(value);
            }
        };
        set(Status::Starting);
        let template = self.template.clone();
        let pids = Arc::clone(&self.pids);
        std::thread::spawn(move || {
            if !dir.join("package.json").is_file() {
                set(Status::Failed("no package.json in the app".to_string()));
                return;
            }
            if !dir.join("node_modules").is_dir() {
                set(Status::Installing);
                let installed = shell("npm install --no-audit --no-fund")
                    .current_dir(&dir)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
                if !installed.is_ok_and(|status| status.success()) {
                    set(Status::Failed("npm install failed".to_string()));
                    return;
                }
            }
            let Some(port) = free_port() else {
                set(Status::Failed("no free port".to_string()));
                return;
            };
            set(Status::Starting);
            let command = template.replace("{port}", &port.to_string());
            let child = shell(&command)
                .current_dir(&dir)
                .env("NEXT_TELEMETRY_DISABLED", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            let mut child = match child {
                Ok(child) => child,
                Err(error) => {
                    set(Status::Failed(format!("could not start: {error}")));
                    return;
                }
            };
            pids.lock().expect("launch pids").push(child.id());
            let started = Instant::now();
            loop {
                if let Ok(Some(_)) = child.try_wait() {
                    set(Status::Failed("the dev server stopped".to_string()));
                    return;
                }
                if port_open(port) {
                    break;
                }
                if started.elapsed() > Duration::from_secs(120) {
                    set(Status::Failed("the dev server did not answer".to_string()));
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            let url = format!("http://localhost:{port}");
            if !open_tiled(&url, arm) {
                open_browser(&url);
            }
            set(Status::Ready(url));
            let _ = child.wait();
        });
    }

    /// Status text for the screen, by arm index: (text, ready).
    pub fn view(&self) -> [Option<(String, bool)>; 2] {
        let status = self.status.lock().expect("launch status");
        let text = |value: &Option<Status>| {
            value.as_ref().map(|status| match status {
                Status::Installing => ("installing dependencies…".to_string(), false),
                Status::Starting => ("starting the app…".to_string(), false),
                Status::Ready(url) => (url.clone(), true),
                Status::Failed(reason) => (format!("launch failed: {reason}"), false),
            })
        };
        [text(&status[0]), text(&status[1])]
    }
}

/// Stop a process and the processes it started (a dev server is a tree:
/// shell, npm, node).
pub fn kill_tree(pid: u32) {
    let mut process = if cfg!(windows) {
        let mut process = std::process::Command::new("taskkill");
        process.args(["/T", "/F", "/PID", &pid.to_string()]);
        process
    } else {
        let mut process = std::process::Command::new("sh");
        process.args([
            "-c",
            &format!("pkill -TERM -P {pid} 2>/dev/null; kill -TERM {pid} 2>/dev/null"),
        ]);
        process
    };
    let _ = process
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

impl Drop for Launcher {
    fn drop(&mut self) {
        for pid in self.pids.lock().expect("launch pids").drain(..) {
            kill_tree(pid);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_halves_cover_the_work_area_side_by_side() {
        // An odd width and a task bar on the left.
        let area = (60, 0, 1861, 1040);
        assert_eq!(half(area, Arm::Single), (60, 0, 930, 1040));
        assert_eq!(half(area, Arm::Slim), (990, 0, 931, 1040));
    }
}
