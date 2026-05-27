//! Minimal dev server.
//!
//! One thread, one request at a time. No watch, no live reload, no
//! concurrent requests. The goal is "open a page in a browser, see CSS
//! work, iterate on typst output" — anything beyond that is a later
//! revision.
//!
//! Routing:
//!
//! - `GET /` — placeholder index listing the slugs found under
//!   `content/`. Stand-in until twyla has a real home page.
//! - `GET /<slug>/` or `GET /<slug>` — if `content/<slug>.typ` exists,
//!   compile it via [`render_slug`] and serve the HTML.
//! - Everything else — served from `static/` first, then `content/`
//!   as a fallback (zola's colocated-asset convention; goes away when
//!   twyla has a real asset model).

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::render::{render_slug, scan_pages};

pub struct Serve {
    pub site_root: PathBuf,
    pub addr: SocketAddr,
}

pub fn run(serve: Serve) -> io::Result<()> {
    let listener = TcpListener::bind(serve.addr)?;
    let local = listener.local_addr()?;
    eprintln!(
        "twyla serve: http://{}  (site root: {})",
        local,
        serve.site_root.display(),
    );
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        if let Err(e) = handle(&serve, stream) {
            eprintln!("connection error: {e}");
        }
    }
    Ok(())
}

fn handle(serve: &Serve, mut stream: TcpStream) -> io::Result<()> {
    let request_line = read_request(&stream)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let start = Instant::now();
    let response = if method != "GET" {
        Response::text(405, "method not allowed")
    } else {
        dispatch(serve, &path)
    };
    let elapsed = start.elapsed();
    eprintln!("{method:>4} {path} → {} ({elapsed:.0?})", response.status);

    write_response(&mut stream, &response)
}

/// Read just enough of the request to know the method and path. We
/// don't care about headers or body for any handler today; drain them
/// so the client sees a well-formed exchange but the values go to
/// `/dev/null`.
fn read_request(stream: &TcpStream) -> io::Result<String> {
    let mut reader = BufReader::new(stream);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    loop {
        let mut h = String::new();
        let n = reader.read_line(&mut h)?;
        if n == 0 || h == "\r\n" || h == "\n" {
            break;
        }
    }
    Ok(request_line.trim_end().to_string())
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn text(status: u16, msg: &str) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: msg.as_bytes().to_vec(),
        }
    }
    fn html(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
        }
    }
    fn bytes(status: u16, content_type: &'static str, body: Vec<u8>) -> Self {
        Self { status, content_type, body }
    }
}

fn write_response(w: &mut impl Write, r: &Response) -> io::Result<()> {
    write!(w, "HTTP/1.1 {} {}\r\n", r.status, status_phrase(r.status))?;
    write!(w, "Content-Type: {}\r\n", r.content_type)?;
    write!(w, "Content-Length: {}\r\n", r.body.len())?;
    w.write_all(b"Connection: close\r\n\r\n")?;
    w.write_all(&r.body)
}

fn status_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "",
    }
}

fn dispatch(serve: &Serve, path: &str) -> Response {
    let path = path.split('?').next().unwrap_or(path);

    if path == "/" {
        return render_index(serve);
    }

    // Page route: single path component, no extension, and a matching
    // `content/<slug>.typ` exists. Anything else falls through to static.
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if !trimmed.is_empty() && !trimmed.contains('/') {
        let typ_path =
            serve.site_root.join("content").join(format!("{trimmed}.typ"));
        if typ_path.is_file() {
            return render_page(serve, trimmed);
        }
    }

    serve_static(&serve.site_root, path)
}

fn render_index(serve: &Serve) -> Response {
    let slugs = match scan_pages(&serve.site_root) {
        Ok(s) => s,
        Err(e) => {
            return Response::html(500, format!("<pre>{}</pre>", html_escape(&e)))
        }
    };
    let mut out = String::new();
    out.push_str(
        "<!doctype html>\n\
         <html><head><meta charset=\"utf-8\"><title>twyla dev</title>\n\
         <style>body{font-family:monospace;padding:2rem;line-height:1.6}\
         a{color:#06c}</style>\n\
         </head><body>\n",
    );
    out.push_str("<h1>twyla dev</h1>\n");
    out.push_str("<p>placeholder index — home page not yet ported.</p>\n");
    out.push_str("<ul>\n");
    for s in &slugs {
        out.push_str(&format!(
            "<li><a href=\"/{slug}/\">/{slug}/</a></li>\n",
            slug = html_escape(s),
        ));
    }
    out.push_str("</ul>\n</body></html>\n");
    Response::html(200, out)
}

fn render_page(serve: &Serve, slug: &str) -> Response {
    match render_slug(&serve.site_root, slug) {
        Ok(doc) => Response::html(200, doc.html),
        Err(e) => {
            eprintln!("render error ({slug}):\n{e}");
            let body = format!(
                "<!doctype html><html><body><h1>twyla: render error</h1>\
                 <pre style=\"white-space:pre-wrap\">{}</pre></body></html>",
                html_escape(&e.to_string()),
            );
            Response::html(500, body)
        }
    }
}

fn serve_static(site_root: &Path, request_path: &str) -> Response {
    let rel = request_path.trim_start_matches('/');
    // Reject path traversal. Bare `..` segments only — anything embedded
    // in a name is a normal char.
    if rel.split('/').any(|c| c == "..") {
        return Response::text(404, "not found");
    }

    let static_path = site_root.join("static").join(rel);
    if let Ok(body) = std::fs::read(&static_path) {
        return Response::bytes(200, mime_for(&static_path), body);
    }

    // Zola colocates assets in `content/` (e.g., `content/foo.svg` →
    // `/foo.svg`). Mirror that for porting; revisit once twyla has a
    // real asset model.
    let content_path = site_root.join("content").join(rel);
    let is_typ =
        content_path.extension().and_then(|e| e.to_str()) == Some("typ");
    if !is_typ {
        if let Ok(body) = std::fs::read(&content_path) {
            return Response::bytes(200, mime_for(&content_path), body);
        }
    }

    Response::text(404, "not found")
}

fn mime_for(p: &Path) -> &'static str {
    match p.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "ico" => "image/x-icon",
        "pdf" => "application/pdf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
