//! Minimal dev server with hot recompile.
//!
//! One thread for accepts, one for the file watcher. Requests are served
//! sequentially out of the last compile; the watcher rebuilds the bundle
//! in the background when typst sources, asset sources, static files, or the
//! contents of any directory under `content/` or `static/` change.
//!
//! Architecture
//! ------------
//!
//! - A single persistent [`DevSession`] (the typst world + comemo cache and
//!   the asset/document resolver) is created on the main thread for the
//!   warm-up compile, then moved into the watcher thread — the only place
//!   that compiles. Request handlers never compile.
//! - The most recent compile result lives in `Arc<Mutex<LastOutput>>`.
//!   Request handlers lock it briefly to clone the matching doc's HTML.
//! - The watcher thread loops: [`Watcher::update`] with the last [`WatchSet`]
//!   → [`Watcher::wait`] for changed paths → [`DevSession::rebuild`] (reset
//!   FileStore, age comemo, scan `content/` + `static/` once, compile, collect
//!   the next watch set) → publish → notify browsers. Mirrors typst's own
//!   `typst watch` loop; see [`crate::watch`] for how the watcher closes the
//!   gap between a compile reading a file and the file being subscribed.
//! - `static/` is part of the same loop: its directories are in the watch set
//!   (each non-recursively, like `content/`'s), and a static change rebuilds
//!   too, because static files take part in the bundle's output-conflict
//!   check. When the only changes are edits to existing static files, the
//!   browser gets an in-place `asset:` swap instead of a full reload.
//! - Failures never end hot reload: a watcher error drops the watcher and
//!   recreates it with backoff, then forces one rebuild to cover whatever
//!   changed while it was down; a compile panic becomes an error page.
//! - Reload events are written to the browsers' SSE sockets outside any lock
//!   and with a write timeout, so a stalled browser tab can't block the
//!   watcher thread; it's dropped instead.
//!
//! Fonts are enumerated once, when the session starts. Installing, removing,
//! or editing a font needs a `twyla serve` restart.

use std::collections::BTreeSet;
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use owo_colors::{AnsiColors, OwoColorize, Stream, Style};

use crate::asset::ResolvedAsset;
use crate::project::TwylaContext;
use crate::render::{Emit, Output, Outputs, RenderError};
use crate::session::{DevSession, Rebuild};
use crate::watch::{ChangedPaths, WatchSet, Watcher};

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
/// Request handlers push new ones; the watcher thread writes reload
/// events (see [`broadcast`]) and prunes any whose socket failed.
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
    // Build the persistent session and run the warm-up compile on the
    // foreground thread. Doing it before bind means a startup error is
    // reported on stderr before any browser sees it, and the first
    // request after bind hits a warm cache.
    let mut session = match DevSession::new(&serve.ctx) {
        Ok(s) => s,
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

    let warm_start = Instant::now();
    let Rebuild {
        result: initial,
        watch,
    } = session.rebuild();
    print_banner(local, &serve.ctx.root, &initial, warm_start.elapsed());

    let last_output: Arc<LastOutput> = Arc::new(Mutex::new(initial));
    let reload_clients: Arc<ReloadClients> = Arc::new(Mutex::new(Vec::new()));

    // Spawn the watcher thread. The session moves into it — it's the only
    // place that compiles; request handlers only read `last_output`. After
    // each recompile it tells the browsers to reload (or swap a static file).
    {
        let last_output = Arc::clone(&last_output);
        let reload_clients = Arc::clone(&reload_clients);
        thread::Builder::new()
            .name("twyla-watcher".to_string())
            .spawn(move || run_watcher(session, watch, last_output, reload_clients))
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

/// Background recompile loop: `watcher.update(watch)` → `wait()` →
/// [`DevSession::rebuild`] → publish → notify browsers → repeat. All the
/// compile logic, and the choice of what to watch, lives in the session
/// (shared with tests); this loop only adds the file watcher, logging, and
/// publishing.
///
/// The loop never exits on its own: a watcher failure is logged and the
/// watcher recreated (with a short backoff), and a compile panic comes back
/// from `rebuild` as an ordinary error. Silently losing hot reload is worse
/// than a noisy log.
fn run_watcher(
    mut session: DevSession,
    mut watch: WatchSet,
    last_output: Arc<LastOutput>,
    reload_clients: Arc<ReloadClients>,
) {
    let static_dir = session.world.ctx.static_dir();
    let mut watcher: Option<Watcher> = None;
    let mut backoff = WATCHER_BACKOFF_MIN;
    // Set whenever the watcher was down: changes made meanwhile produced no
    // events, so rebuild once without waiting for one. (Re-subscribing also
    // re-checks every path's observed stamp, but this doesn't rely on that.)
    let mut force_rebuild = false;
    loop {
        // (Re)create the watcher if the last attempt failed. A fresh watcher
        // has no subscriptions, so the `update` below re-watches everything.
        let w = match &mut watcher {
            Some(w) => w,
            None => match Watcher::new() {
                Ok(w) => watcher.insert(w),
                Err(e) => {
                    watcher_failed(&format!("cannot start file watcher: {e}"), &mut backoff);
                    force_rebuild = true;
                    continue;
                }
            },
        };
        // Either call failing means the backend is unusable (dead event
        // loop, disconnected channel, a path it refuses to watch): drop this
        // watcher (dropping is panic-safe) and retry with a new one.
        if let Err(e) = w.update(&watch) {
            watcher_failed(&format!("watcher update failed: {e}"), &mut backoff);
            watcher = None;
            force_rebuild = true;
            continue;
        }
        let changed = if std::mem::take(&mut force_rebuild) {
            ChangedPaths {
                unknown: true,
                ..Default::default()
            }
        } else {
            match w.wait() {
                Ok(changed) => changed,
                Err(e) => {
                    watcher_failed(&format!("watcher wait failed: {e}"), &mut backoff);
                    watcher = None;
                    force_rebuild = true;
                    continue;
                }
            }
        };
        backoff = WATCHER_BACKOFF_MIN;

        // Some watched path changed: rebuild and publish. The static keys of
        // the outgoing result decide below whether this was an in-place edit.
        let static_before = static_keys(&last_output.lock().unwrap());
        let start = Instant::now();
        let rebuild = session.rebuild();
        let elapsed = start.elapsed();
        match &rebuild.result {
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
        watch = rebuild.watch;
        let swaps = static_swaps(&changed, &static_dir, &static_before, &rebuild.result);
        *last_output.lock().unwrap() = rebuild.result;

        // Tell every connected browser. We signal on *every* publish,
        // including compile errors, so that fixing broken typst reloads the
        // error page back to a working one.
        match swaps {
            Some(urls) => {
                for url in urls {
                    eprintln!(
                        "  {}  {url}  {}",
                        "↻".if_supports_color(OUT, |s| s.cyan()),
                        "(static reload)".if_supports_color(OUT, |s| s.dimmed()),
                    );
                    broadcast(&reload_clients, &format!("asset:{url}"));
                }
            }
            None => broadcast(&reload_clients, "reload"),
        }
    }
}

/// Backoff between watcher-recovery attempts, doubling up to the max so a
/// persistently broken watcher logs a few times a minute rather than spinning.
const WATCHER_BACKOFF_MIN: Duration = Duration::from_millis(250);
const WATCHER_BACKOFF_MAX: Duration = Duration::from_secs(10);

/// Log a watcher failure and sleep before the retry.
fn watcher_failed(msg: &str, backoff: &mut Duration) {
    eprintln!(
        "  {}",
        format!("{msg} (retrying in {backoff:.1?})").if_supports_color(OUT, |s| s.red())
    );
    thread::sleep(*backoff);
    *backoff = (*backoff * 2).min(WATCHER_BACKOFF_MAX);
}

/// The output keys of every `static/` file in a compile result (empty for an
/// error).
fn static_keys(result: &Result<Outputs, RenderError>) -> BTreeSet<String> {
    let Ok(site) = result else {
        return BTreeSet::new();
    };
    site.iter()
        .filter_map(|out| match out {
            Output::Static(key, _) => Some(key.clone()),
            _ => None,
        })
        .collect()
}

/// If this rebuild was caused only by edits to existing static files, the
/// URLs to swap in place (`static/site.css` → `/site.css`); otherwise `None`,
/// meaning a full reload.
///
/// "Only edits" is decided from the outputs, not from event kinds (which vary
/// by backend and editor): every changed path is under `static/`, the compile
/// succeeded, and the set of static files is the same before and after. A
/// changed path that's a static file in neither result is ignored (an
/// editor's temp file, created and renamed away within the batch).
fn static_swaps(
    changed: &ChangedPaths,
    static_dir: &Path,
    before: &BTreeSet<String>,
    after: &Result<Outputs, RenderError>,
) -> Option<Vec<String>> {
    if changed.is_empty() || changed.unknown || after.is_err() {
        return None;
    }
    if static_keys(after) != *before {
        return None;
    }
    let mut urls = Vec::new();
    for path in &changed.paths {
        let rel = path.strip_prefix(static_dir).ok()?;
        let key = rel.to_string_lossy().replace('\\', "/");
        if before.contains(&key) {
            // The URL the dev server serves this file at. The client matches
            // it suffix-wise against `<link>`/`<img>` URLs, so a
            // `base_url`/version prefix on the page is tolerated.
            urls.push(format!("/{key}"));
        }
    }
    (!urls.is_empty()).then_some(urls)
}

/// How long writing one SSE message may take before the client is
/// considered stalled and dropped (see [`write_sse`]).
const SSE_WRITE_TIMEOUT: Duration = Duration::from_secs(1);

/// Push one Server-Sent-Events message to every connected reload client,
/// dropping any whose write fails or times out.
///
/// The client list is taken out of the mutex for the writes and the
/// survivors put back afterwards, so a slow socket never holds the lock that
/// request handlers need to register new clients. Each write is bounded by
/// [`SSE_WRITE_TIMEOUT`], so a tab that stopped reading costs the watcher
/// thread at most that long, once.
fn broadcast(clients: &ReloadClients, data: &str) {
    let taken = std::mem::take(&mut *clients.lock().unwrap());
    let alive: Vec<TcpStream> = taken
        .into_iter()
        .filter_map(|mut stream| write_sse(&mut stream, data).ok().map(|()| stream))
        .collect();
    // Clients that connected during the writes are already in the list.
    clients.lock().unwrap().extend(alive);
}

/// Write one SSE message, giving up after [`SSE_WRITE_TIMEOUT`] in total.
/// The socket timeout alone bounds each `write` call, and a client draining
/// slowly could keep `write_all` going indefinitely one partial write at a
/// time, so each call gets only what's left of the deadline.
fn write_sse(stream: &mut TcpStream, data: &str) -> io::Result<()> {
    let msg = format!("data: {data}\n\n");
    let mut buf = msg.as_bytes();
    let deadline = Instant::now() + SSE_WRITE_TIMEOUT;
    while !buf.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::ErrorKind::TimedOut.into());
        }
        stream.set_write_timeout(Some(left))?;
        match stream.write(buf)? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => buf = &buf[n..],
        }
    }
    Ok(())
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
/// the socket in the reload registry. The watcher thread writes events
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
        Some(Output::Asset { asset, .. }) => return serve_asset(asset),
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

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::path::PathBuf;

    use super::*;

    /// A connected socket pair; the first is the server side.
    fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    /// A browser tab that stopped reading fills its socket buffer; after
    /// that, a broadcast must give up on it within the write timeout and drop
    /// it, never block indefinitely. A healthy client is kept throughout.
    #[test]
    fn stalled_sse_client_is_dropped_not_waited_on() {
        let (stalled, _stalled_peer) = socket_pair();
        let (healthy, healthy_peer) = socket_pair();
        // Keep the healthy peer drained.
        std::thread::spawn(move || {
            let mut peer = healthy_peer;
            let _ = io::copy(&mut peer, &mut io::sink());
        });

        let clients: ReloadClients = Mutex::new(vec![stalled, healthy]);
        let big = "x".repeat(1 << 20);
        for _ in 0..256 {
            let start = Instant::now();
            broadcast(&clients, &big);
            assert!(
                start.elapsed() < SSE_WRITE_TIMEOUT + Duration::from_millis(500),
                "broadcast blocked for {:?}",
                start.elapsed()
            );
            if clients.lock().unwrap().len() == 1 {
                return; // stalled client dropped, healthy one kept
            }
        }
        panic!("stalled client never dropped");
    }

    fn outputs(statics: &[&str]) -> Result<Outputs, RenderError> {
        let mut all = iddqd::IdHashMap::new();
        for key in statics {
            let path = PathBuf::from("/site/static").join(key);
            all.insert_unique(Output::Static(key.to_string(), path))
                .unwrap();
        }
        Ok(Outputs { all })
    }

    fn changed(paths: &[&str]) -> ChangedPaths {
        ChangedPaths {
            paths: paths.iter().map(PathBuf::from).collect(),
            unknown: false,
        }
    }

    #[test]
    fn static_swaps_only_for_edits_to_existing_static_files() {
        let dir = Path::new("/site/static");
        let before = static_keys(&outputs(&["site.css", "img/a.png"]));
        let same = outputs(&["site.css", "img/a.png"]);

        // An edit (plus an editor temp file that came and went): swap.
        assert_eq!(
            static_swaps(
                &changed(&["/site/static/site.css", "/site/static/.site.css.swp"]),
                dir,
                &before,
                &same
            ),
            Some(vec!["/site.css".to_string()])
        );
        // A content change alongside: full reload.
        assert_eq!(
            static_swaps(
                &changed(&["/site/static/site.css", "/site/content/main.typ"]),
                dir,
                &before,
                &same
            ),
            None
        );
        // A static file added or removed: full reload.
        let added = outputs(&["site.css", "img/a.png", "new.css"]);
        assert_eq!(
            static_swaps(&changed(&["/site/static/new.css"]), dir, &before, &added),
            None
        );
        // Unknown changes (forced rebuild / overflow): full reload.
        let unknown = ChangedPaths {
            unknown: true,
            ..changed(&["/site/static/site.css"])
        };
        assert_eq!(static_swaps(&unknown, dir, &before, &same), None);
    }
}
