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
//!   check. When the only changes are edits to existing static files *and*
//!   no page's HTML changed, the browser gets an in-place `asset:` swap
//!   instead of a full reload (see [`static_swaps`]).
//! - Failures never end hot reload: a watcher error drops the watcher and
//!   recreates it with backoff, then forces one rebuild to cover whatever
//!   changed while it was down; a compile panic becomes an error page.
//! - Reload events go through a third thread, the broadcaster: the watcher
//!   thread only records the latest event in a coalescing slot
//!   ([`Broadcaster`]) and never touches a socket. The broadcaster writes to
//!   all SSE sockets at once, nonblocking, under one deadline, and drops any
//!   that stalled — so neither a rebuild nor a healthy tab waits on a stalled
//!   one.
//!
//! Fonts are enumerated once, when the session starts. Installing, removing,
//! or editing a font needs a `twyla serve` restart.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use owo_colors::{AnsiColors, OwoColorize, Stream, Style};

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
/// Request handlers push new ones; the broadcaster thread writes reload
/// events (see [`broadcast`]) and prunes any whose socket failed or stalled.
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
    let broadcaster = Arc::new(Broadcaster::default());

    // Spawn the broadcaster thread: the only place that writes to SSE
    // sockets, so a stalled browser tab never delays a rebuild.
    {
        let broadcaster = Arc::clone(&broadcaster);
        let reload_clients = Arc::clone(&reload_clients);
        thread::Builder::new()
            .name("twyla-broadcaster".to_string())
            .spawn(move || run_broadcaster(&broadcaster, &reload_clients))
            .map_err(io::Error::other)?;
    }

    // Spawn the watcher thread. The session moves into it — it's the only
    // place that compiles; request handlers only read `last_output`. After
    // each recompile it tells the browsers to reload (or swap a static file).
    {
        let last_output = Arc::clone(&last_output);
        thread::Builder::new()
            .name("twyla-watcher".to_string())
            .spawn(move || run_watcher(session, watch, last_output, broadcaster))
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
    broadcaster: Arc<Broadcaster>,
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
        // loop, disconnected channel, a panic): drop this watcher (dropping
        // is panic-safe) and retry with a new one. A path the backend merely
        // refuses is polled instead, with a one-time warning.
        match w.update(&watch) {
            Ok(warnings) => {
                for warning in warnings {
                    eprintln!("  {}", warning.if_supports_color(OUT, |s| s.yellow()));
                }
            }
            Err(e) => {
                watcher_failed(&format!("watcher update failed: {e}"), &mut backoff);
                watcher = None;
                force_rebuild = true;
                continue;
            }
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

        // Some watched path changed: rebuild and publish. What the outgoing
        // result published decides below whether this was an in-place edit.
        let before = Published::of(&last_output.lock().unwrap());
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
        let swaps = static_swaps(&changed, &static_dir, &before, &rebuild.result);
        *last_output.lock().unwrap() = rebuild.result;

        // Tell every connected browser. We signal on *every* publish,
        // including compile errors, so that fixing broken typst reloads the
        // error page back to a working one. Never blocks: delivery happens on
        // the broadcaster thread.
        match swaps {
            Some(urls) => {
                for url in &urls {
                    eprintln!(
                        "  {}  {url}  {}",
                        "↻".if_supports_color(OUT, |s| s.cyan()),
                        "(static reload)".if_supports_color(OUT, |s| s.dimmed()),
                    );
                }
                broadcaster.swap(urls);
            }
            None => broadcaster.reload(),
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

/// What a compile result put in front of the browser, as far as deciding
/// between an in-place swap and a full reload goes: the set of static files,
/// and a hash of every page's HTML by output path. Empty for an error.
#[derive(Debug, Default, PartialEq, Eq)]
struct Published {
    static_keys: BTreeSet<String>,
    doc_html: BTreeMap<String, u64>,
}

impl Published {
    fn of(result: &Result<Outputs, RenderError>) -> Published {
        let mut published = Published::default();
        let Ok(site) = result else {
            return published;
        };
        for out in site.iter() {
            match out {
                Output::Static(key, _) => {
                    published.static_keys.insert(key.clone());
                }
                Output::Doc(doc) => {
                    let mut hasher = DefaultHasher::new();
                    doc.html.hash(&mut hasher);
                    published
                        .doc_html
                        .insert(doc.output_path.clone(), hasher.finish());
                }
                Output::Asset { .. } | Output::Companion { .. } => {}
            }
        }
        published
    }
}

/// If this rebuild was caused only by edits to existing static files, the
/// URLs to swap in place (`static/site.css` → `/site.css`); otherwise `None`,
/// meaning a full reload.
///
/// "Only edits" is decided from the outputs and the disk, not from event
/// kinds (which vary by backend and editor). All of:
/// - the compile succeeded, and nothing is `unknown`;
/// - the set of static files is the same before and after;
/// - every page's HTML is unchanged — a page can `read()` a static file
///   (say, to inline it), and swapping the `<link>` would leave that copy
///   stale;
/// - every changed path is under `static/` and is either a known static file
///   (swapped) or no longer exists and never was one (an editor's temp file,
///   created and renamed away within the batch; ignored). Anything else — a
///   directory, whose event says nothing about which files under it changed,
///   or a file that exists but isn't in the outputs — means a full reload.
fn static_swaps(
    changed: &ChangedPaths,
    static_dir: &Path,
    before: &Published,
    after: &Result<Outputs, RenderError>,
) -> Option<BTreeSet<String>> {
    if changed.is_empty() || changed.unknown || after.is_err() {
        return None;
    }
    if Published::of(after) != *before {
        return None;
    }
    let mut urls = BTreeSet::new();
    for path in &changed.paths {
        let rel = path.strip_prefix(static_dir).ok()?;
        let key = rel.to_string_lossy().replace('\\', "/");
        let known = before.static_keys.contains(&key);
        match (known, path.is_file()) {
            // The URL the dev server serves this file at. The client matches
            // it suffix-wise against `<link>`/`<img>` URLs, so a
            // `base_url`/version prefix on the page is tolerated.
            (true, true) => {
                urls.insert(format!("/{key}"));
            }
            (false, false) if !path.exists() => {}
            _ => return None,
        }
    }
    (!urls.is_empty()).then_some(urls)
}

/// The reload event the broadcaster thread still has to deliver. Only the
/// latest state matters, so events coalesce: swaps accumulate (every swapped
/// file still needs swapping), and a reload supersedes any pending swaps (the
/// page will refetch everything anyway).
#[derive(Debug, Default, PartialEq, Eq)]
enum Pending {
    #[default]
    Nothing,
    Swaps(BTreeSet<String>),
    Reload,
}

/// The single-slot, coalescing hand-off from the watcher thread to the
/// broadcaster thread. Posting only takes a short-lived lock on the slot,
/// never one held during socket writes, so the watcher thread can't block on
/// delivery.
#[derive(Default)]
struct Broadcaster {
    pending: Mutex<Pending>,
    wake: Condvar,
}

impl Broadcaster {
    /// Tell every browser to reload.
    fn reload(&self) {
        *self.pending.lock().unwrap() = Pending::Reload;
        self.wake.notify_one();
    }

    /// Tell every browser to swap these static files in place.
    fn swap(&self, urls: BTreeSet<String>) {
        let mut pending = self.pending.lock().unwrap();
        match &mut *pending {
            Pending::Reload => {}
            Pending::Swaps(queued) => queued.extend(urls),
            Pending::Nothing => *pending = Pending::Swaps(urls),
        }
        drop(pending);
        self.wake.notify_one();
    }

    /// Block until there's something to deliver, and take it.
    fn next(&self) -> Pending {
        let mut pending = self.pending.lock().unwrap();
        while *pending == Pending::Nothing {
            pending = self.wake.wait(pending).unwrap();
        }
        std::mem::take(&mut *pending)
    }
}

/// The broadcaster thread: deliver each coalesced event to every client.
fn run_broadcaster(broadcaster: &Broadcaster, clients: &ReloadClients) {
    loop {
        let msg = match broadcaster.next() {
            Pending::Nothing => continue,
            Pending::Reload => "data: reload\n\n".to_string(),
            Pending::Swaps(urls) => urls
                .iter()
                .map(|url| format!("data: asset:{url}\n\n"))
                .collect(),
        };
        broadcast(clients, msg.as_bytes());
    }
}

/// How long one broadcast may take in total before every client that hasn't
/// received the whole message is considered stalled and dropped.
const SSE_DEADLINE: Duration = Duration::from_secs(1);
/// How long to sleep between write passes over clients whose socket buffer
/// is full.
const SSE_RETRY: Duration = Duration::from_millis(5);

/// Write `msg` (one or more complete SSE messages) to every connected reload
/// client, dropping any whose write fails or doesn't finish within
/// [`SSE_DEADLINE`].
///
/// The client list is taken out of the mutex for the writes and the
/// survivors put back afterwards, so a slow socket never holds the lock that
/// request handlers need to register new clients. Writes are nonblocking and
/// interleaved across clients under one shared deadline: a healthy client
/// gets the message on the first pass no matter how many stalled ones there
/// are, and a broadcast costs at most [`SSE_DEADLINE`], not that per client.
fn broadcast(clients: &ReloadClients, msg: &[u8]) {
    let taken = std::mem::take(&mut *clients.lock().unwrap());
    let deadline = Instant::now() + SSE_DEADLINE;
    // (socket, bytes of `msg` already written)
    let mut sending: Vec<(TcpStream, usize)> = taken
        .into_iter()
        .filter(|stream| stream.set_nonblocking(true).is_ok())
        .map(|stream| (stream, 0))
        .collect();
    let mut alive = Vec::new();
    loop {
        let mut blocked = Vec::new();
        for (mut stream, mut written) in sending {
            match write_nonblocking(&mut stream, &msg[written..]) {
                Ok(n) if written + n == msg.len() => alive.push(stream),
                Ok(n) => {
                    written += n;
                    blocked.push((stream, written));
                }
                Err(_) => {} // dropped
            }
        }
        sending = blocked;
        if sending.is_empty() || Instant::now() >= deadline {
            break; // anything still in `sending` stalled: dropped
        }
        thread::sleep(SSE_RETRY);
    }
    // Clients that connected during the writes are already in the list.
    clients.lock().unwrap().extend(alive);
}

/// Write as much of `buf` as the socket accepts right now; `Ok(0)` only if
/// its buffer is full. A closed socket is an error.
fn write_nonblocking(stream: &mut TcpStream, buf: &[u8]) -> io::Result<usize> {
    let mut written = 0;
    while written < buf.len() {
        match stream.write(&buf[written..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(written)
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
/// the socket in the reload registry. The broadcaster thread writes events
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
        Some(Output::Asset { asset, .. }) => {
            return serve_emit(Path::new(&asset.output_path), &asset.built.emit);
        }
        Some(Output::Companion { path, emit, .. }) => return serve_emit(Path::new(path), emit),
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

/// Serve a processed (`asset.*`) asset's bytes — or an `asset.js` chunk's —
/// with the Content-Type of its output path `dest`.
fn serve_emit(dest: &Path, emit: &Emit) -> Response {
    match emit {
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

/// The `Content-Type` for a file, by extension: what `serve` sends, and
/// the mime type of `asset.js` `?inline` data URLs.
pub(crate) fn mime_for(p: &Path) -> &'static str {
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "application/javascript; charset=utf-8",
        "json" | "map" => "application/json; charset=utf-8",
        "xml" => "application/xml; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "md" => "text/markdown; charset=utf-8",
        "csv" => "text/csv; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "wasm" => "application/wasm",
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
    use std::io::Read;
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use super::*;
    use crate::session::DevSession;

    /// A connected socket pair; the first is the server side.
    fn socket_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server, _) = listener.accept().unwrap();
        (server, client)
    }

    /// Several tabs that stopped reading must not delay a healthy one, nor
    /// the sender: posting an event returns at once, the healthy client gets
    /// all of it well within one deadline (serial per-client timeouts would
    /// take one deadline *per stalled client* before reaching it), and the
    /// stalled ones are dropped.
    ///
    /// The event is a large swap set (32 MiB on the wire) rather than a
    /// 14-byte `reload`: Linux loopback sockets keep absorbing small writes
    /// well past the first `WouldBlock`, so only a message bigger than any
    /// socket buffer reliably stalls.
    #[test]
    fn stalled_sse_clients_dont_delay_sender_or_healthy_client() {
        let mut peers = Vec::new();
        let mut list = Vec::new();
        for _ in 0..4 {
            // The peer never reads.
            let (server, peer) = socket_pair();
            list.push(server);
            peers.push(peer);
        }
        let (healthy, mut healthy_peer) = socket_pair();
        list.push(healthy); // last: a serial broadcast would reach it last

        let swap_urls: BTreeSet<String> = (0..32)
            .map(|i| format!("/{i:02}-{}.css", "x".repeat(1 << 20)))
            .collect();
        let expected: usize = swap_urls
            .iter()
            .map(|url| format!("data: asset:{url}\n\n").len())
            .sum();

        let (got_tx, got_rx) = mpsc::channel();
        thread::spawn(move || {
            let mut total = 0;
            let mut buf = vec![0; 1 << 16];
            while let Ok(n) = healthy_peer.read(&mut buf) {
                if n == 0 {
                    break;
                }
                total += n;
                if total == expected {
                    let _ = got_tx.send(());
                }
            }
        });

        let clients: Arc<ReloadClients> = Arc::new(Mutex::new(list));
        let broadcaster = Arc::new(Broadcaster::default());
        {
            let (b, c) = (Arc::clone(&broadcaster), Arc::clone(&clients));
            thread::spawn(move || run_broadcaster(&b, &c));
        }

        let start = Instant::now();
        broadcaster.swap(swap_urls);
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "posting blocked for {:?}",
            start.elapsed()
        );
        got_rx
            .recv_timeout(SSE_DEADLINE / 2)
            .expect("healthy client didn't get the event promptly");

        let deadline = Instant::now() + SSE_DEADLINE * 3;
        while clients.lock().unwrap().len() != 1 {
            assert!(
                Instant::now() < deadline,
                "stalled clients never dropped ({} left)",
                clients.lock().unwrap().len()
            );
            thread::sleep(Duration::from_millis(20));
        }
        drop(peers);
    }

    #[test]
    fn pending_events_coalesce() {
        let b = Broadcaster::default();
        b.swap(BTreeSet::from(["/a.css".to_string()]));
        b.swap(BTreeSet::from(["/b.css".to_string()]));
        assert_eq!(
            b.next(),
            Pending::Swaps(BTreeSet::from(["/a.css".into(), "/b.css".into()]))
        );
        b.swap(BTreeSet::from(["/a.css".to_string()]));
        b.reload();
        b.swap(BTreeSet::from(["/b.css".to_string()]));
        assert_eq!(b.next(), Pending::Reload, "reload supersedes swaps");
    }

    fn outputs(static_dir: &Path, statics: &[&str]) -> Result<Outputs, RenderError> {
        let mut all = iddqd::IdHashMap::new();
        for key in statics {
            all.insert_unique(Output::Static(key.to_string(), static_dir.join(key)))
                .unwrap();
        }
        Ok(Outputs { all })
    }

    fn changed(paths: &[PathBuf]) -> ChangedPaths {
        ChangedPaths {
            paths: paths.iter().cloned().collect(),
            unknown: false,
        }
    }

    fn urls(urls: &[&str]) -> Option<BTreeSet<String>> {
        Some(urls.iter().map(|u| u.to_string()).collect())
    }

    #[test]
    fn static_swaps_only_for_edits_to_existing_static_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("static");
        std::fs::create_dir_all(dir.join("img")).unwrap();
        std::fs::write(dir.join("site.css"), "").unwrap();
        std::fs::write(dir.join("img/a.png"), "").unwrap();
        let before = Published::of(&outputs(&dir, &["site.css", "img/a.png"]));
        let same = outputs(&dir, &["site.css", "img/a.png"]);

        // An edit (plus an editor temp file that came and went): swap.
        assert_eq!(
            static_swaps(
                &changed(&[dir.join("site.css"), dir.join(".site.css.swp")]),
                &dir,
                &before,
                &same
            ),
            urls(&["/site.css"])
        );
        // A content change alongside: full reload.
        assert_eq!(
            static_swaps(
                &changed(&[dir.join("site.css"), tmp.path().join("content/main.typ")]),
                &dir,
                &before,
                &same
            ),
            None
        );
        // A static file added or removed: full reload.
        std::fs::write(dir.join("new.css"), "").unwrap();
        let added = outputs(&dir, &["site.css", "img/a.png", "new.css"]);
        assert_eq!(
            static_swaps(&changed(&[dir.join("new.css")]), &dir, &before, &added),
            None
        );
        // Unknown changes (forced rebuild / overflow): full reload.
        let unknown = ChangedPaths {
            unknown: true,
            ..changed(&[dir.join("site.css")])
        };
        assert_eq!(static_swaps(&unknown, &dir, &before, &same), None);
        // A directory in the batch: which files under it changed is unknown,
        // so full reload even alongside a swappable edit.
        assert_eq!(
            static_swaps(
                &changed(&[dir.join("site.css"), dir.join("img")]),
                &dir,
                &before,
                &same
            ),
            None
        );
        // An existing file that isn't a known static output: full reload.
        assert_eq!(
            static_swaps(&changed(&[dir.join("new.css")]), &dir, &before, &same),
            None
        );
    }

    /// A page that `read()`s a static stylesheet (to inline it) changes when
    /// the stylesheet does; swapping the `<link>` alone would leave the
    /// inlined copy stale, so it's a full reload. The same edit with a page
    /// that only links the file is swapped.
    #[test]
    fn static_swaps_reload_when_page_html_changes() {
        for (page, expect_swap) in [("#read(\"/static/site.css\")", false), ("plain page", true)] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path();
            std::fs::create_dir_all(root.join("content")).unwrap();
            std::fs::create_dir_all(root.join("static")).unwrap();
            std::fs::write(root.join("content/main.typ"), page).unwrap();
            let css = root.join("static/site.css");
            std::fs::write(&css, "body { color: red }").unwrap();
            let ctx = TwylaContext::new(root, None).unwrap();
            let mut session = DevSession::new(&ctx).unwrap();
            let before = Published::of(&session.rebuild().result);

            std::fs::write(&css, "body { color: blue }").unwrap();
            let after = session.rebuild().result;
            let swaps = static_swaps(
                &changed(std::slice::from_ref(&css)),
                &ctx.static_dir(),
                &before,
                &after,
            );
            if expect_swap {
                assert_eq!(swaps, urls(&["/site.css"]), "{page}");
            } else {
                assert_eq!(swaps, None, "{page}");
            }
        }
    }
}
