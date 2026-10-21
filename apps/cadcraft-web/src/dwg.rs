//! DWG and DWT on the web, through the QuickPlot DWG server (`apps/quickplot-dwg-server`).
//!
//! The DWG bridge doesn't run in WebAssembly, so:
//! - opening a DWG/DWT sends it to the server's `POST /to-dxf` and opens the DXF it answers;
//! - saving as `.dwg`/`.dwt` writes DXF here ([`write`], [`write_as`]), and the delivery hooks
//!   ([`deliver_download`], [`deliver_host`]) send it to `POST /to-dwg` before handing the
//!   DWG on. If the server can't be reached the drawing is delivered as DXF instead, so a save is
//!   never lost.
//!
//! The server is the page's own address when the DWG server hosts QuickPlot (`--static`), else
//! `http://127.0.0.1:8790`; `?dwg=URL` names another ([`set_server`]).

use std::sync::OnceLock;

use cadcraft_engine::cmd::file::SaveAs;
use wasm_bindgen::JsCast as _;
use web_sys::js_sys;

const DEFAULT_SERVER: &str = "http://127.0.0.1:8790";

static SERVER: OnceLock<String> = OnceLock::new();

/// Use `url` (from `?dwg=`) as the conversion server. Without one: the page's own address when
/// the DWG server serves QuickPlot itself (`--static`, e.g. on the local network), else the
/// server on this computer (pages hosted elsewhere, such as GitHub Pages).
pub fn set_server(url: Option<String>) {
    let url = url.filter(|u| !u.trim().is_empty()).or_else(own_origin).unwrap_or_else(|| DEFAULT_SERVER.to_string());
    let _ = SERVER.set(url.trim_end_matches('/').to_string());
}

/// The page's origin, unless it is a static host that can't convert DWG (GitHub Pages).
fn own_origin() -> Option<String> {
    let location = web_sys::window()?.location();
    let host = location.hostname().ok()?;
    if host.ends_with(".github.io") {
        return None;
    }
    location.origin().ok().filter(|o| o.starts_with("http"))
}

fn server() -> &'static str {
    SERVER.get().map(String::as_str).unwrap_or(DEFAULT_SERVER)
}

/// Whether `name` is saved as DWG content (a drawing or a template).
pub fn is_dwg_name(name: &str) -> bool {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    ext == "dwg" || ext == "dwt"
}

/// The OPEN hook's writer: DXF in place of DWG/DWT (converted when delivered), else the usual one.
pub fn write(d: &cadcraft_doc::Drawing, name: &str) -> Result<Vec<u8>, String> {
    if is_dwg_name(name) {
        return cadcraft_io::write(d, "drawing.dxf").map_err(|e| e.to_string());
    }
    cadcraft_io::write(d, name).map_err(|e| e.to_string())
}

/// SAVEAS with options: DXF at the chosen version in place of DWG/DWT, else the usual writer.
pub fn write_as(d: &cadcraft_doc::Drawing, name: &str, o: &SaveAs) -> Result<Vec<u8>, String> {
    if is_dwg_name(name) {
        if o.binary {
            return Err("binary applies to DXF files only".into());
        }
        let opts = cadcraft_io::SaveOptions { version: o.version, binary: false };
        return cadcraft_io::write_with(d, "drawing.dxf", &opts).map_err(|e| e.to_string());
    }
    cadcraft_io::write_with(d, name, &cadcraft_io::SaveOptions { version: o.version, binary: o.binary }).map_err(|e| e.to_string())
}

/// Save as a browser download, converting DXF to DWG/DWT first when the name asks for it.
pub fn deliver_download(name: &str, mime: &str, bytes: &[u8]) -> Result<(), String> {
    deliver_via(name, mime, bytes, crate::download::download)
}

/// Save to the embedding page (`?host=parent`), converting to DWG/DWT first when the name asks for it.
pub fn deliver_host(name: &str, mime: &str, bytes: &[u8]) -> Result<(), String> {
    deliver_via(name, mime, bytes, crate::host::deliver)
}

fn deliver_via(name: &str, mime: &str, bytes: &[u8], then: fn(&str, &str, &[u8]) -> Result<(), String>) -> Result<(), String> {
    if !is_dwg_name(name) || cadcraft_dwg::is_dwg(bytes) {
        return then(name, mime, bytes);
    }
    let (name, dxf) = (name.to_string(), bytes.to_vec());
    wasm_bindgen_futures::spawn_local(async move {
        let url = format!("{}/to-dwg?name={}", server(), encode(&name));
        match post(&url, &dxf).await {
            Ok(dwg) => {
                if let Err(e) = then(&name, "application/acad", &dwg) {
                    alert(&format!("Save {name}: {e}"));
                }
            }
            Err(e) => {
                let fallback = format!("{}.dxf", name.rsplit_once('.').map_or(name.as_str(), |(stem, _)| stem));
                alert(&format!(
                    "{name} could not be saved as DWG: {e}\n\nThe DWG server is at {}. Your drawing is saved as {fallback} instead.",
                    server()
                ));
                if let Err(e) = then(&fallback, "application/dxf", &dxf) {
                    alert(&format!("Save {fallback}: {e}"));
                }
            }
        }
    });
    Ok(())
}

/// Convert a DWG/DWT file to DXF on the server.
pub async fn to_dxf(name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
    post(&format!("{}/to-dxf", server()), bytes)
        .await
        .map_err(|e| format!("Open: {name} is a DWG file, which QuickPlot opens through its DWG server ({}): {e}", server()))
}

/// GET `url` (for `?open=URL`).
pub async fn get(url: &str) -> Result<Vec<u8>, String> {
    let init = web_sys::RequestInit::new();
    init.set_method("GET");
    fetch(url, &init).await
}

async fn post(url: &str, body: &[u8]) -> Result<Vec<u8>, String> {
    let init = web_sys::RequestInit::new();
    init.set_method("POST");
    init.set_body(&js_sys::Uint8Array::from(body));
    fetch(url, &init).await
}

async fn fetch(url: &str, init: &web_sys::RequestInit) -> Result<Vec<u8>, String> {
    let window = web_sys::window().ok_or("no browser window")?;
    let res = wasm_bindgen_futures::JsFuture::from(window.fetch_with_str_and_init(url, init))
        .await
        .map_err(|e| format!("the server could not be reached ({})", js_error(e)))?
        .dyn_into::<web_sys::Response>()
        .map_err(|_| "the server's answer was not a response".to_string())?;
    if !res.ok() {
        let text = match res.text() {
            Ok(p) => wasm_bindgen_futures::JsFuture::from(p).await.ok().and_then(|v| v.as_string()).unwrap_or_default(),
            Err(_) => String::new(),
        };
        return Err(format!("HTTP {} {}", res.status(), text.trim()));
    }
    let buf = wasm_bindgen_futures::JsFuture::from(res.array_buffer().map_err(js_error)?).await.map_err(js_error)?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

fn encode(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

fn alert(msg: &str) {
    log::warn!("{msg}");
    if let Some(w) = web_sys::window() {
        let _ = w.alert_with_message(msg);
    }
}

fn js_error(e: wasm_bindgen::JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}
