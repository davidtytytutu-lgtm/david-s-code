// HEXFORGE Lab — serveur distant (Rust/axum).
//
// Routes :
//   GET /api/ping        -> {"server","status":"pong","timestamp"}
//   GET /  (WebSocket)   -> JSON-RPC 2.0 : ping / status / exec {lang,file,code,args}
//
// `exec` exécute vraiment le code en sous-processus (node, lua, javac/java,
// rustc) avec timeout, et renvoie {exit, stdout, stderr}. HTML = analyse simple
// (le rendu se fait côté client, preview sandbox).
//
// ATTENTION : pas de sandbox — n'exposez ce serveur qu'à du code de confiance
// ou dans un conteneur jetable (voir Dockerfile). Limites : 200 Ko de code,
// 25 s max, 2 exécutions simultanées.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, sync::Semaphore, time::timeout};
use tower_http::cors::CorsLayer;

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

const MAX_CODE_BYTES: usize = 200_000;
const EXEC_TIMEOUT: Duration = Duration::from_secs(25);
const OUT_TAIL_CHARS: usize = 4000;

#[derive(Clone)]
struct AppState {
    exec_sem: Arc<Semaphore>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    let state = AppState {
        exec_sem: Arc::new(Semaphore::new(2)),
    };
    let app = Router::new()
        .route("/api/ping", get(ping))
        .route("/", get(ws_handler))
        .layer(CorsLayer::permissive())
        .with_state(state);
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(3000);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    tracing::info!("hexforge-server listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, app).await.expect("serve");
}

async fn ping() -> Json<Value> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Json(json!({"server":"HEXFORGE Lab Remote API","status":"pong","timestamp":ts}))
}

// ---------- JSON-RPC 2.0 ----------

#[derive(Deserialize)]
struct RpcReq {
    #[allow(dead_code)]
    jsonrpc: String,
    id: Value,
    method: String,
    params: Option<Value>,
}

fn ok_resp(id: &Value, result: Value) -> String {
    json!({"jsonrpc":"2.0","id":id,"result":result}).to_string()
}

fn err_resp(id: &Value, code: i32, message: &str) -> String {
    json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}).to_string()
}

async fn ws_handler(ws: WebSocketUpgrade, State(state): State<AppState>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    while let Some(msg) = socket.recv().await {
        let text = match msg {
            Ok(Message::Text(t)) => t,
            Ok(Message::Close(_)) | Err(_) => break,
            _ => continue,
        };
        let req: RpcReq = match serde_json::from_str(&text) {
            Ok(r) => r,
            Err(e) => {
                let _ = socket
                    .send(Message::Text(err_resp(
                        &Value::Null,
                        -32700,
                        &format!("parse error: {e}"),
                    )))
                    .await;
                continue;
            }
        };
        let out = match req.method.as_str() {
            "ping" => ok_resp(&req.id, json!({"status": "pong"})),
            "status" => ok_resp(
                &req.id,
                json!({"server":"HEXFORGE Lab Remote API","langs":["js","lua","java","rust","html"]}),
            ),
            "exec" => match exec_code(&state, &req.params.unwrap_or(Value::Null)).await {
                Ok(v) => ok_resp(&req.id, v),
                Err(e) => err_resp(&req.id, -32000, &e),
            },
            _ => err_resp(&req.id, -32601, "Unknown method"),
        };
        if socket.send(Message::Text(out)).await.is_err() {
            break;
        }
    }
}

// ---------- exec ----------

async fn exec_code(state: &AppState, params: &Value) -> Result<Value, String> {
    let lang = params
        .get("lang")
        .and_then(|v| v.as_str())
        .unwrap_or("js")
        .to_lowercase();
    let code = params.get("code").and_then(|v| v.as_str()).unwrap_or("");
    if code.len() > MAX_CODE_BYTES {
        return Err("code trop volumineux (max 200 Ko)".into());
    }
    if code.trim().is_empty() {
        return Err("code vide".into());
    }
    let args: Vec<String> = match params.get("args") {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| match x {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    };
    let _permit = state
        .exec_sem
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "serveur occupé, réessayez".to_string())?;
    let dir = fresh_tmp().await?;
    let res = timeout(EXEC_TIMEOUT, run_lang(&lang, code, &args, &dir))
        .await
        .map_err(|_| "timeout (25 s)".to_string())?;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    res
}

async fn fresh_tmp() -> Result<PathBuf, String> {
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("hexforge-{}-{ms}-{n}", std::process::id()));
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("tmp: {e}"))?;
    Ok(dir)
}

/// Garde les dernières lignes/caractères (sans jamais couper un caractère UTF-8).
fn tail(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= OUT_TAIL_CHARS {
        s.to_string()
    } else {
        format!(
            "…[tronqué]\n{}",
            chars[chars.len() - OUT_TAIL_CHARS..].iter().collect::<String>()
        )
    }
}

/// Lance un programme dans `dir`, avec timeout. Erreur si le binaire manque.
async fn run_cmd(
    dir: &Path,
    prog: &str,
    args: &[String],
    secs: u64,
) -> Result<(i32, String, String), String> {
    let mut child = Command::new(prog)
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("lancement {prog} impossible ({e}) — runtime manquant ?"))?;
    let out = match timeout(Duration::from_secs(secs), child.wait_with_output()).await {
        Ok(o) => o.map_err(|e| format!("attente {prog}: {e}"))?,
        Err(_) => {
            let _ = child.kill().await;
            return Err(format!("{prog} : timeout ({secs} s), processus tué"));
        }
    };
    let code = out.status.code().unwrap_or(-1);
    Ok((
        code,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

/// Essaie plusieurs noms de binaire (ex: lua / lua5.4) : passe au suivant
/// seulement si le lancement est impossible (binaire absent).
async fn run_cmd_first(
    dir: &Path,
    progs: &[&str],
    args: &[String],
    secs: u64,
) -> Result<(i32, String, String), String> {
    let mut last = String::new();
    for p in progs {
        match run_cmd(dir, p, args, secs).await {
            Ok(o) => return Ok(o),
            Err(e) if e.contains("lancement") && e.contains("impossible") => {
                last = e;
                continue;
            }
            Err(e) => return Err(e),
        }
    }
    Err(if last.is_empty() {
        "aucun runtime disponible".to_string()
    } else {
        last
    })
}

async fn run_lang(
    lang: &str,
    code: &str,
    args: &[String],
    dir: &Path,
) -> Result<Value, String> {
    match lang {
        "js" | "javascript" => {
            let src =
                strip_export_lines(code) + "\nif (typeof main === \"function\") await main(ns);\n";
            let prelude = format!("const ns = {};\n", ns_prelude(args)?);
            tokio::fs::write(dir.join("main.mjs"), prelude + &src)
                .await
                .map_err(|e| format!("write: {e}"))?;
            let argv = std::iter::once("main.mjs".to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            let (c, so, se) = run_cmd_first(dir, &["node", "nodejs"], &argv, 20).await?;
            Ok(json!({"lang":"js","exit":c,"stdout":tail(&so),"stderr":tail(&se)}))
        }
        "lua" => {
            let src = format!("ns = {};\n", lua_args(args)) + code + "\nif main then main(ns) end\n";
            tokio::fs::write(dir.join("main.lua"), src)
                .await
                .map_err(|e| format!("write: {e}"))?;
            let argv = std::iter::once("main.lua".to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            let (c, so, se) = run_cmd_first(dir, &["lua", "lua5.4", "luajit"], &argv, 20).await?;
            Ok(json!({"lang":"lua","exit":c,"stdout":tail(&so),"stderr":tail(&se)}))
        }
        "java" => {
            tokio::fs::write(dir.join("Main.java"), code)
                .await
                .map_err(|e| format!("write: {e}"))?;
            let (cc, co, ce) =
                run_cmd(dir, "javac", &["Main.java".to_string()], 20).await?;
            if cc != 0 {
                return Ok(json!({"lang":"java","exit":cc,"stdout":tail(&co),"stderr":tail(&ce)}));
            }
            let argv = std::iter::once("Main".to_string())
                .chain(args.iter().cloned())
                .collect::<Vec<_>>();
            let (c, so, se) = run_cmd(dir, "java", &argv, 20).await?;
            Ok(json!({"lang":"java","exit":c,"stdout":tail(&so),"stderr":tail(&se)}))
        }
        "rs" | "rust" => {
            tokio::fs::write(dir.join("main.rs"), code)
                .await
                .map_err(|e| format!("write: {e}"))?;
            let prog = if cfg!(windows) { ".\\prog.exe" } else { "./prog" };
            let (cc, co, ce) = run_cmd(
                dir,
                "rustc",
                &[
                    "-O".to_string(),
                    "main.rs".to_string(),
                    "-o".to_string(),
                    "prog".to_string(),
                ],
                20,
            )
            .await?;
            if cc != 0 {
                return Ok(
                    json!({"lang":"rust","exit":cc,"stdout":tail(&co),"stderr":tail(&ce)})
                );
            }
            let (c, so, se) = run_cmd(dir, prog, args, 20).await?;
            Ok(json!({"lang":"rust","exit":c,"stdout":tail(&so),"stderr":tail(&se)}))
        }
        "html" | "htm" => Ok(json!({
            "lang": "html",
            "bytes": code.len(),
            "has_form": code.contains("<form"),
            "has_script": code.contains("<script"),
            "note": "rendu côté client (preview sandbox) — le serveur ne rend pas la page"
        })),
        other => Err(format!("langage inconnu : {other} (js, lua, java, rust, html)")),
    }
}

/// Enlève les `export ...` en début de ligne (le code tourne en module nu).
fn strip_export_lines(code: &str) -> String {
    code.lines()
        .map(|l| {
            let t = l.trim_start();
            if let Some(rest) = t.strip_prefix("export default ") {
                rest.to_string()
            } else if let Some(rest) = t.strip_prefix("export ") {
                rest.to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Objet `ns` injecté devant le JS : args + print/tprint/sleep/httpGet.
fn ns_prelude(args: &[String]) -> Result<String, String> {
    let a = serde_json::to_string(args).map_err(|e| format!("args: {e}"))?;
    Ok(format!(
        "{{ args: {a}, print: (...x) => console.log(...x), \
        tprint: (...x) => console.log(...x), \
        sleep: (ms) => new Promise((r) => setTimeout(r, Number(ms) || 0)), \
        httpGet: async (url) => {{ const r = await fetch(String(url)); return await r.text(); }} }}"
    ))
}

/// Table `ns` injectée devant le Lua (chaînes échappées).
fn lua_args(args: &[String]) -> String {
    let mut s = String::from("{ args = {");
    for (i, a) in args.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        s.push('"');
        for ch in a.chars() {
            match ch {
                '\\' => s.push_str("\\\\"),
                '"' => s.push_str("\\\""),
                '\n' => s.push_str("\\n"),
                '\r' => s.push_str("\\r"),
                c => s.push(c),
            }
        }
        s.push('"');
    }
    s.push_str(
        "}, print = function(...) print(...) end, tprint = function(...) print(...) end, \
        sleep = function(ms) local t = os.clock() + (tonumber(ms) or 0) / 1000 \
        while os.clock() < t do end end }",
    );
    s
}
