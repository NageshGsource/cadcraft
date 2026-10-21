//! QuickPlot DWG server.
//!
//! The web app runs as WebAssembly, where the DWG bridge (`acadrust`) isn't available, so it asks
//! this server to convert:
//!
//! - `POST /to-dxf` with a DWG or DWT file as the body answers the DXF the desktop app would read
//!   it through ([`cadcraft_dwg::dwg_to_dxf_noted`]).
//! - `POST /to-dwg?name=drawing.dwg` (or `.dwt`) with a DXF body answers the DWG (or template)
//!   the desktop app would save ([`cadcraft_io::write`]).
//! - `GET /health` answers `ok`.
//!
//! Every answer carries CORS headers (and the Private Network Access one) so a page served from
//! elsewhere, such as GitHub Pages, may call a server on the user's own machine.
//!
//! Usage: `quickplot-dwg-server [--addr 127.0.0.1:8790] [--allow-origin *]`
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(not(target_arch = "wasm32"))]
mod server {
    use std::io::Read as _;

    use tiny_http::{Header, Method, Request, Response, Server};

    /// The largest file accepted, the same order as the web app's own open limit.
    const MAX_BYTES: u64 = 256 << 20;
    const DEFAULT_ADDR: &str = "127.0.0.1:8790";

    struct Options {
        addr: String,
        allow_origin: String,
    }

    fn options() -> Result<Options, String> {
        let mut o = Options { addr: DEFAULT_ADDR.to_string(), allow_origin: "*".to_string() };
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            match a.as_str() {
                "--addr" => o.addr = args.next().ok_or("--addr needs HOST:PORT")?,
                "--allow-origin" => o.allow_origin = args.next().ok_or("--allow-origin needs an origin or *")?,
                "-h" | "--help" => {
                    return Err(format!("usage: quickplot-dwg-server [--addr {DEFAULT_ADDR}] [--allow-origin *]"));
                }
                other => return Err(format!("unknown argument `{other}` (try --help)")),
            }
        }
        Ok(o)
    }

    pub fn run() -> i32 {
        let o = match options() {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{e}");
                return 2;
            }
        };
        let server = match Server::http(&o.addr) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("quickplot-dwg-server: cannot listen on {}: {e}", o.addr);
                return 1;
            }
        };
        eprintln!("quickplot-dwg-server listening on http://{} (CORS origin {})", o.addr, o.allow_origin);
        for req in server.incoming_requests() {
            let origin = o.allow_origin.clone();
            // One thread per request: a large conversion doesn't hold up the others.
            std::thread::spawn(move || handle(req, &origin));
        }
        0
    }

    fn handle(mut req: Request, origin: &str) {
        let (path, query) = match req.url().split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (req.url().to_string(), String::new()),
        };
        let method = req.method().clone();
        let reply = match (&method, path.as_str()) {
            (Method::Options, _) => Reply::empty(204),
            (Method::Get, "/health") => Reply::text(200, "ok"),
            (Method::Post, "/to-dxf") => body(&mut req).and_then(|b| guarded(|| to_dxf(&b))).unwrap_or_else(|e| e),
            (Method::Post, "/to-dwg") => body(&mut req).and_then(|b| guarded(|| to_dwg(&b, &query))).unwrap_or_else(|e| e),
            _ => Reply::text(404, "not found: use POST /to-dxf, POST /to-dwg?name=…, GET /health"),
        };
        eprintln!("{method} {path} -> {}", reply.status);
        respond(req, reply, origin);
    }

    struct Reply {
        status: u16,
        mime: &'static str,
        bytes: Vec<u8>,
    }

    impl Reply {
        fn empty(status: u16) -> Self {
            Reply { status, mime: "text/plain; charset=utf-8", bytes: Vec::new() }
        }
        fn text(status: u16, s: &str) -> Self {
            Reply { status, mime: "text/plain; charset=utf-8", bytes: s.as_bytes().to_vec() }
        }
    }

    fn body(req: &mut Request) -> Result<Vec<u8>, Reply> {
        if req.body_length().is_some_and(|n| n as u64 > MAX_BYTES) {
            return Err(Reply::text(413, "the file is larger than 256 MB"));
        }
        let mut bytes = Vec::new();
        req.as_reader().take(MAX_BYTES + 1).read_to_end(&mut bytes).map_err(|e| Reply::text(400, &format!("could not read the request: {e}")))?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err(Reply::text(413, "the file is larger than 256 MB"));
        }
        if bytes.is_empty() {
            return Err(Reply::text(400, "the request has no file"));
        }
        Ok(bytes)
    }

    /// Run a conversion; a panic in a converter becomes an error reply, not a dead thread with no answer.
    fn guarded(f: impl FnOnce() -> Reply + std::panic::UnwindSafe) -> Result<Reply, Reply> {
        std::panic::catch_unwind(f).map_err(|_| Reply::text(500, "the converter failed on this file"))
    }

    fn to_dxf(bytes: &[u8]) -> Reply {
        if !cadcraft_dwg::is_dwg(bytes) {
            return Reply::text(422, "this is not a DWG or DWT file");
        }
        match cadcraft_dwg::dwg_to_dxf_noted(bytes, cadcraft_dwg::Limits::DEFAULT) {
            Ok((dxf, _notes)) => Reply { status: 200, mime: "application/dxf", bytes: dxf },
            Err(e) => Reply::text(422, &format!("could not read this DWG: {e}")),
        }
    }

    fn to_dwg(dxf: &[u8], query: &str) -> Reply {
        let name = query.split('&').find_map(|p| p.strip_prefix("name=")).unwrap_or("drawing.dwg");
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
        if ext != "dwg" && ext != "dwt" {
            return Reply::text(400, "name must end in .dwg or .dwt");
        }
        let d = match cadcraft_io::read(dxf, "drawing.dxf") {
            Ok(d) => d,
            Err(e) => return Reply::text(422, &format!("could not read the DXF: {e}")),
        };
        match cadcraft_io::write(&d, &format!("drawing.{ext}")) {
            Ok(dwg) => Reply { status: 200, mime: "application/acad", bytes: dwg },
            Err(e) => Reply::text(422, &format!("could not write the {}: {e}", ext.to_ascii_uppercase())),
        }
    }

    fn respond(req: Request, r: Reply, origin: &str) {
        let mut res = Response::from_data(r.bytes).with_status_code(r.status);
        for (k, v) in [
            ("Content-Type", r.mime),
            ("Access-Control-Allow-Origin", origin),
            ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
            ("Access-Control-Allow-Headers", "Content-Type"),
            ("Access-Control-Allow-Private-Network", "true"),
            ("Access-Control-Max-Age", "600"),
        ] {
            if let Ok(h) = Header::from_bytes(k.as_bytes(), v.as_bytes()) {
                res.add_header(h);
            }
        }
        if let Err(e) = req.respond(res) {
            eprintln!("could not send the reply: {e}");
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    std::process::exit(server::run());
}

#[cfg(target_arch = "wasm32")]
fn main() {}
