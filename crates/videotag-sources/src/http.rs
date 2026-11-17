//! Minimal HTTP over the system `curl` binary (same approach as
//! MusicTag's sources crate: curl ships with macOS, Windows 10+ and
//! Linux, so this crate needs no TLS stack of its own).

use crate::{Result, SourceError};
use std::process::{Command, Stdio};

pub const USER_AGENT: &str = concat!("VideoTag/", env!("CARGO_PKG_VERSION"));

const STATUS_MARKER: &str = "__VIDEOTAG_HTTP_STATUS__:";

/// HTTP verb for [`send`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
}

/// One HTTP request: a URL, optional headers and JSON body, and an
/// optional User-Agent override (Bangumi rejects generic agents).
pub struct Request<'a> {
    pub method: Method,
    pub url: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: Option<&'a str>,
    pub user_agent: Option<&'a str>,
}

impl<'a> Request<'a> {
    pub fn get(url: &'a str) -> Self {
        Self {
            method: Method::Get,
            url,
            headers: &[],
            body: None,
            user_agent: None,
        }
    }

    pub fn post_json(url: &'a str, body: &'a str) -> Self {
        Self {
            method: Method::Post,
            url,
            headers: &[("Content-Type", "application/json")],
            body: Some(body),
            user_agent: None,
        }
    }

    pub fn with_headers(mut self, headers: &'a [(&'a str, &'a str)]) -> Self {
        self.headers = headers;
        self
    }

    pub fn with_user_agent(mut self, user_agent: &'a str) -> Self {
        self.user_agent = Some(user_agent);
        self
    }
}

/// Send a request, returning the HTTP status code and the body as
/// text.
///
/// The status is appended to curl's stdout with a marker via `-w`,
/// so callers can tell a real 404 apart from a successful empty
/// response.
pub fn send(req: &Request) -> Result<(u32, String)> {
    let mut args: Vec<String> = vec![
        "-sS".into(),
        "-m".into(),
        "25".into(),
        "-A".into(),
        req.user_agent.unwrap_or(USER_AGENT).to_string(),
        "-w".into(),
        format!("\n{STATUS_MARKER}%{{http_code}}"),
    ];
    if req.method == Method::Post {
        args.push("-X".into());
        args.push("POST".into());
    }
    for (name, value) in req.headers {
        args.push("-H".into());
        args.push(format!("{name}: {value}"));
    }
    if let Some(body) = req.body {
        args.push("--data-raw".into());
        args.push(body.to_string());
    }
    args.push(req.url.to_string());
    let mut cmd = Command::new("curl");
    cmd.args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Windows: curl is a console program — without this flag every
    // request flashes a console window from the GUI app.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd
        .output()
        .map_err(|e| SourceError::Http(format!("cannot run curl: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(SourceError::Http(format!(
            "curl failed ({}) {stderr}",
            out.status
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    match text.rfind(STATUS_MARKER) {
        Some(pos) => {
            let status: u32 = text[pos + STATUS_MARKER.len()..]
                .trim()
                .parse()
                .map_err(|_| SourceError::Http("cannot parse curl status".to_string()))?;
            let mut body = text[..pos].to_string();
            if body.ends_with('\n') {
                body.pop();
            }
            Ok((status, body))
        }
        None => Err(SourceError::Http(
            "curl output missing status marker".to_string(),
        )),
    }
}

/// GET a URL, returning the HTTP status code and the body as text.
pub fn get_status_text(url: &str) -> Result<(u32, String)> {
    send(&Request::get(url))
}
