//! Minimal dev server with hot recompile.
//!
//! One thread for accepts, one for the file watcher. Requests are served
//! sequentially out of the last successful compile; the watcher rebuilds
//! the bundle in the background when typst sources, templates, or
//! `content/` directory contents change.
//!
//! Architecture
//! ------------
//!
//! - A single persistent [`RenderWorld`] (typst compile state + comemo
//!   cache) lives behind `Arc<Mutex<..>>`. It is locked only by the
//!   watcher thread — request handlers never compile.
//! - The most recent compile result lives in `Arc<Mutex<LastOutput>>`.
//!   Request handlers lock it briefly to clone the matching doc's HTML.
//! - The watcher thread loops: collect `world.dependencies()` + the
//!   `content/` directory → `watcher.update(..)` → `wait()` → on event:
//!   `refresh_main()` (rescan content/ + regen virtual main),
//!   `reset()` (mark FileStore slots stale), `comemo::evict(10)` (age
//!   memoized entries), `compile_bundle()` (incremental on warm cache),
//!   publish result. Mirrors typst's own `typst watch` loop.
//!
//! Routing:
//!
//! - `GET /` — the bundle's `index.html` (compiled from
//!   `content/_index.typ`), or a placeholder slug index if `_index.typ`
//!   is absent.
//! - `GET /<slug>/` (or `/<slug>`) — bundle's `<slug>/index.html`, if
//!   `content/<slug>.typ` exists.
//! - Everything else — served from `static/` first, then `content/` as
//!   a zola-style colocated-asset fallback.

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

use typst_kit::watcher::Watcher;

use crate::render::{
    RenderError, RenderWorld, RoutedDoc, bundle_path_for_slug, scan_pages,
};

pub struct Serve {
    pub site_root: PathBuf,
    pub addr: SocketAddr,
}

/// Result of the most recent bundle compile. Published by the watcher
/// thread, read by request handlers.
type LastOutput = Mutex<Result<Vec<RoutedDoc>, RenderError>>;

struct ServeState {
    site_root: PathBuf,
    last_output: Arc<LastOutput>,
}

pub fn run(serve: Serve) -> io::Result<()> {
    // Build the persistent world and run the warm-up compile on the
    // foreground thread. Doing it before bind means a startup error is
    // reported on stderr before any browser sees it, and the first
    // request after bind hits a warm cache.
    let world = match RenderWorld::new(&serve.site_root) {
        Ok(w) => Arc::new(Mutex::new(w)),
        Err(e) => {
            eprintln!("twyla serve: setup failed:\n{e}");
            return Err(io::Error::other("setup failed"));
        }
    };

    let warm_start = Instant::now();
    let initial = world.lock().unwrap().compile_bundle();
    match &initial {
        Ok(docs) => eprintln!(
            "twyla serve: warmed {} doc(s) in {:.1?}",
            docs.len(),
            warm_start.elapsed(),
        ),
        Err(e) => eprintln!(
            "twyla serve: initial compile errored — continuing; the \
             error page will surface in the browser on first request:\n{e}",
        ),
    }
    let last_output: Arc<LastOutput> = Arc::new(Mutex::new(initial));

    // Spawn the watcher thread. It owns the world from this point on
    // for compile purposes; request handlers only read `last_output`.
    let content_dir = world.lock().unwrap().content_dir();
    {
        let world = Arc::clone(&world);
        let last_output = Arc::clone(&last_output);
        thread::Builder::new()
            .name("twyla-watcher".to_string())
            .spawn(move || run_watcher(world, last_output, content_dir))
            .map_err(io::Error::other)?;
    }

    let listener = TcpListener::bind(serve.addr)?;
    let local = listener.local_addr()?;
    eprintln!(
        "twyla serve: http://{}  (site root: {})",
        local,
        serve.site_root.display(),
    );

    let state = ServeState {
        site_root: serve.site_root,
        last_output,
    };

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!("accept error: {e}");
                continue;
            }
        };
        if let Err(e) = handle(&state, stream) {
            eprintln!("connection error: {e}");
        }
    }
    Ok(())
}

/// Background recompile loop. Mirrors typst's own `typst watch` shape
/// (`typst-cli/src/watch.rs`) — update watched paths from last compile,
/// wait, reset, evict, recompile — but the compile feeds `last_output`
/// for request handlers to read rather than writing to disk.
fn run_watcher(
    world: Arc<Mutex<RenderWorld>>,
    last_output: Arc<LastOutput>,
    content_dir: PathBuf,
) {
    let mut watcher = match Watcher::new(None) {
        Ok(w) => w,
        Err(e) => {
            eprintln!(
                "twyla serve: cannot start file watcher ({e}). \
                 File changes will not trigger recompiles."
            );
            return;
        }
    };

    loop {
        // Subscribe to the latest dep set + always to `content/`
        // (non-recursive directory watch picks up create/delete of
        // top-level posts that weren't in the dep list).
        let paths: Vec<PathBuf> = {
            let mut w = world.lock().unwrap();
            let mut v: Vec<PathBuf> = w.dependencies().collect();
            v.push(content_dir.clone());
            v
        };
        if let Err(e) = watcher.update(paths) {
            eprintln!("twyla serve: watcher update failed: {e}");
            return;
        }
        if let Err(e) = watcher.wait() {
            eprintln!("twyla serve: watcher wait failed: {e}");
            return;
        }

        // Some watched path changed. Refresh the main (in case
        // content/ shape changed), reset FileStore, age comemo, and
        // recompile. Publish the result.
        let start = Instant::now();
        let (slugs_result, docs_result) = {
            let mut w = world.lock().unwrap();
            let slugs = w.refresh_main();
            w.reset();
            comemo::evict(10);
            let docs = w.compile_bundle();
            (slugs, docs)
        };
        let elapsed = start.elapsed();
        match (&slugs_result, &docs_result) {
            (Ok(slugs), Ok(docs)) => eprintln!(
                "twyla serve: recompiled {} doc(s) from {} slug(s) in {:.1?}",
                docs.len(),
                slugs.len(),
                elapsed,
            ),
            (Err(e), _) => eprintln!(
                "twyla serve: content/ rescan failed: {e}"
            ),
            (Ok(_), Err(e)) => eprintln!(
                "twyla serve: recompile errored (after {:.1?}):\n{e}",
                elapsed,
            ),
        }
        *last_output.lock().unwrap() = docs_result;
    }
}

fn handle(state: &ServeState, mut stream: TcpStream) -> io::Result<()> {
    let request_line = read_request(&stream)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    let start = Instant::now();
    let response = if method != "GET" {
        Response::text(405, "method not allowed")
    } else {
        dispatch(state, &path)
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

fn dispatch(state: &ServeState, path: &str) -> Response {
    let path = path.split('?').next().unwrap_or(path);

    if path == "/" {
        return serve_doc(state, "index.html");
    }

    // Page route: single path component, no extension, matching
    // `content/<slug>.typ`. Anything else falls through to static.
    let trimmed = path.trim_start_matches('/').trim_end_matches('/');
    if !trimmed.is_empty() && !trimmed.contains('/') {
        let typ_path =
            state.site_root.join("content").join(format!("{trimmed}.typ"));
        if typ_path.is_file() {
            return serve_doc(state, &bundle_path_for_slug(trimmed));
        }
    }

    serve_static(&state.site_root, path)
}

/// Serve a compiled doc from the cached `last_output`. For `/`,
/// `bundle_path` is `index.html`; for `/<slug>/` it's
/// `<slug>/index.html`. If the bundle doesn't contain the requested
/// path: for `/` we fall back to the placeholder slug index (early
/// project state — no `_index.typ` yet); for slug routes we return 404
/// even though the `.typ` exists (probably the compile errored on that
/// doc — the error path below handles the compile-error case before
/// this).
fn serve_doc(state: &ServeState, bundle_path: &str) -> Response {
    let target = PathBuf::from(bundle_path);
    let guard = state.last_output.lock().unwrap();
    match &*guard {
        Ok(docs) => match docs.iter().find(|d| d.path == target) {
            Some(d) => Response::html(200, d.html.clone()),
            None => {
                if bundle_path == "index.html" {
                    placeholder_index(&state.site_root)
                } else {
                    Response::text(404, "page not found in bundle")
                }
            }
        },
        Err(e) => {
            let body = format!(
                "<!doctype html><html><body><h1>twyla: render error</h1>\
                 <pre style=\"white-space:pre-wrap\">{}</pre></body></html>",
                html_escape(&e.to_string()),
            );
            Response::html(500, body)
        }
    }
}

/// Plain-list index of available slugs, served at `/` when no
/// `content/_index.typ` exists. Stand-in for a real home page during
/// the early stages of a site.
fn placeholder_index(site_root: &Path) -> Response {
    let slugs = match scan_pages(site_root) {
        Ok(s) => s,
        Err(e) => {
            return Response::html(500, format!("<pre>{}</pre>", html_escape(&e)));
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
    out.push_str("<p>placeholder index — content/_index.typ not present.</p>\n");
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
