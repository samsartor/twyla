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

use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use notify::{EventKind, RecursiveMode, Watcher as _};
use typst_kit::watcher::Watcher;

use owo_colors::{AnsiColors, OwoColorize, Stream, Style};

use crate::asset::ResolvedAsset;
use crate::resolver::Resolver;
use crate::project::TwylaContext;
use crate::render::{Emit, Output, Outputs, RenderError, RenderWorld};

/// All of serve's own logging goes to stderr; color follows that stream's tty.
const OUT: Stream = Stream::Stderr;

/// Color an HTTP status code by class: 2xx green, 3xx cyan, 4xx yellow, else red.
fn status_color(status: u16) -> AnsiColors {
    match status / 100 {
        2 => AnsiColors::Green,
        3 => AnsiColors::Cyan,
        4 => AnsiColors::Yellow,
        _ => AnsiColors::Red,
    }
}

/// One aligned request-log line: status colored by class, then the method and
/// path, then a dimmed trailing detail (the timing, or a note).
fn log_request(status: u16, method: &str, path: &str, detail: &str) {
    let code = format!("{status:>3}");
    let req = format!("{method} {path}");
    eprintln!(
        "  {}  {req:<34}{}",
        code.if_supports_color(OUT, |s| s
            .style(Style::new().color(status_color(status)).bold())),
        detail.if_supports_color(OUT, |s| s.dimmed()),
    );
}

pub struct Serve {
    pub ctx: TwylaContext,
    pub addr: SocketAddr,
}

/// Result of the most recent bundle compile (pages + processed assets).
/// Published by the watcher thread, read by request handlers.
type LastOutput = Mutex<Result<Outputs, RenderError>>;

/// Live Server-Sent-Events connections subscribed to `/__twyla/reload`.
/// Request handlers push new ones; the watcher threads write reload
/// events and prune any whose socket has closed.
type ReloadClients = Mutex<Vec<TcpStream>>;

struct ServeState {
    ctx: TwylaContext,
    last_output: Arc<LastOutput>,
    reload_clients: Arc<ReloadClients>,
}

/// The startup banner: the URL, the site root, and the warm-up result.
fn print_banner(
    local: SocketAddr,
    root: &Path,
    initial: &Result<Outputs, RenderError>,
    elapsed: Duration,
) {
    // Show the root relative to the cwd when possible — `test_site/` reads
    // better than an absolute path. The zero-flag `serve` runs in the site
    // root, where that relative path is empty, so fall back to `.`.
    let cwd = std::env::current_dir().unwrap_or_default();
    let root = root.strip_prefix(&cwd).unwrap_or(root);
    let root = match root.to_str() {
        Some("") => ".".to_string(),
        _ => format!("{}/", root.display()),
    };

    let mark = "▲"
        .if_supports_color(OUT, |s| s.style(Style::new().cyan().bold()))
        .to_string();
    let arrow = "→".if_supports_color(OUT, |s| s.dimmed()).to_string();
    let url = format!("http://{local}/")
        .if_supports_color(OUT, |s| s.cyan())
        .to_string();

    eprintln!();
    eprintln!(
        "  {mark} {}",
        "twyla serve".if_supports_color(OUT, |s| s.bold())
    );
    eprintln!();
    eprintln!("  {arrow}  Local:  {url}");
    eprintln!("  {arrow}  Root:   {root}");
    match initial {
        Ok(site) => eprintln!(
            "  {}  ready in {elapsed:.1?} ({} pages, {} assets)",
            "✓".if_supports_color(OUT, |s| s.style(Style::new().green().bold())),
            site.docs().count(),
            site.assets().count(),
        ),
        Err(e) => {
            eprintln!(
                "  {}  initial compile failed in {elapsed:.1?} — the browser will show the error",
                "✗".if_supports_color(OUT, |s| s.style(Style::new().red().bold())),
            );
            eprintln!("\n{e}");
        }
    }
    eprintln!();
}

pub fn run(serve: Serve) -> io::Result<()> {
    // Build the persistent world and run the warm-up compile on the
    // foreground thread. Doing it before bind means a startup error is
    // reported on stderr before any browser sees it, and the first
    // request after bind hits a warm cache.
    let world = match RenderWorld::new(&serve.ctx) {
        Ok(w) => Arc::new(Mutex::new(w)),
        Err(e) => {
            eprintln!(
                "  {} setup failed",
                "✗".if_supports_color(OUT, |s| s.style(Style::new().red().bold()))
            );
            eprintln!("{e}");
            return Err(io::Error::other("setup failed"));
        }
    };

    // Bind before the warm-up so the banner can show the URL next to the
    // compile result. Binding only reserves the port; we don't accept until
    // the loop below, so the warm cache is still ready before any request.
    let listener = TcpListener::bind(serve.addr)?;
    let local = listener.local_addr()?;

    // The asset resolver persists across recompiles (its store seeds each
    // compile's map; `revalidate` evicts changed sources). Lives here, moves
    // into the watcher thread — the only place that compiles.
    let mut resolver = Resolver::new(&serve.ctx);

    let warm_start = Instant::now();
    let initial = world.lock().unwrap().compile_bundle(&mut resolver);
    print_banner(local, &serve.ctx.root, &initial, warm_start.elapsed());

    let last_output: Arc<LastOutput> = Arc::new(Mutex::new(initial));
    let reload_clients: Arc<ReloadClients> = Arc::new(Mutex::new(Vec::new()));

    // Spawn the typst watcher thread. It owns the world from this point
    // on for compile purposes; request handlers only read `last_output`.
    // After each recompile it broadcasts a full-reload event.
    let content_dir = serve.ctx.content_dir();
    {
        let world = Arc::clone(&world);
        let last_output = Arc::clone(&last_output);
        let reload_clients = Arc::clone(&reload_clients);
        thread::Builder::new()
            .name("twyla-watcher".to_string())
            .spawn(move || run_watcher(world, last_output, content_dir, reload_clients, resolver))
            .map_err(io::Error::other)?;
    }

    // Spawn the static-asset watcher thread. Unlike the typst watcher it
    // surfaces the changed path, so the browser can swap one stylesheet
    // or image in place instead of doing a full reload.
    {
        let static_dir = serve.ctx.static_dir();
        let reload_clients = Arc::clone(&reload_clients);
        thread::Builder::new()
            .name("twyla-static-watcher".to_string())
            .spawn(move || run_static_watcher(static_dir, reload_clients))
            .map_err(io::Error::other)?;
    }

    let state = ServeState {
        ctx: serve.ctx,
        last_output,
        reload_clients,
    };

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "  {}",
                    format!("accept error: {e}").if_supports_color(OUT, |s| s.red())
                );
                continue;
            }
        };
        if let Err(e) = handle(&state, stream) {
            eprintln!(
                "  {}",
                format!("connection error: {e}").if_supports_color(OUT, |s| s.red())
            );
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
    reload_clients: Arc<ReloadClients>,
    mut resolver: Resolver,
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
        let mut paths: Vec<PathBuf> = {
            let mut w = world.lock().unwrap();
            let mut v: Vec<PathBuf> = w.dependencies().collect();
            v.push(content_dir.clone());
            v
        };
        // Asset upstreams (e.g. sass `@import` partials) are read by twyla's
        // own pass, not typst, so they're not in `world.dependencies()` — add
        // them explicitly or an edit to a partial wouldn't trigger a recompile.
        if let Ok(site) = &*last_output.lock().unwrap() {
            for asset in site.assets() {
                paths.extend(asset.upstream_paths().map(Path::to_path_buf));
            }
        }
        if let Err(e) = watcher.update(paths) {
            eprintln!(
                "  {}",
                format!("watcher update failed: {e}").if_supports_color(OUT, |s| s.red())
            );
            return;
        }
        if let Err(e) = watcher.wait() {
            eprintln!(
                "  {}",
                format!("watcher wait failed: {e}").if_supports_color(OUT, |s| s.red())
            );
            return;
        }

        // Some watched path changed. Refresh the main (in case
        // content/ shape changed), reset FileStore, age comemo, and
        // recompile. Publish the result.
        let start = Instant::now();
        let result = {
            comemo::evict(10);
            let mut w = world.lock().unwrap();
            w.files.reset();
            w.compile_bundle(&mut resolver)
        };
        let elapsed = start.elapsed();
        match &result {
            Ok(site) => eprintln!(
                "  {}  rebuilt in {elapsed:.1?} ({} pages, {} assets)",
                "✓".if_supports_color(OUT, |s| s.style(Style::new().green().bold())),
                site.docs().count(),
                site.assets().count(),
            ),
            Err(e) => eprintln!(
                "  {}  rebuild failed in {elapsed:.1?}\n{e}",
                "✗".if_supports_color(OUT, |s| s.style(Style::new().red().bold())),
            ),
        }
        *last_output.lock().unwrap() = result;

        // Tell every connected browser to reload. We signal on *every*
        // publish, including compile errors, so that fixing broken typst
        // reloads the error page back to a working one.
        broadcast(&reload_clients, "reload");
    }
}

/// Dedicated `notify` watcher for `static/`. The typst watcher only
/// signals "something changed" and never sees `static/` (static assets
/// aren't typst dependencies); this fills that gap and, crucially,
/// surfaces the *changed path* so the browser can swap a single
/// stylesheet or image in place rather than reloading the whole page.
fn run_static_watcher(static_dir: PathBuf, reload_clients: Arc<ReloadClients>) {
    if !static_dir.is_dir() {
        return; // no static/ → nothing to watch
    }

    let (tx, rx) = mpsc::channel();
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(e) => {
            let msg =
                format!("static watcher unavailable ({e}); static asset edits won't hot-reload.");
            eprintln!("  {}", msg.if_supports_color(OUT, |s| s.yellow()));
            return;
        }
    };
    if let Err(e) = watcher.watch(&static_dir, RecursiveMode::Recursive) {
        let msg = format!(
            "cannot watch {} ({e}); static asset edits won't hot-reload.",
            static_dir.display(),
        );
        eprintln!("  {}", msg.if_supports_color(OUT, |s| s.yellow()));
        return;
    }

    // `watcher` must stay alive for the lifetime of this loop.
    for res in rx {
        let event = match res {
            Ok(ev) => ev,
            Err(e) => {
                eprintln!(
                    "  {}",
                    format!("static watch error: {e}").if_supports_color(OUT, |s| s.red())
                );
                continue;
            }
        };
        if !matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_)) {
            continue;
        }
        for path in &event.paths {
            let Ok(rel) = path.strip_prefix(&static_dir) else {
                continue;
            };
            // The URL the dev server serves this file at: `static/site.css`
            // → `/site.css`. The client matches it suffix-wise against
            // `<link>`/`<img>` URLs, so a `base_url`/version prefix on the
            // page is tolerated.
            let url = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
            eprintln!(
                "  {}  {url}  {}",
                "↻".if_supports_color(OUT, |s| s.cyan()),
                "(static reload)".if_supports_color(OUT, |s| s.dimmed()),
            );
            broadcast(&reload_clients, &format!("asset:{url}"));
        }
    }
}

/// Push one Server-Sent-Events message to every connected reload client,
/// dropping any whose socket has closed.
fn broadcast(clients: &ReloadClients, data: &str) {
    let mut clients = clients.lock().unwrap();
    clients.retain_mut(|s| write_sse(s, data).is_ok());
}

fn write_sse(stream: &mut TcpStream, data: &str) -> io::Result<()> {
    write!(stream, "data: {data}\n\n")?;
    stream.flush()
}

fn handle(state: &ServeState, mut stream: TcpStream) -> io::Result<()> {
    let request_line = read_request(&stream)?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("/").to_string();

    // The live-reload event stream is a long-lived connection: write the
    // SSE headers and hand the socket to the registry instead of running
    // it through the request/response path (which would close it).
    if method == "GET" && path.split('?').next() == Some("/__twyla/reload") {
        return accept_reload_client(state, stream);
    }

    let start = Instant::now();
    let mut response = if method != "GET" {
        Response::text(405, "method not allowed")
    } else {
        dispatch(state, &path)
    };
    // Splice the reload client into every HTML response served (pages,
    // the error page, the placeholder index, and static `.html`). This
    // is serve-only — the bundle written by `twyla build` is untouched.
    if response.content_type.starts_with("text/html") {
        inject_reload_script(&mut response.body);
    }
    let elapsed = start.elapsed();
    log_request(response.status, &method, &path, &format!("{elapsed:.0?}"));

    write_response(&mut stream, &response)
}

/// Upgrade `GET /__twyla/reload` to a Server-Sent-Events stream and park
/// the socket in the reload registry. The watcher threads write events
/// to it; we never read from it again. The connection stays open because
/// the registry owns the stream — returning here does not close it.
fn accept_reload_client(state: &ServeState, mut stream: TcpStream) -> io::Result<()> {
    // `retry:` shortens the browser's auto-reconnect delay (default 3s)
    // so a `twyla serve` restart reconnects quickly.
    write!(
        stream,
        "HTTP/1.1 200 OK\r\n\
         Content-Type: text/event-stream\r\n\
         Cache-Control: no-cache\r\n\
         Connection: keep-alive\r\n\
         \r\n\
         retry: 1000\n\n",
    )?;
    stream.flush()?;
    log_request(200, "GET", "/__twyla/reload", "reload client connected");
    state.reload_clients.lock().unwrap().push(stream);
    Ok(())
}

/// Inject the live-reload client `<script>` just before `</body>`, or
/// append it if there's no closing body tag.
fn inject_reload_script(body: &mut Vec<u8>) {
    const TAG: &[u8] = b"</body>";
    match body.windows(TAG.len()).rposition(|w| w == TAG) {
        Some(i) => {
            let tail = body.split_off(i);
            body.extend_from_slice(RELOAD_SCRIPT.as_bytes());
            body.extend_from_slice(&tail);
        }
        None => body.extend_from_slice(RELOAD_SCRIPT.as_bytes()),
    }
}

/// Client for the `/__twyla/reload` SSE stream. A `reload` event reloads
/// the page; an `asset:<path>` event swaps a single stylesheet or image
/// in place (preserving scroll/state), falling back to a full reload for
/// anything else or when no matching element is found.
const RELOAD_SCRIPT: &str = r#"<script>
(() => {
  const es = new EventSource("/__twyla/reload");
  const bust = (url) => {
    const u = new URL(url, location.href);
    u.searchParams.set("__twyla", Date.now());
    return u.href;
  };
  const matches = (url, path) => {
    try { return new URL(url, location.href).pathname.endsWith(path); }
    catch { return false; }
  };
  const swap = (sel, attr, path) => {
    let hit = false;
    for (const el of document.querySelectorAll(sel)) {
      if (matches(el[attr], path)) { el[attr] = bust(el[attr]); hit = true; }
    }
    return hit;
  };
  es.onmessage = (e) => {
    const m = e.data;
    if (m.startsWith("asset:")) {
      const path = m.slice(6);
      if (path.endsWith(".css")) {
        if (swap('link[rel="stylesheet"]', "href", path)) return;
      } else if (/\.(png|jpe?g|gif|webp|svg|ico)$/i.test(path)) {
        if (swap("img", "src", path)) return;
      }
    }
    location.reload();
  };
})();
</script>
"#;

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
        Self {
            status,
            content_type,
            body,
        }
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
    let path = path
        .split('?')
        .next()
        .unwrap_or(path)
        .trim_start_matches('/');

    let guard = state.last_output.lock().unwrap();
    let site = match &*guard {
        Ok(site) => site,
        Err(e) => return Response::html(500, error_page(e.html())),
    };

    // Resolve against the compiled outputs: the path as-is (an asset, or a page
    // given with its full `…/index.html`), then the directory-index form
    // (`/`, `foo`, `foo/` → `…/index.html`). Pages and assets share one map, so
    // there's no doc-then-asset fallback to chain.
    let index_key = if path.is_empty() || path.ends_with('/') {
        format!("{path}index.html")
    } else {
        format!("{path}/index.html")
    };
    match site.get(path).or_else(|| site.get(&index_key)) {
        Some(Output::Doc(d)) => return Response::html(200, d.html.clone()),
        Some(Output::Asset(a)) => return serve_asset(a),
        // `Output::Static` falls through to the live-disk read below, so a
        // freshly-added file (not yet in the recompiled map) still resolves.
        _ => {}
    }

    // The placeholder home when there's no `content/main.typ`; otherwise a
    // static file, read live from disk.
    if path.is_empty() {
        return placeholder_main(site, &state.ctx);
    }
    drop(guard);
    serve_static(&state.ctx, path)
}

/// Serve a processed (`asset.*`) asset's bytes, by its `assets/…` output key.
fn serve_asset(asset: &ResolvedAsset) -> Response {
    let dest = Path::new(&asset.output_path);
    match &asset.built.emit {
        Emit::Copy(src) => match std::fs::read(src) {
            Ok(body) => Response::bytes(200, mime_for(dest), body),
            Err(_) => Response::text(404, "asset source unreadable"),
        },
        Emit::Bytes(bytes) => Response::bytes(200, mime_for(dest), bytes.as_slice().to_vec()),
    }
}

/// Wrap a rendered diagnostic block (HTML from `ansi-to-html`) in a dark error
/// page. `ansi-to-html` emits ANSI 4-bit colors as `var(--name, fallback)`, so
/// the `--*` palette below retunes typst's colors for a dark background —
/// otherwise the default `#00a` blue / `#a00` red are unreadable on dark grey.
fn error_page(diagnostic_html: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><style>\
         body{{background:#1e1e1e;color:#d4d4d4;font-family:ui-monospace,monospace;\
         margin:0;padding:1.5rem 2rem;line-height:1.5;\
         --black:#5c6370;--red:#e06c75;--green:#98c379;--yellow:#e5c07b;\
         --blue:#61afef;--magenta:#c678dd;--cyan:#56b6c2;--white:#abb2bf;\
         --bright-black:#7f848e;--bright-red:#ff7b86;--bright-green:#b5e890;\
         --bright-yellow:#ffd596;--bright-blue:#80c4ff;--bright-magenta:#d790ee;\
         --bright-cyan:#6fd3df;--bright-white:#ffffff}}\
         h1{{font-size:1rem;font-weight:600;color:#ff7b86;margin:0 0 1rem}}\
         pre{{white-space:pre-wrap;margin:0;font:inherit}}\
         </style></head>\
         <body><h1>twyla: render error</h1><pre>{diagnostic_html}</pre></body></html>",
    )
}

/// Plain-list index of available slugs, served at `/` when no
/// `content/main.typ` exists. Stand-in for a real home page during
/// the early stages of a site.
fn placeholder_main(site: &Outputs, _ctx: &TwylaContext) -> Response {
    let mut out = String::new();
    out.push_str(
        "<!doctype html>\n\
         <html><head><meta charset=\"utf-8\"><title>twyla dev</title>\n\
         <style>body{font-family:monospace;padding:2rem;line-height:1.6}\
         a{color:#06c}</style>\n\
         </head><body>\n",
    );
    out.push_str("<h1>twyla dev</h1>\n");
    out.push_str("<p>placeholder index — content/main.typ not present.</p>\n");
    out.push_str("<ul>\n");
    for d in site.docs() {
        out.push_str(&format!(
            "<li><a href=\"/{slug}\">/{slug}</a></li>\n",
            slug = html_escape(&d.output_path),
        ));
    }
    out.push_str("</ul>\n</body></html>\n");
    Response::html(200, out)
}

fn serve_static(ctx: &TwylaContext, request_path: &str) -> Response {
    let rel = request_path.trim_start_matches('/');
    // Reject path traversal. Bare `..` segments only — anything embedded
    // in a name is a normal char.
    if rel.split('/').any(|c| c == "..") {
        return Response::text(404, "not found");
    }

    // Static files are served live from disk so edits show without a recompile.
    // (They're also in the compiled `Outputs`, which `build`/`manifest` use.)
    let path = ctx.static_dir().join(rel);
    if path.is_file()
        && let Ok(body) = std::fs::read(&path)
    {
        return Response::bytes(200, mime_for(&path), body);
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
