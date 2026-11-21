//! The `pages` quality check: start the built app and request every page.
//!
//! A project can compile and still fail when a page is opened (for example a
//! Server Component that passes a function to a Client Component). This check
//! starts the production server (`next start`) in the current directory,
//! requests each static route of the `app` directory, and counts the answers
//! with status 200. It runs after the `build` check, for both apps.

use crate::arms::shell;
use crate::launch::{free_port, kill_tree, port_open};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

/// The static routes below `dir` (an `app` directory). Routes with a dynamic
/// segment are left out: the check has no parameter value for them.
pub(crate) fn routes(dir: &Path, prefix: &str, found: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name.starts_with('[') || name.starts_with('_') || name.starts_with('@') {
                continue;
            }
            // A route group `(name)` is not part of the URL.
            let next = if name.starts_with('(') {
                prefix.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            routes(&path, &next, found);
        } else if matches!(
            name.as_str(),
            "page.tsx" | "page.jsx" | "page.ts" | "page.js"
        ) {
            found.push(if prefix.is_empty() {
                "/".to_string()
            } else {
                prefix.to_string()
            });
        }
    }
}

/// The HTTP status of `GET route` on the local server, if it answers.
pub fn status(port: u16, route: &str) -> Option<u16> {
    let targets: [SocketAddr; 2] = [
        ([127, 0, 0, 1], port).into(),
        (std::net::Ipv6Addr::LOCALHOST, port).into(),
    ];
    let mut stream = targets
        .iter()
        .find_map(|target| TcpStream::connect_timeout(target, Duration::from_secs(2)).ok())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok()?;
    stream
        .write_all(
            format!("GET {route} HTTP/1.0\r\nHost: localhost:{port}\r\nAccept: text/html\r\n\r\n")
                .as_bytes(),
        )
        .ok()?;
    let mut head = [0u8; 64];
    let count = stream.read(&mut head).ok()?;
    // "HTTP/1.1 200 OK"
    String::from_utf8_lossy(&head[..count])
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Run the check in the current directory. Returns the process exit code.
pub fn run() -> i32 {
    let app = ["app", "src/app"]
        .iter()
        .map(Path::new)
        .find(|path| path.is_dir());
    let mut found = Vec::new();
    if let Some(app) = app {
        routes(app, "", &mut found);
    }
    found.sort();
    if found.is_empty() {
        println!("0 passed | 1 failed (no pages found)");
        return 1;
    }
    if !Path::new(".next").join("BUILD_ID").is_file() {
        println!(
            "0 passed | {} failed (no production build: the build check must pass first)",
            found.len()
        );
        return 1;
    }
    let Some(port) = free_port() else {
        println!("0 passed | 1 failed (no free port)");
        return 1;
    };
    let child = shell(&format!("npx next start -p {port}"))
        .env("NEXT_TELEMETRY_DISABLED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else {
        println!("0 passed | 1 failed (the server did not start)");
        return 1;
    };
    let started = Instant::now();
    let up = loop {
        if port_open(port) {
            break true;
        }
        if matches!(child.try_wait(), Ok(Some(_))) || started.elapsed() > Duration::from_secs(60) {
            break false;
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    let mut failed = Vec::new();
    if up {
        for route in &found {
            match status(port, route) {
                Some(200) => {}
                Some(code) => failed.push(format!("FAILED {route} (status {code})")),
                None => failed.push(format!("FAILED {route} (no answer)")),
            }
        }
    }
    kill_tree(child.id());
    let _ = child.wait();
    if !up {
        println!(
            "0 passed | {} failed (the server did not start: no build?)",
            found.len()
        );
        return 1;
    }
    for line in &failed {
        println!("{line}");
    }
    let passed = found.len() - failed.len();
    if failed.is_empty() {
        println!("{passed} passed ({} pages)", found.len());
        0
    } else {
        println!(
            "{passed} passed | {} failed ({} pages)",
            failed.len(),
            found.len()
        );
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_skip_dynamic_segments_and_route_groups() {
        let root = std::env::temp_dir().join(format!("showdown-pages-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["", "board", "tasks/[id]", "(main)/team", "api"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in [
            "page.tsx",
            "board/page.tsx",
            "tasks/[id]/page.tsx",
            "(main)/team/page.tsx",
            "api/route.ts",
        ] {
            std::fs::write(root.join(file), "").unwrap();
        }
        let mut found = Vec::new();
        routes(&root, "", &mut found);
        found.sort();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(found, vec!["/", "/board", "/team"]);
    }
}
