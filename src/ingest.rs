//! Builds the search index from the raw Fibery JSONL dump.
//!
//! The shape of the problem: prose lives in 2.08M `Collaboration~Documents`
//! rows, discussion lives in 154k `comment` rows, and the things people
//! actually search for are the ~900k domain entities that *point at* those
//! rows. So we do a small number of cheap lookup-table passes, then one wide
//! parallel pass that folds text, actors and edges onto each entity.

use crate::catalog::{Catalog, DbDef, FieldKind};
use crate::index::{actor_term, build_schema, role, scoped_term, Fields};
use crate::store::{EntityRec, Meta, Store, UserRec};
use crate::util::{format_uuid, now_secs, parse_datetime, parse_uuid};
use anyhow::{Context, Result};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use serde_json::Value;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use tantivy::{DateTime, Index, TantivyDocument};

/// Lookup tables built before the main pass.
struct Sidecars {
    /// document secret -> prose
    docs: FxHashMap<u128, Box<str>>,
    /// comment id -> (author id, document secret)
    comments: FxHashMap<u128, (Option<u128>, Option<u128>)>,
    /// enum value id -> label
    enums: FxHashMap<u128, Box<str>>,
    /// workflow state id -> (is_final, lifecycle type: started / finished / …)
    ///
    /// The schema marks terminal states explicitly via `workflow/Final`, which
    /// is far better than guessing from state names: every one of the 347
    /// workflow tables names its states differently.
    states: FxHashMap<u128, (bool, Box<str>)>,
    /// entity id -> number of inbound document references
    inbound: FxHashMap<u128, u32>,
    /// entity id -> entities it is linked to by a document mention, both ways
    mentions: FxHashMap<u128, Vec<u128>>,
    users: Vec<(u128, UserRec)>,
    text_bytes: u64,
    /// How many comment threads had prose we could actually retrieve.
    comments_with_text: u64,
}

/// Read a file and hand each line to `f` in parallel.
///
/// Reads the whole file into memory: the largest input here is 259MB, and this
/// beats per-line locking on a shared reader by a wide margin.
fn par_lines<T, F>(path: &Path, f: F) -> Result<Vec<T>>
where
    T: Send,
    F: Fn(&str) -> Option<T> + Send + Sync,
{
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    Ok(lines.par_iter().filter_map(|l| f(l)).collect())
}

/// Pull the `fibery/id` of a to-one reference: `{"fibery/id": "..."}`.
fn ref_id(v: &Value) -> Option<u128> {
    v.get("fibery/id")?.as_str().and_then(parse_uuid)
}

/// Pull ids out of either a to-one object or a to-many array.
fn ref_ids(v: &Value, out: &mut Vec<u128>) {
    match v {
        Value::Object(_) => {
            if let Some(id) = ref_id(v) {
                out.push(id);
            }
        }
        Value::Array(a) => {
            for e in a {
                if let Some(id) = ref_id(e) {
                    out.push(id);
                }
            }
        }
        _ => {}
    }
}

impl Sidecars {
    fn build(cat: &Catalog, root: &Path) -> Result<Self> {
        let t0 = std::time::Instant::now();

        // --- prose -------------------------------------------------------
        let docs_path = root.join("documents.jsonl");
        let pairs: Vec<(u128, Box<str>)> = if docs_path.exists() {
            par_lines(&docs_path, |line| {
                let v: Value = serde_json::from_str(line).ok()?;
                let content = v.get("content")?.as_str()?;
                if content.is_empty() {
                    return None;
                }
                let secret = parse_uuid(v.get("secret")?.as_str()?)?;
                Some((secret, clean_text(content).into_boxed_str()))
            })?
        } else {
            Vec::new()
        };
        let text_bytes: u64 = pairs.iter().map(|(_, s)| s.len() as u64).sum();
        let docs: FxHashMap<u128, Box<str>> = pairs.into_iter().collect();
        eprintln!(
            "  prose      {:>9} documents, {:>6} MB  ({:.1}s)",
            docs.len(),
            text_bytes / 1_000_000,
            t0.elapsed().as_secs_f64()
        );

        // --- comments ----------------------------------------------------
        let t1 = std::time::Instant::now();
        let cpath = root.join("databases/comments__comment.jsonl");
        let comments: FxHashMap<u128, (Option<u128>, Option<u128>)> = if cpath.exists() {
            par_lines(&cpath, |line| {
                let v: Value = serde_json::from_str(line).ok()?;
                let id = parse_uuid(v.get("fibery/id")?.as_str()?)?;
                let author = v
                    .get("comment/author")
                    .and_then(ref_id)
                    .or_else(|| v.get("fibery/created-by").and_then(ref_id));
                let secret = v
                    .get("comment/document-secret")
                    .and_then(|s| s.as_str())
                    .and_then(parse_uuid);
                Some((id, (author, secret)))
            })?
            .into_iter()
            .collect()
        } else {
            FxHashMap::default()
        };
        // A comment's text lives in its own document, keyed by
        // `comment/document-secret`. Those secrets are NOT the same population
        // as the Document rows' secrets, so a dump that only fetched the latter
        // has comment *authorship* but no comment *prose*. Detect and report
        // that rather than silently indexing an empty field.
        let with_text = comments
            .values()
            .filter(|(_, sec)| sec.is_some_and(|s| docs.contains_key(&s)))
            .count();
        eprintln!(
            "  comments   {:>9} threads, {} with retrievable text  ({:.1}s)",
            comments.len(),
            with_text,
            t1.elapsed().as_secs_f64()
        );
        if comments.len() > 1000 && with_text * 20 < comments.len() {
            eprintln!(
                "             \x1b[33mnote: comment prose is absent from this dump\x1b[0m"
            );
            eprintln!(
                "             \x1b[2m`commented-by:` still works (authorship comes from the"
            );
            eprintln!(
                "             comment rows); `comments:<term>` cannot match anything until"
            );
            eprintln!(
                "             the scraper also fetches comment/document-secret bodies.\x1b[2m\x1b[0m"
            );
        }

        // --- enum labels -------------------------------------------------
        let t2 = std::time::Instant::now();
        let enum_dbs: Vec<&DbDef> = cat
            .dbs
            .iter()
            .filter(|d| d.is_enum && d.file.is_some())
            .collect();
        type EnumRow = (u128, Box<str>, Option<(bool, Box<str>)>);
        let rows: Vec<EnumRow> = enum_dbs
            .par_iter()
            .flat_map(|d| {
                let title = d.title_field.clone().unwrap_or_else(|| "enum/name".into());
                let path = d.file.clone().expect("filtered on is_some");
                par_lines(&path, |line| {
                    let v: Value = serde_json::from_str(line).ok()?;
                    let id = parse_uuid(v.get("fibery/id")?.as_str()?)?;
                    let label = v.get(&title)?.as_str()?;
                    if label.is_empty() {
                        return None;
                    }
                    // Only workflow-state tables carry `workflow/Final`.
                    let wf = v.get("workflow/Final").and_then(|b| b.as_bool()).map(|fin| {
                        let ty = v
                            .get("workflow/Type")
                            .and_then(|s| s.as_str())
                            .unwrap_or("unknown")
                            .to_lowercase()
                            .replace(' ', "-");
                        (fin, ty.into_boxed_str())
                    });
                    Some((id, label.to_string().into_boxed_str(), wf))
                })
                .unwrap_or_default()
            })
            .collect();
        let mut enums: FxHashMap<u128, Box<str>> = FxHashMap::default();
        let mut states: FxHashMap<u128, (bool, Box<str>)> = FxHashMap::default();
        for (id, label, wf) in rows {
            enums.insert(id, label);
            if let Some(w) = wf {
                states.insert(id, w);
            }
        }
        eprintln!(
            "  enums      {:>9} labels from {} tables ({} workflow states) ({:.1}s)",
            enums.len(),
            enum_dbs.len(),
            states.len(),
            t2.elapsed().as_secs_f64()
        );

        // --- inbound reference counts ------------------------------------
        // `Collaboration~Documents/Reference` records "document X mentions
        // entity Y". Counting them gives a cheap, meaningful authority signal.
        let t3 = std::time::Instant::now();
        let rpath = root.join("databases/Collaboration~Documents__Reference.jsonl");
        let mut inbound: FxHashMap<u128, u32> = FxHashMap::default();
        let mut mentions: FxHashMap<u128, Vec<u128>> = FxHashMap::default();
        if rpath.exists() {
            let edges: Vec<(u128, u128)> = par_lines(&rpath, |line| {
                let v: Value = serde_json::from_str(line).ok()?;
                let from = v
                    .get("Collaboration~Documents/FromEntityId")?
                    .as_str()
                    .and_then(parse_uuid)?;
                let to = v
                    .get("Collaboration~Documents/ToEntityId")?
                    .as_str()
                    .and_then(parse_uuid)?;
                (from != to).then_some((from, to))
            })?;
            for (from, to) in edges {
                *inbound.entry(to).or_insert(0) += 1;
                // Symmetric: a mention relates the two entities in both
                // directions, so `related:(X)` finds them from either side.
                mentions.entry(from).or_default().push(to);
                mentions.entry(to).or_default().push(from);
            }
            for v in mentions.values_mut() {
                v.sort_unstable();
                v.dedup();
            }
        }
        eprintln!(
            "  mentions   {:>9} entities, {} referenced   ({:.1}s)",
            mentions.len(),
            inbound.len(),
            t3.elapsed().as_secs_f64()
        );

        // --- users -------------------------------------------------------
        let upath = root.join("databases/fibery__user.jsonl");
        let users: Vec<(u128, UserRec)> = if upath.exists() {
            par_lines(&upath, |line| {
                let v: Value = serde_json::from_str(line).ok()?;
                let id = parse_uuid(v.get("fibery/id")?.as_str()?)?;
                Some((
                    id,
                    UserRec {
                        name: v
                            .get("user/name")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        email: v
                            .get("user/email")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string(),
                        active: v
                            .get("fibery/active?")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false),
                    },
                ))
            })?
        } else {
            Vec::new()
        };
        eprintln!("  users      {:>9} accounts", users.len());

        Ok(Sidecars {
            docs,
            comments,
            enums,
            states,
            inbound,
            mentions,
            users,
            text_bytes,
            comments_with_text: with_text as u64,
        })
    }
}

/// Strip the markdown/HTML artefacts that would otherwise pollute both the
/// index and the snippets: image embeds, angle-bracket autolinks, `<br>`.
fn clean_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        // Fibery entity mention: `[[label#^typeid/entityid]]` or
        // `[[#@typeid/entityid]]`. Keep the label, drop the raw uuids — they
        // are already in the index as `rel` terms and only add noise to text.
        if b[i] == b'[' && i + 1 < b.len() && b[i + 1] == b'[' {
            if let Some(close) = s[i..].find("]]") {
                let inner = &s[i + 2..i + close];
                let label = inner
                    .find("#^")
                    .or_else(|| inner.find("#@"))
                    .map(|c| &inner[..c])
                    .unwrap_or(inner);
                out.push_str(label.trim());
                out.push(' ');
                i += close + 2;
                continue;
            }
        }
        // `![alt](url)` -> drop entirely
        if b[i] == b'!' && i + 1 < b.len() && b[i + 1] == b'[' {
            if let Some(close) = s[i..].find(')') {
                i += close + 1;
                out.push(' ');
                continue;
            }
        }
        // `[text](url)` -> keep `text`
        if b[i] == b'[' {
            if let (Some(rb), Some(rp)) = (s[i..].find(']'), s[i..].find(')')) {
                if rp > rb && s.as_bytes().get(i + rb + 1) == Some(&b'(') {
                    out.push_str(&s[i + 1..i + rb]);
                    i += rp + 1;
                    continue;
                }
            }
        }
        // `<br>` / `<br/>` -> newline; other bare tags -> space
        if b[i] == b'<' {
            if let Some(gt) = s[i..].find('>') {
                let inner = &s[i + 1..i + gt];
                if inner.starts_with("br") {
                    out.push('\n');
                    i += gt + 1;
                    continue;
                }
                // Autolink `<https://...>`: keep the URL, it is searchable.
                if inner.starts_with("http") {
                    out.push_str(inner);
                    i += gt + 1;
                    continue;
                }
                if inner.len() < 20 && !inner.contains(' ') {
                    out.push(' ');
                    i += gt + 1;
                    continue;
                }
            }
        }
        let ch = s[i..].chars().next().unwrap_or(' ');
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Squash whitespace and clip, for a one-line result preview.
fn snippet_of(s: &str, max: usize) -> String {
    let mut out = String::with_capacity(max + 8);
    let mut space = true;
    for ch in s.chars() {
        if out.chars().count() >= max {
            out.push('…');
            break;
        }
        if ch.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(ch);
            space = false;
        }
    }
    out.trim().to_string()
}

/// Normalise a field name into what a user would type: `Engineering/Due Date`
/// becomes `due-date`.
pub fn field_key(name: &str) -> String {
    crate::catalog::short_name(name)
        .to_lowercase()
        .replace(' ', "-")
}

pub struct IngestOpts {
    pub root: std::path::PathBuf,
    pub out: std::path::PathBuf,
    pub heap_mb: usize,
    pub threads: usize,
    /// Cap entities per database; for quick smoke tests.
    pub limit: Option<usize>,
}

pub fn run(opts: &IngestOpts) -> Result<Meta> {
    let t_start = std::time::Instant::now();
    std::fs::create_dir_all(&opts.out).context("create index dir")?;

    eprintln!("Loading schema...");
    let cat = Catalog::load(&opts.root)?;
    let searchable: Vec<&DbDef> = cat.searchable().collect();
    eprintln!(
        "  {} types, {} searchable databases",
        cat.dbs.len(),
        searchable.len()
    );

    eprintln!("Building lookup tables...");
    let side = Sidecars::build(&cat, &opts.root)?;

    // --- open the two stores -------------------------------------------
    let store_path = opts.out.join("store.redb");
    let _ = std::fs::remove_file(&store_path);
    let store = Store::create(&store_path)?;
    store.put_users(&side.users)?;

    let tv_path = opts.out.join("tantivy");
    if tv_path.exists() {
        std::fs::remove_dir_all(&tv_path)?;
    }
    std::fs::create_dir_all(&tv_path)?;
    let (schema, f) = build_schema();
    let index = Index::create_in_dir(&tv_path, schema)?;
    // Must happen before the writer is built: the schema names this tokenizer
    // for title/body/comments, and an unregistered name is a hard error.
    index
        .tokenizers()
        .register(crate::text::TOKENIZER, crate::text::analyzer());
    let writer = index.writer_with_num_threads(
        opts.threads.clamp(1, 8),
        opts.heap_mb * 1_000_000,
    )?;

    // Field names are interned so EntityRec can store `(u32, u128)` edges.
    let field_names: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let field_ids: Mutex<FxHashMap<String, u32>> = Mutex::new(FxHashMap::default());
    let intern = |name: &str| -> u32 {
        let mut ids = field_ids.lock().expect("field_ids poisoned");
        if let Some(&i) = ids.get(name) {
            return i;
        }
        let mut names = field_names.lock().expect("field_names poisoned");
        let i = names.len() as u32;
        names.push(name.to_string());
        ids.insert(name.to_string(), i);
        i
    };

    let scanned = AtomicU64::new(0);
    let indexed = AtomicU64::new(0);
    let db_index = cat.name_index();

    eprintln!("Indexing {} databases...", searchable.len());
    let t_idx = std::time::Instant::now();

    // One task per database; the biggest (HubSpot/Email, 173k) sets the tail.
    let results: Result<Vec<()>> = searchable
        .par_iter()
        .map(|db| -> Result<()> {
            let path = db.file.as_ref().expect("searchable implies file");
            let dbi = *db_index.get(&db.name).expect("db in name index");
            let bytes = std::fs::read(path)
                .with_context(|| format!("read {}", path.display()))?;
            let text = String::from_utf8_lossy(&bytes);

            let mut batch: Vec<(u128, EntityRec)> = Vec::with_capacity(4096);
            let mut texts: Vec<(u128, String, String)> = Vec::with_capacity(4096);
            let mut n = 0usize;

            for line in text.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(lim) = opts.limit {
                    if n >= lim {
                        break;
                    }
                }
                n += 1;
                scanned.fetch_add(1, Ordering::Relaxed);

                let v: Value = match serde_json::from_str(line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let Some(id) = v.get("fibery/id").and_then(|s| s.as_str()).and_then(parse_uuid)
                else {
                    continue;
                };

                let built = build_entity(db, dbi, id, &v, &side, &f, &intern);
                if let Some((tdoc, rec, body, comments)) = built {
                    writer.add_document(tdoc)?;
                    batch.push((id, rec));
                    if !body.is_empty() || !comments.is_empty() {
                        texts.push((id, body, comments));
                    }
                    indexed.fetch_add(1, Ordering::Relaxed);
                }

                if batch.len() >= 4096 {
                    store.put_entities(&batch)?;
                    batch.clear();
                    store.put_texts(&texts)?;
                    texts.clear();
                }
            }
            if !batch.is_empty() {
                store.put_entities(&batch)?;
            }
            if !texts.is_empty() {
                store.put_texts(&texts)?;
            }
            Ok(())
        })
        .collect();
    results?;

    let n_indexed = indexed.load(Ordering::Relaxed);
    eprintln!(
        "  {} entities -> index in {:.1}s",
        n_indexed,
        t_idx.elapsed().as_secs_f64()
    );

    eprintln!("Committing index (merge + fsync)...");
    let t_c = std::time::Instant::now();
    let mut writer = writer;
    writer.commit()?;
    writer.wait_merging_threads()?;
    eprintln!("  committed in {:.1}s", t_c.elapsed().as_secs_f64());

    let meta = Meta {
        db_names: cat.dbs.iter().map(|d| d.name.clone()).collect(),
        field_names: field_names.into_inner().expect("field_names poisoned"),
        entities_indexed: n_indexed,
        entities_scanned: scanned.load(Ordering::Relaxed),
        documents_loaded: side.docs.len() as u64,
        comments_loaded: side.comments.len() as u64,
        comments_with_text: side.comments_with_text,
        text_bytes: side.text_bytes,
        ingest_secs: t_start.elapsed().as_secs_f64(),
        built_at: now_secs(),
    };
    store.put_meta(&meta)?;
    store.put_catalog(&cat)?;
    Ok(meta)
}

/// Fold one raw entity into a Tantivy document plus a redb record.
#[allow(clippy::too_many_arguments)]
fn build_entity(
    db: &DbDef,
    dbi: u32,
    id: u128,
    v: &Value,
    side: &Sidecars,
    f: &Fields,
    intern: &dyn Fn(&str) -> u32,
) -> Option<(TantivyDocument, EntityRec, String, String)> {
    let mut d = TantivyDocument::default();
    let id_str = format_uuid(id);
    d.add_text(f.id, &id_str);
    d.add_text(f.db, &db.name);
    d.add_text(f.space, &db.space);

    let mut rec = EntityRec {
        db: dbi,
        ..Default::default()
    };

    // --- title ---------------------------------------------------------
    let title = db
        .title_field
        .as_deref()
        .and_then(|tf| v.get(tf))
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim();
    d.add_text(f.title, title);
    rec.title = title.to_string();

    if let Some(pid) = v.get("fibery/public-id").and_then(|s| s.as_str()) {
        d.add_text(f.public_id, pid);
        rec.public_id = pid.to_string();
    }

    // --- timestamps ----------------------------------------------------
    let created = v
        .get("fibery/creation-date")
        .and_then(|s| s.as_str())
        .and_then(parse_datetime)
        .unwrap_or(0);
    let updated = v
        .get("fibery/modification-date")
        .and_then(|s| s.as_str())
        .and_then(parse_datetime)
        .unwrap_or(created);
    d.add_date(f.created_at, DateTime::from_timestamp_secs(created));
    d.add_date(f.updated_at, DateTime::from_timestamp_secs(updated));
    rec.created = created;
    rec.updated = updated;

    // --- actors --------------------------------------------------------
    // Every `fibery/user`-typed field is a participation edge, whatever it is
    // called. That is what makes "what has my teammate worked on" answerable
    // without hand-listing roles.
    let mut touched: Vec<u128> = Vec::new();
    let mut has_assignee = false;
    if let Some(cb) = v.get("fibery/created-by").and_then(ref_id) {
        d.add_text(f.actor, actor_term(role::CREATED, &format_uuid(cb)));
        touched.push(cb);
        rec.created_by = Some(cb);
    }

    // --- body text -----------------------------------------------------
    let mut body = String::new();
    let mut comment_text = String::new();
    let mut has_description = false;
    let mut rank_flags: u64 = 0;
    let mut ids_buf: Vec<u128> = Vec::new();

    for fd in &db.fields {
        let Some(val) = v.get(&fd.name) else { continue };
        if val.is_null() {
            continue;
        }
        match fd.kind {
            FieldKind::Title => {}
            FieldKind::Text => {
                if let Some(s) = val.as_str() {
                    if !s.is_empty() && s.len() < 20_000 {
                        body.push_str(s);
                        body.push('\n');
                    }
                }
            }
            FieldKind::RichText => {
                // The handle carries the secret; the prose is in the sidecar.
                if let Some(secret) = val
                    .get("Collaboration~Documents/secret")
                    .and_then(|s| s.as_str())
                    .and_then(parse_uuid)
                {
                    if let Some(text) = side.docs.get(&secret) {
                        body.push_str(text);
                        body.push('\n');
                        has_description = true;
                    }
                }
            }
            FieldKind::Comments => {
                ids_buf.clear();
                ref_ids(val, &mut ids_buf);
                for cid in &ids_buf {
                    if let Some((author, secret)) = side.comments.get(cid) {
                        if let Some(a) = author {
                            d.add_text(
                                f.actor,
                                actor_term(role::COMMENTED, &format_uuid(*a)),
                            );
                            touched.push(*a);
                        }
                        if let Some(s) = secret {
                            if let Some(text) = side.docs.get(s) {
                                comment_text.push_str(text);
                                comment_text.push('\n');
                            }
                        }
                    }
                }
                if !ids_buf.is_empty() {
                    d.add_text(f.has, "comments");
                }
            }
            FieldKind::User => {
                ids_buf.clear();
                ref_ids(val, &mut ids_buf);
                let key = field_key(&fd.name);
                for uid in &ids_buf {
                    let us = format_uuid(*uid);
                    d.add_text(f.actor, actor_term(&key, &us));
                    touched.push(*uid);
                }
                if !ids_buf.is_empty() {
                    has_assignee = true;
                }
            }
            FieldKind::Enum => {
                if let Some(eid) = ref_id(val) {
                    if let Some(label) = side.enums.get(&eid) {
                        let key = field_key(&fd.name);
                        d.add_text(f.enumv, scoped_term(&key, &label.to_lowercase()));
                        if fd.name == "workflow/state" {
                            d.add_text(f.state, label.as_ref());
                            rec.state = Some(label.to_string());
                            // `is:open` / `is:closed` come from the schema's
                            // own terminal-state flag, not from state names.
                            if let Some((is_final, ty)) = side.states.get(&eid) {
                                d.add_text(
                                    f.enumv,
                                    scoped_term("workflow", if *is_final { "final" } else { "open" }),
                                );
                                d.add_text(f.enumv, scoped_term("workflow-type", ty));
                                if *is_final {
                                    rank_flags |= crate::index::flags::FINAL;
                                }
                                if ty.as_ref() == "not-started" {
                                    rank_flags |= crate::index::flags::NOT_STARTED;
                                }
                            }
                        }
                    }
                }
            }
            FieldKind::Relation => {
                ids_buf.clear();
                ref_ids(val, &mut ids_buf);
                let key = field_key(&fd.name);
                let fid = intern(&fd.name);
                for tid in &ids_buf {
                    let ts = format_uuid(*tid);
                    d.add_text(f.rel, &ts);
                    d.add_text(f.relf, scoped_term(&key, &ts));
                    rec.rels.push((fid, *tid));
                }
            }
            _ => {}
        }
    }

    // Document mentions are relations too: "this note names that bet".
    if let Some(ms) = side.mentions.get(&id) {
        for m in ms {
            d.add_text(f.rel, format_uuid(*m));
        }
    }

    // Role-agnostic actor edge, deduped.
    touched.sort_unstable();
    touched.dedup();
    for u in &touched {
        d.add_text(f.actor, actor_term(role::TOUCHED, &format_uuid(*u)));
    }

    if has_description {
        d.add_text(f.has, "description");
    }
    if has_assignee {
        d.add_text(f.has, "assignee");
    }

    // An entity with no title and no prose is structural noise (join rows,
    // empty placeholders) — indexing it only dilutes results.
    if title.is_empty() && body.trim().is_empty() && comment_text.trim().is_empty() {
        return None;
    }

    if !body.is_empty() {
        d.add_text(f.body, &body);
    }
    if !comment_text.is_empty() {
        d.add_text(f.comments, &comment_text);
    }

    let snip = snippet_of(if body.trim().is_empty() { &comment_text } else { &body }, 180);
    d.add_text(f.snippet, &snip);
    rec.snippet = snip;

    let inb = side.inbound.get(&id).copied().unwrap_or(0);
    d.add_u64(f.inbound, inb as u64);

    // Ranking inputs: how much substance the entity has, and its lifecycle.
    let text_len = (body.len() + comment_text.len()) as u64;
    d.add_u64(f.text_len, text_len);
    if !body.trim().is_empty() {
        rank_flags |= crate::index::flags::HAS_BODY;
    }
    d.add_u64(f.rank_flags, rank_flags);

    Some((d, rec, body, comment_text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_is_flattened() {
        assert_eq!(clean_text("![x.png](/api/files/abc)").trim(), "");
        assert_eq!(clean_text("see [the doc](http://x/y) now"), "see the doc now");
        assert_eq!(clean_text("a<br>\nb"), "a\n\nb");
        assert!(clean_text("<https://example.com/z>").contains("example.com/z"));
    }

    #[test]
    fn entity_mentions_keep_their_label_and_drop_uuids() {
        // Labelled mention: keep the readable part.
        let labelled = clean_text(
            "see [[Shared/Note: Fix the thing#^b85855a0-4515-11ed-959d-473694b08023/ff7dd5e2-0e50-4aeb-8ffe-9e20f2175e7b]] ok",
        );
        assert!(labelled.contains("Shared/Note: Fix the thing"), "got {labelled:?}");
        assert!(!labelled.contains("b85855a0"), "uuid leaked: {labelled:?}");

        // Bare user mention: nothing readable, so nothing kept.
        let bare = clean_text(
            "Author: [[#@1b728600-af9c-11e9-95b5-2985503542d7/dd81ef00-5605-11ef-a26a-692e722be878]]",
        );
        assert!(!bare.contains("1b728600"), "uuid leaked: {bare:?}");
        assert_eq!(bare.trim(), "Author:");
    }

    #[test]
    fn single_bracket_links_are_still_links() {
        assert_eq!(clean_text("see [the doc](http://x/y) now"), "see the doc now");
    }

    #[test]
    fn snippets_squash_whitespace_and_clip() {
        assert_eq!(snippet_of("a   \n\n b", 80), "a b");
        let long = "x".repeat(500);
        let s = snippet_of(&long, 10);
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 11);
    }

    #[test]
    fn field_keys_match_what_users_type() {
        assert_eq!(field_key("Engineering/Due Date"), "due-date");
        assert_eq!(field_key("assignments/assignees"), "assignees");
    }
}
