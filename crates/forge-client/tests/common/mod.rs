#![allow(clippy::expect_used, clippy::panic)] // tests may fail loudly
//! Test harness: a scripted HTTP/1.1 server on loopback and an in-process
//! `gstn` runner. All data is synthetic.
#![allow(dead_code)]

use forge_client::app::{run, Io};
use std::collections::BTreeMap;
use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A synthetic token. Tests assert it never appears in any output.
pub const TOKEN: &str = "gst_SYNTHETIC_0123456789abcdefSECRET";

#[derive(Debug, Clone)]
pub struct Req {
    pub method: String,
    pub target: String,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

pub struct Resp {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Resp {
    pub fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: body.into(),
        }
    }
}

pub struct Mock {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Req>>>,
}

impl Mock {
    pub fn start(handler: impl Fn(&Req) -> Resp + Send + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let url = format!("http://{}", listener.local_addr().expect("addr"));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        thread::spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut s) = conn else { continue };
                if let Some(req) = read_req(&mut s) {
                    let resp = handler(&req);
                    seen.lock().expect("lock").push(req);
                    write_resp(&mut s, &resp);
                }
            }
        });
        Self { url, requests }
    }

    pub fn seen(&self) -> Vec<Req> {
        self.requests.lock().expect("lock").clone()
    }
}

fn read_req(s: &mut TcpStream) -> Option<Req> {
    s.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.lines();
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: BTreeMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = s.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
    Some(Req {
        method,
        target,
        headers,
        body,
    })
}

fn write_resp(s: &mut TcpStream, r: &Resp) {
    let mut out = format!(
        "HTTP/1.1 {} X\r\nContent-Length: {}\r\nConnection: close\r\n",
        r.status,
        r.body.len()
    );
    for (k, v) in &r.headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    out.push_str(&r.body);
    let _ = s.write_all(out.as_bytes());
}

pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn all(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout)
            .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {}", self.stdout))
    }
    pub fn err_json(&self) -> serde_json::Value {
        serde_json::from_str(self.stderr.trim())
            .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {}", self.stderr))
    }
}

/// A scratch config directory plus the environment every test uses.
pub struct Sandbox {
    pub dir: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        Self {
            dir: tempfile::tempdir().expect("tempdir"),
        }
    }

    pub fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("cfg")
    }

    pub fn run(&self, host: &str, args: &[&str], stdin: &str) -> Out {
        self.run_env(host, args, stdin, &[])
    }

    pub fn run_env(&self, host: &str, args: &[&str], stdin: &str, extra: &[(&str, &str)]) -> Out {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        env.insert(
            "GSTN_CONFIG_DIR".into(),
            self.path().to_string_lossy().to_string(),
        );
        env.insert("GSTN_CREDENTIAL_STORE".into(), "file".into());
        if !host.is_empty() {
            env.insert("GSTN_HOST".into(), host.into());
        }
        for (k, v) in extra {
            env.insert((*k).into(), (*v).into());
        }
        let envf = move |k: &str| env.get(k).cloned();
        let mut argv = vec!["gstn".to_string()];
        argv.extend(args.iter().map(|a| (*a).to_string()));
        let mut input = Cursor::new(stdin.as_bytes().to_vec());
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut input,
                stdout: &mut out,
                stderr: &mut err,
                stdin_tty: false,
                stdout_tty: false,
            };
            run(&argv, &envf, "/usr/local/bin/gstn", &mut io)
        };
        Out {
            code,
            stdout: String::from_utf8_lossy(&out).to_string(),
            stderr: String::from_utf8_lossy(&err).to_string(),
        }
    }
}

pub const USER_JSON: &str = r#"{"id":7,"username":"alice","display_name":"Alice A","is_admin":false,"is_agent":false,"token_scopes":["read_api","read_repository"]}"#;

/// A server that knows one valid token.
pub fn user_server() -> Mock {
    Mock::start(|r| {
        let auth = r.headers.get("authorization").cloned().unwrap_or_default();
        if r.target == "/api/v1/user" {
            if auth == format!("Bearer {TOKEN}") {
                Resp::json(200, USER_JSON)
            } else {
                Resp::json(
                    401,
                    r#"{"error_code":"FG-AUTH-0001","user_message":"認証に失敗しました。","request_id":"req-401"}"#,
                )
            }
        } else {
            Resp::json(
                404,
                r#"{"error_code":"FG-REPO-0005","user_message":"見つかりません。","request_id":"req-404"}"#,
            )
        }
    })
}
