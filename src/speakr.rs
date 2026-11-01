//! Upload finished recordings to a Speakr server (`POST /api/v1/recordings/upload`, Bearer token).
//! Uses the system `curl` (TLS + streamed multipart, no extra crates); the token goes in through
//! stdin, never argv. The token itself lives in the login Keychain, not in the settings file.

use std::{
    io::Write,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};

const KEYCHAIN_SERVICE: &str = "recordr.speakr";

/// A running upload. Poll it each frame; drop or `cancel` to kill it.
pub struct Upload {
    pub path: PathBuf,
    child: Child,
}

impl Upload {
    pub fn start(server: &str, token: &str, path: &Path) -> Result<Self, String> {
        let url = format!(
            "{}/api/v1/recordings/upload",
            server.trim().trim_end_matches('/')
        );
        let title = path.file_stem().unwrap_or_default().to_string_lossy();
        let quoted = path
            .to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let mut child = Command::new("/usr/bin/curl")
            .args(["-sS", "--fail-with-body", "-K", "-"])
            .arg("-F")
            .arg(format!("file=@\"{quoted}\""))
            .arg("--form-string")
            .arg(format!("title={title}"))
            .arg(&url)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("curl: {e}"))?;
        let token = token.trim().replace(['"', '\\', '\n'], "");
        let config = format!("header = \"Authorization: Bearer {token}\"\n");
        // Dropping stdin after the write closes it, so curl starts.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(config.as_bytes());
        }
        Ok(Upload {
            path: path.to_path_buf(),
            child,
        })
    }

    /// None while running; then Ok or a short error.
    // ponytail: output is read after exit; fine for Speakr's small JSON replies (< pipe buffer).
    pub fn poll(&mut self) -> Option<Result<(), String>> {
        let status = match self.child.try_wait() {
            Ok(None) => return None,
            Ok(Some(s)) => s,
            Err(e) => return Some(Err(e.to_string())),
        };
        if status.success() {
            return Some(Ok(()));
        }
        let read = |r: Option<&mut dyn std::io::Read>| {
            let mut s = String::new();
            if let Some(r) = r {
                let _ = r.read_to_string(&mut s);
            }
            s
        };
        let body = read(self.child.stdout.as_mut().map(|r| r as _));
        let err = read(self.child.stderr.as_mut().map(|r| r as _));
        let msg = format!("{} {}", err.trim(), body.trim());
        let msg: String = msg.trim().chars().take(300).collect();
        Some(Err(if msg.is_empty() {
            format!("curl failed ({status})")
        } else {
            msg
        }))
    }

    pub fn cancel(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn load_token() -> String {
    Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

// ponytail: `security -w <token>` puts the token on argv for an instant (same-user `ps` only);
// the Security framework API avoids that if it ever matters.
pub fn save_token(token: &str) {
    let mut cmd = Command::new("/usr/bin/security");
    if token.is_empty() {
        cmd.args(["delete-generic-password", "-s", KEYCHAIN_SERVICE]);
    } else {
        cmd.args([
            "add-generic-password",
            "-U",
            "-s",
            KEYCHAIN_SERVICE,
            "-a",
            "speakr",
            "-w",
        ])
        .arg(token);
    }
    let _ = cmd.output();
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};

    /// Upload to a one-shot local HTTP server and check what curl sent.
    fn upload_to(reply: &'static str) -> (Result<(), String>, String) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            let mut len = 0;
            loop {
                let mut line = String::new();
                r.read_line(&mut line).unwrap();
                if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap();
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; len];
            r.read_exact(&mut body).unwrap();
            s.write_all(reply.as_bytes()).unwrap();
            head + &String::from_utf8_lossy(&body)
        });
        let dir = std::env::temp_dir().join("recordr-speakr-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("2026-01-01-10.00.00-test.wav");
        std::fs::write(&path, b"RIFFfake").unwrap();
        let mut up =
            super::Upload::start(&format!("http://127.0.0.1:{port}/"), "tok123", &path).unwrap();
        let result = loop {
            if let Some(r) = up.poll() {
                break r;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        (result, server.join().unwrap())
    }

    #[test]
    fn uploads_with_token_and_title() {
        let ok = "HTTP/1.1 202 Accepted\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
        let (result, req) = upload_to(ok);
        assert_eq!(result, Ok(()));
        assert!(req.starts_with("POST /api/v1/recordings/upload "), "{req}");
        assert!(req.contains("Authorization: Bearer tok123"), "{req}");
        assert!(
            req.contains("name=\"title\"\r\n\r\n2026-01-01-10.00.00-test"),
            "{req}"
        );
        assert!(
            req.contains("filename=\"2026-01-01-10.00.00-test.wav\""),
            "{req}"
        );
        assert!(req.contains("RIFFfake"), "{req}");
    }

    #[test]
    fn reports_server_errors() {
        let bad = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 15\r\nConnection: close\r\n\r\n{\"error\":\"bad\"}";
        let (result, _) = upload_to(bad);
        let err = result.unwrap_err();
        assert!(err.contains("401") && err.contains("bad"), "{err}");
    }
}
