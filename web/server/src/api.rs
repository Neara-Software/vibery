use crate::catalog::{short_name, space_name, FieldKind};
use crate::engine::Engine;
use crate::query::{Parser, Sort};
use crate::rows::Rows;
use crate::util::{format_uuid, parse_datetime, parse_uuid};
use axum::body::Body;
use axum::extract::{Path as AxPath, Query, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};

pub struct AppState {
    eng: Engine,
    rows: Rows,
    data: PathBuf,
    cache: PathBuf,
    docs_cache: PathBuf,
    http: reqwest::Client,
    api_key: Option<String>,
    mention: regex::Regex,
    syntax: String,
}

type St = Arc<AppState>;
type ApiResult = Result<Response, (StatusCode, Json<Value>)>;

fn err(code: StatusCode, msg: impl ToString) -> (StatusCode, Json<Value>) {
    (code, Json(json!({ "error": msg.to_string() })))
}
fn e500(e: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    err(StatusCode::INTERNAL_SERVER_ERROR, e)
}

fn syntax_text() -> String {
    let src = include_str!("../../../src/main.rs");
    let start = src.find("const SYNTAX: &str = \"").map(|i| i + 22).unwrap_or(0);
    let body = &src[start..];
    let end = body.find("\";\n").unwrap_or(body.len());
    let mut out = String::new();
    let mut it = body[..end].chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\n') => {
                while it.peek().is_some_and(|c| c.is_whitespace()) {
                    it.next();
                }
            }
            Some(o) => out.push(o),
            None => {}
        }
    }
    out
}

impl AppState {
    pub fn new(eng: Engine, rows: Rows, data: PathBuf, web: PathBuf) -> anyhow::Result<Self> {
        let api_key = std::env::var("HOME")
            .ok()
            .and_then(|h| std::fs::read(Path::new(&h).join(".phab-helper.json")).ok())
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .and_then(|v| v["fibery"]["api_key"].as_str().map(String::from));
        let cache = web.join("cache/files");
        std::fs::create_dir_all(&cache)?;
        let docs_cache = web.join("cache/docs");
        std::fs::create_dir_all(&docs_cache)?;
        Ok(AppState {
            eng,
            rows,
            data,
            cache,
            docs_cache,
            http: reqwest::Client::new(),
            api_key,
            mention: regex::Regex::new(
                r"\[\[([^\[\]#]*)#([\^@])([0-9a-f-]{36})/([0-9a-f-]{36})\]\]",
            )?,
            syntax: syntax_text(),
        })
    }

    /// Touch the index so the first real query isn't cold.
    pub fn warm(&self) {
        for q in ["#note rvo", "design", "@sami"] {
            let (spec, w) = Parser::parse(q);
            let _ = self.eng.search(&spec, w);
        }
    }

    fn user_name(&self, id: u128) -> Option<String> {
        self.eng.store.get_user(id).ok()?.map(|u| u.name)
    }

    /// Store first; the raw dump row covers entities newer than the index.
    fn title_of(&self, id: u128) -> Option<(String, String)> {
        if let Some(r) = self.eng.entity(id).ok().flatten() {
            return Some((r.title, self.eng.db_name(r.db).to_string()));
        }
        let (row, db) = self.rows.get(id)?;
        let d = self.eng.catalog.dbs.get(db as usize)?;
        let title = row.get(d.title_field.as_deref()?)?.as_str()?.to_string();
        Some((title, d.name.clone()))
    }

    fn enum_label(&self, id: u128) -> Option<String> {
        let (row, db) = self.rows.get(id)?;
        let tf = self
            .eng
            .catalog
            .dbs
            .get(db as usize)
            .and_then(|d| d.title_field.clone())
            .unwrap_or_else(|| "enum/name".into());
        row.get(&tf)
            .and_then(|v| v.as_str())
            .or_else(|| {
                row.as_object()?
                    .iter()
                    .find(|(k, v)| k.ends_with("/name") && v.is_string())
                    .and_then(|(_, v)| v.as_str())
            })
            .map(String::from)
    }

    fn doc_markdown(&self, secret: u128) -> Option<String> {
        let s = format_uuid(secret);
        let raw = std::fs::read_to_string(self.data.join("documents").join(format!("{s}.md")))
            .ok()
            .or_else(|| std::fs::read_to_string(self.docs_cache.join(format!("{s}.md"))).ok())
            .or_else(|| {
                let (row, _) = self.rows.get(secret)?;
                row.get("content")?.as_str().map(String::from)
            })?;
        Some(self.rewrite_mentions(&raw))
    }

    fn rewrite_mentions(&self, md: &str) -> String {
        self.mention
            .replace_all(md, |c: &regex::Captures| {
                let label = c[1].trim();
                let Some(id) = parse_uuid(&c[4]) else { return c[0].to_string() };
                if &c[2] == "@" {
                    let n = self.user_name(id).unwrap_or_else(|| label.to_string());
                    return format!("@{}", if n.is_empty() { "unknown" } else { &n });
                }
                let t = if label.is_empty() {
                    self.title_of(id).map(|t| t.0).unwrap_or_else(|| "(not in local copy)".into())
                } else {
                    label.to_string()
                };
                format!("[{}](/e/{})", t.replace(['[', ']'], ""), format_uuid(id))
            })
            .into_owned()
    }
}

fn ids_of(v: &Value) -> Vec<u128> {
    let one = |o: &Value| o.get("fibery/id").and_then(|s| s.as_str()).and_then(parse_uuid);
    match v {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::Object(_) => one(v).into_iter().collect(),
        _ => vec![],
    }
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').chars().take(60).collect()
}

// ------------------------------------------------------------- handlers

#[derive(Deserialize)]
struct SearchQ {
    q: Option<String>,
    limit: Option<usize>,
    sort: Option<String>,
}

async fn search(State(s): State<St>, Query(p): Query<SearchQ>) -> ApiResult {
    tokio::task::spawn_blocking(move || {
        let (mut spec, warns) = Parser::parse(p.q.as_deref().unwrap_or(""));
        spec.limit = p.limit.unwrap_or(50).min(500);
        if let Some(so) = p.sort.as_deref() {
            spec.sort = match so {
                "recent" | "updated" => Sort::Recent,
                "created" => Sort::Created,
                "oldest" => Sort::Oldest,
                _ => Sort::Relevance,
            };
        }
        let r = s.eng.search(&spec, warns).map_err(|e| err(StatusCode::BAD_REQUEST, e))?;
        let hits: Vec<Value> = r
            .hits
            .iter()
            .map(|h| {
                json!({"id": h.id, "db": h.db, "title": h.title, "public_id": h.public_id,
                   "snippet": h.snippet, "created": h.created, "updated": h.updated,
                   "state": h.state, "inbound": h.inbound, "pinned": h.pinned})
            })
            .collect();
        Ok(Json(json!({"hits": hits, "total": r.total, "by_db": r.by_db,
            "took_ms": r.took_ms, "warnings": r.warnings, "plan": r.plan}))
        .into_response())
    })
    .await
    .map_err(e500)?
}

async fn entity(State(s): State<St>, AxPath(id): AxPath<String>) -> ApiResult {
    tokio::task::spawn_blocking(move || entity_json(&s, &id)).await.map_err(e500)?
}

fn entity_json(s: &AppState, id_s: &str) -> ApiResult {
    let nf = || err(StatusCode::NOT_FOUND, "unknown entity");
    let id = parse_uuid(id_s).ok_or_else(nf)?;
    let (row, file_db) = s.rows.get(id).ok_or_else(nf)?;
    let rec = match s.eng.entity(id).map_err(e500)? {
        Some(r) => r,
        // Rows the index skipped still render from the raw JSON.
        None => {
            let d = s.eng.catalog.dbs.get(file_db as usize).ok_or_else(nf)?;
            let date = |k: &str| row.get(k).and_then(|v| v.as_str()).and_then(parse_datetime).unwrap_or(0);
            crate::store::EntityRec {
                db: file_db,
                title: d.title_field.as_deref().and_then(|t| row.get(t)).and_then(|v| v.as_str()).unwrap_or("").to_string(),
                public_id: row.get("fibery/public-id").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                created: date("fibery/creation-date"),
                updated: date("fibery/modification-date"),
                created_by: row.get("fibery/created-by").map(ids_of).and_then(|v| v.first().copied()),
                ..Default::default()
            }
        }
    };
    let dbname = s.eng.db_name(rec.db).to_string();
    let dbdef = s.eng.catalog.dbs.get(rec.db as usize);
    let empty = Map::new();
    let obj = row.as_object().unwrap_or(&empty);

    let mut fields = vec![];
    let mut relations = vec![];
    let mut people = vec![];
    let mut docs = vec![];
    let mut comments = vec![];
    for (k, v) in obj {
        if v.is_null() || k.starts_with("fibery/") {
            continue;
        }
        let kind = dbdef.and_then(|d| d.field(k)).map(|f| f.kind);
        match kind {
            Some(FieldKind::Title) | Some(FieldKind::Ignore) => {}
            Some(FieldKind::Enum) => {
                let labels: Vec<String> = ids_of(v).into_iter().filter_map(|e| s.enum_label(e)).collect();
                if !labels.is_empty() {
                    fields.push(json!({"name": k, "value": labels.join(", ")}));
                }
            }
            Some(FieldKind::User) => {
                let items: Vec<Value> = ids_of(v)
                    .into_iter()
                    .filter_map(|u| Some(json!({"id": format_uuid(u), "name": s.user_name(u)?})))
                    .collect();
                if !items.is_empty() {
                    people.push(json!({"field": k, "items": items}));
                }
            }
            Some(FieldKind::Relation) => {
                let ids = ids_of(v);
                let recs = s.eng.store.get_entities(&ids).unwrap_or_default();
                let items: Vec<Value> = ids
                    .iter()
                    .zip(recs)
                    .filter_map(|(i, r)| {
                        let r = r?;
                        Some(json!({"id": format_uuid(*i), "title": r.title, "db": s.eng.db_name(r.db)}))
                    })
                    .collect();
                if !items.is_empty() {
                    relations.push(json!({"field": k, "items": items}));
                }
            }
            Some(FieldKind::RichText) => {
                if let Some(sec) = v.get("Collaboration~Documents/secret").and_then(|x| x.as_str()).and_then(parse_uuid) {
                    let md = s.doc_markdown(sec).unwrap_or_default();
                    docs.push(json!({"field": k, "secret": format_uuid(sec), "markdown": md}));
                }
            }
            Some(FieldKind::Comments) => {
                for cid in ids_of(v) {
                    let Some((c, _)) = s.rows.get(cid) else { continue };
                    let author = c.get("comment/author").or(c.get("fibery/created-by"))
                        .map(ids_of).and_then(|a| a.first().copied())
                        .and_then(|a| s.user_name(a)).unwrap_or_default();
                    let created = c.get("fibery/creation-date").and_then(|d| d.as_str())
                        .and_then(parse_datetime).unwrap_or(0);
                    let sec = c.get("comment/document-secret").and_then(|x| x.as_str()).and_then(parse_uuid);
                    let md = sec.and_then(|sec| s.doc_markdown(sec));
                    comments.push(json!({"id": format_uuid(cid), "author": author, "created": created,
                        "secret": sec.map(format_uuid), "markdown": md}));
                }
            }
            _ => match v {
                Value::String(t) if !t.is_empty() => fields.push(json!({"name": k, "value": t})),
                Value::Number(_) | Value::Bool(_) => fields.push(json!({"name": k, "value": v})),
                Value::Object(o) if o.contains_key("start") => fields.push(json!({"name": k, "value": v})),
                _ => {}
            },
        }
    }
    comments.sort_by_key(|c| c["created"].as_i64());

    let backlinks: Vec<Value> = s
        .eng
        .backlinks(id, 200)
        .unwrap_or_default()
        .into_iter()
        .map(|(sid, r)| {
            let f = r.rels.iter().find(|(_, t)| *t == id).map(|(f, _)| s.eng.field_name(*f).to_string())
                .unwrap_or_else(|| "Mentioned in".into());
            json!({"field": f, "id": sid, "title": r.title, "db": s.eng.db_name(r.db)})
        })
        .collect();

    let created_by = rec.created_by.and_then(|u| Some(json!({"id": format_uuid(u), "name": s.user_name(u)?})));
    let url = format!(
        "/{}/{}/{}-{}",
        space_name(&dbname).replace(' ', "_"),
        short_name(&dbname).replace(' ', "_"),
        slug(&rec.title),
        rec.public_id
    );
    Ok(Json(json!({
        "id": format_uuid(id), "db": dbname, "title": rec.title, "public_id": rec.public_id,
        "created": rec.created, "updated": rec.updated, "created_by": created_by, "state": rec.state,
        "url": url, "fields": fields, "relations": relations, "people": people, "docs": docs,
        "comments": comments, "backlinks": backlinks,
    }))
    .into_response())
}

#[derive(Deserialize)]
struct ResolveQ {
    db: String,
    pid: String,
}

async fn resolve(State(s): State<St>, Query(p): Query<ResolveQ>) -> ApiResult {
    let nf = || err(StatusCode::NOT_FOUND, "not found");
    let want = p.db.replace('_', " ");
    let db = s
        .eng
        .catalog
        .dbs
        .iter()
        .position(|d| d.name == p.db || d.name == want)
        .ok_or_else(nf)? as u32;
    let pid: u32 = p.pid.parse().map_err(|_| nf())?;
    let id = s.rows.resolve_pid(db, pid).ok_or_else(nf)?;
    Ok(Json(json!({"id": format_uuid(id)})).into_response())
}

async fn dbs(State(s): State<St>) -> Json<Value> {
    let mut spaces: Vec<(String, Vec<Value>)> = vec![];
    let mut list: Vec<_> = s.eng.catalog.searchable().filter(|d| d.entity_count > 0).collect();
    list.sort_by(|a, b| (&a.space, &a.short).cmp(&(&b.space, &b.short)));
    for d in list {
        let item = json!({"name": d.name, "short": d.short, "count": d.entity_count});
        match spaces.last_mut() {
            Some((n, v)) if *n == d.space => v.push(item),
            _ => spaces.push((d.space.clone(), vec![item])),
        }
    }
    let spaces: Vec<Value> = spaces.into_iter().map(|(n, d)| json!({"name": n, "dbs": d})).collect();
    Json(json!({ "spaces": spaces }))
}

async fn syntax(State(s): State<St>) -> Json<Value> {
    Json(json!({"text": s.syntax}))
}

const IMMUTABLE: &str = "public, max-age=31536000, immutable";

async fn file(State(s): State<St>, AxPath(fid): AxPath<String>) -> ApiResult {
    if fid.is_empty() || !fid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Err(err(StatusCode::BAD_REQUEST, "bad file id"));
    }
    let path = s.cache.join(&fid);
    let ctp = s.cache.join(format!("{fid}.ct"));
    let (bytes, ct) = match (tokio::fs::read(&path).await, tokio::fs::read_to_string(&ctp).await) {
        (Ok(b), Ok(ct)) => (b, ct),
        _ => {
            let key = s.api_key.as_deref().ok_or_else(|| e500("no fibery api key"))?;
            let resp = s
                .http
                .get(format!("https://neara.fibery.io/api/files/{fid}"))
                .header("Authorization", format!("Token {key}"))
                .send()
                .await
                .map_err(e500)?;
            if !resp.status().is_success() {
                return Err(err(StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY), "upstream error"));
            }
            let ct = resp
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_string();
            let b = resp.bytes().await.map_err(e500)?.to_vec();
            let _ = tokio::fs::write(&path, &b).await;
            let _ = tokio::fs::write(&ctp, &ct).await;
            (b, ct)
        }
    };
    Ok(([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, IMMUTABLE.to_string())], bytes).into_response())
}

/// Local doc, else a read-only fetch from Fibery cached under web/cache/docs.
async fn get_doc(State(s): State<St>, AxPath(secret): AxPath<String>) -> ApiResult {
    let sec = parse_uuid(&secret).ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad secret"))?;
    let s2 = s.clone();
    if let Some(md) = tokio::task::spawn_blocking(move || s2.doc_markdown(sec)).await.map_err(e500)? {
        return Ok(Json(json!({"markdown": md})).into_response());
    }
    let key = s.api_key.as_deref().ok_or_else(|| e500("no fibery api key"))?;
    let r = s
        .http
        .get(format!("https://neara.fibery.io/api/documents/{}?format=md", format_uuid(sec)))
        .header("Authorization", format!("Token {key}"))
        .send()
        .await
        .map_err(e500)?;
    if !r.status().is_success() {
        return Err(err(StatusCode::NOT_FOUND, format!("upstream {}", r.status())));
    }
    let v: Value = r.json().await.map_err(e500)?;
    let raw = v["content"].as_str().unwrap_or("").to_string();
    let _ = tokio::fs::write(s.docs_cache.join(format!("{}.md", format_uuid(sec))), &raw).await;
    Ok(Json(json!({"markdown": s.rewrite_mentions(&raw)})).into_response())
}

#[derive(Deserialize)]
struct DocBody {
    markdown: String,
}

async fn put_doc(State(s): State<St>, AxPath(secret): AxPath<String>, Json(b): Json<DocBody>) -> ApiResult {
    let sec = parse_uuid(&secret).ok_or_else(|| err(StatusCode::BAD_REQUEST, "bad secret"))?;
    let path = s.data.join("documents").join(format!("{}.md", format_uuid(sec)));
    tokio::fs::write(&path, &b.markdown).await.map_err(e500)?;
    let mut pushed = false;
    if std::env::var("VIBERY_WRITE_THROUGH").as_deref() == Ok("1") {
        let key = s.api_key.as_deref().ok_or_else(|| e500("no fibery api key"))?;
        let r = s
            .http
            .put(format!("https://neara.fibery.io/api/documents/{}?format=md", format_uuid(sec)))
            .header("Authorization", format!("Token {key}"))
            .json(&json!({"content": b.markdown}))
            .send()
            .await
            .map_err(e500)?;
        if !r.status().is_success() {
            return Err(err(StatusCode::BAD_GATEWAY, format!("push failed: {}", r.status())));
        }
        pushed = true;
    }
    Ok(Json(json!({"ok": true, "pushed": pushed})).into_response())
}

/// Hashed vite assets are immutable; everything else (index.html) revalidates.
async fn static_cache(req: Request, next: Next) -> Response {
    let hashed = req.uri().path().starts_with("/assets/");
    let mut res = next.run(req).await;
    let v = if hashed { IMMUTABLE } else { "no-cache" };
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(v));
    res
}

pub fn router(state: St, dist: &Path) -> Router {
    let spa = ServeDir::new(dist).fallback(ServeFile::new(dist.join("index.html")));
    let api = Router::new()
        .route("/api/search", get(search))
        .route("/api/entity/:id", get(entity))
        .route("/api/resolve", get(resolve))
        .route("/api/dbs", get(dbs))
        .route("/api/syntax", get(syntax))
        .route("/api/files/:fid", get(file))
        .route("/api/doc/:secret", get(get_doc).put(put_doc))
        .with_state(state);
    Router::new()
        .merge(api)
        .fallback_service(Router::new().fallback_service(spa).layer(middleware::from_fn(static_cache)))
        .layer(CompressionLayer::new())
}

#[allow(unused)]
fn _body(_: Body) {}
