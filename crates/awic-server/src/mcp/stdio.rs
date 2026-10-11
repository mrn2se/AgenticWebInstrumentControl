//! `awic mcp-stdio`: a stdio↔HTTP proxy in front of a running server's
//! `/mcp` endpoint (SPECIFICATION.md §8.1), for MCP clients that only speak
//! the stdio transport — `podman exec -i awic awic mcp-stdio`, or an MCP
//! client config's `"command"` entry pointed at this binary directly when
//! run on the host.
//!
//! MCP's stdio transport (the spec's own wording) is newline-delimited
//! JSON-RPC: one message per line, no embedded newlines, nothing else on
//! the wire. This shim reads one line from stdin, POSTs it verbatim as the
//! body of an HTTP request to the target `/mcp` URL, and writes the
//! response body back to stdout as one line — carrying the `Mcp-Session-Id`
//! the server hands back from `initialize` on every request after that, so
//! the whole stdio conversation rides one HTTP-level MCP session.
//!
//! The HTTP client here is hand-rolled rather than pulling in `reqwest` (or
//! any other HTTP client crate): the target is always this project's own
//! `/mcp` route, always plain HTTP on localhost/a container network, always
//! a small JSON body with an exact `Content-Length` — axum never chunks a
//! response this small — so a `Content-Length`-bounded read over a
//! `tokio::net::TcpStream` is both correct and a great deal smaller than a
//! general-purpose client.

use std::io;

use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

const DEFAULT_URL: &str = "http://127.0.0.1:8080/mcp";

/// Runs the shim until stdin closes. `args` is everything after
/// `mcp-stdio` on the command line: `--url <url>` and `--token <token>`,
/// each falling back to `AWIC_MCP_URL`/`AWIC_MCP_TOKEN` if omitted, and
/// finally to [`DEFAULT_URL`]/no token.
pub async fn run(args: &[String]) -> anyhow::Result<()> {
    let mut url = std::env::var("AWIC_MCP_URL").unwrap_or_else(|_| DEFAULT_URL.to_string());
    let mut token = std::env::var("AWIC_MCP_TOKEN").ok();

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--url" => {
                i += 1;
                url = args
                    .get(i)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("--url requires a value"))?;
            }
            "--token" => {
                i += 1;
                token = Some(
                    args.get(i)
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("--token requires a value"))?,
                );
            }
            other => return Err(anyhow::anyhow!("unknown mcp-stdio argument: {other:?}")),
        }
        i += 1;
    }

    let target = HttpTarget::parse(&url)?;
    let mut session_id: Option<String> = None;

    let stdin = tokio::io::stdin();
    let mut lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let request_id = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .and_then(|v| v.get("id").cloned());

        match post(&target, token.as_deref(), session_id.as_deref(), line).await {
            Ok(reply) => {
                if let Some(new_session) = reply.session_id {
                    session_id = Some(new_session);
                }
                if reply.status == 401 {
                    respond_error(
                        &mut stdout,
                        request_id,
                        "unauthorized: missing or invalid bearer token",
                    )
                    .await?;
                } else if !reply.body.trim().is_empty() {
                    write_line(&mut stdout, reply.body.trim()).await?;
                }
                // A 202/empty body (a notification's ack) gets no stdout
                // line, matching what the MCP stdio transport expects for a
                // message that carried no `id`.
            }
            Err(err) => {
                respond_error(&mut stdout, request_id, &format!("mcp-stdio: {err}")).await?;
            }
        }
    }

    Ok(())
}

async fn respond_error(
    stdout: &mut tokio::io::Stdout,
    request_id: Option<serde_json::Value>,
    message: &str,
) -> anyhow::Result<()> {
    match request_id {
        // A notification never gets a response, error or otherwise.
        None => {
            eprintln!("{message}");
            Ok(())
        }
        Some(id) => {
            let line = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32000, "message": message },
            })
            .to_string();
            write_line(stdout, &line).await
        }
    }
}

async fn write_line(stdout: &mut tokio::io::Stdout, line: &str) -> anyhow::Result<()> {
    stdout.write_all(line.as_bytes()).await?;
    stdout.write_all(b"\n").await?;
    stdout.flush().await?;
    Ok(())
}

struct HttpTarget {
    host: String,
    port: u16,
    path: String,
}

impl HttpTarget {
    fn parse(url: &str) -> anyhow::Result<Self> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| anyhow::anyhow!("mcp-stdio only supports http:// URLs, got {url:?}"))?;
        let (authority, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (
                h.to_string(),
                p.parse()
                    .map_err(|_| anyhow::anyhow!("bad port in {url:?}"))?,
            ),
            None => (authority.to_string(), 80),
        };
        Ok(Self {
            host,
            port,
            path: if path.is_empty() {
                "/".to_string()
            } else {
                path.to_string()
            },
        })
    }
}

struct HttpReply {
    status: u16,
    session_id: Option<String>,
    body: String,
}

/// One request/response over a fresh connection (`Connection: close` — this
/// shim's request volume is one MCP tool call at a time, so paying a new
/// TCP handshake per call is a fine trade for never having to reason about
/// HTTP/1.1 keep-alive or chunked framing).
async fn post(
    target: &HttpTarget,
    token: Option<&str>,
    session_id: Option<&str>,
    body: &str,
) -> anyhow::Result<HttpReply> {
    let stream = TcpStream::connect((target.host.as_str(), target.port)).await?;
    let (read_half, mut write_half) = stream.into_split();

    let mut request = format!(
        "POST {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Content-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Length: {}\r\n",
        target.path,
        target.host,
        body.len(),
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(session_id) = session_id {
        request.push_str(&format!("{SESSION_HEADER}: {session_id}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");

    write_half.write_all(request.as_bytes()).await?;
    write_half.write_all(body.as_bytes()).await?;
    write_half.flush().await?;

    let mut reader = BufReader::new(read_half);
    let mut status_line = String::new();
    if reader.read_line(&mut status_line).await? == 0 {
        return Err(
            io::Error::new(io::ErrorKind::UnexpectedEof, "server closed the connection").into(),
        );
    }
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut content_length = 0usize;
    let mut new_session_id = None;
    loop {
        let mut header_line = String::new();
        let n = reader.read_line(&mut header_line).await?;
        if n == 0 || header_line.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header_line.split_once(':') {
            let name = name.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            } else if name == SESSION_HEADER.to_ascii_lowercase() {
                new_session_id = Some(value);
            }
        }
    }

    let mut body_buf = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body_buf).await?;
    }

    Ok(HttpReply {
        status,
        session_id: new_session_id,
        body: String::from_utf8_lossy(&body_buf).into_owned(),
    })
}

const SESSION_HEADER: &str = super::route::SESSION_HEADER;
