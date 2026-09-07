//! Query planning and execution.
//!
//! The AST from `query.rs` is compiled into a Tantivy query. Two nodes need
//! more than a term lookup:
//!
//! * `Actor` resolves a typed name (`@sami`) against the user table, so you
//!   never have to paste a UUID.
//! * `Related` runs its subquery first, collects the matching ids, and rewrites
//!   itself into a disjunction over the `rel` field. One extra index pass per
//!   hop, and hops compose.

use crate::catalog::Catalog;
use crate::index::{actor_term, scoped_term, Fields};
use crate::query::{DateField, Node, Sort, Spec, TextScope};
use crate::store::{EntityRec, Meta, Store, UserRec};
use crate::util::{format_uuid, parse_uuid};
use anyhow::{Context, Result};
use rustc_hash::FxHashMap;
use std::collections::Bound;
use std::path::Path;
use tantivy::collector::{Collector, Count, SegmentCollector, TopDocs};
use tantivy::columnar::StrColumn;
use tantivy::query::{
    AllQuery, BooleanQuery, BoostQuery, EmptyQuery, Occur, PhraseQuery, Query, RangeQuery,
    RegexQuery, TermQuery,
};
use tantivy::schema::{IndexRecordOption, Value as _};
use tantivy::{DateTime, DocId, Score, SegmentOrdinal, SegmentReader, TantivyDocument, Term};

/// Boosts: a title hit is worth far more than a body hit, and a hit buried in
/// a comment thread is worth less than either.
const BOOST_TITLE: Score = 6.0;
const BOOST_BODY: Score = 1.0;
const BOOST_COMMENTS: Score = 0.4;

/// Cap on ids pulled back from a `related:(...)` subquery. Beyond this the
/// disjunction stops being worth it and the filter is effectively "anything".
const REL_SUBQUERY_CAP: usize = 4096;

pub struct Engine {
    pub index: tantivy::Index,
    pub reader: tantivy::IndexReader,
    pub fields: Fields,
    pub store: Store,
    pub catalog: Catalog,
    pub meta: Meta,
    /// Lowercased name/email fragments -> user id, for `@name` resolution.
    users: Vec<(u128, UserRec)>,
}

#[derive(Debug, Clone)]
pub struct Hit {
    pub id: String,
    pub db: String,
    pub title: String,
    pub public_id: String,
    pub snippet: String,
    pub created: i64,
    pub updated: i64,
    pub score: Score,
    pub state: Option<String>,
    pub inbound: u64,
}

#[derive(Debug, Clone)]
pub struct Results {
    pub hits: Vec<Hit>,
    pub total: usize,
    pub by_db: Vec<(String, u64)>,
    pub took_ms: f64,
    pub plan: String,
    pub warnings: Vec<String>,
}

impl Engine {
    pub fn open(dir: &Path) -> Result<Self> {
        let store = Store::open(&dir.join("store.redb"))?;
        let catalog = store.get_catalog()?;
        let meta = store.get_meta()?;
        let index = tantivy::Index::open_in_dir(dir.join("tantivy"))
            .with_context(|| format!("open tantivy index in {}", dir.display()))?;
        let fields = Fields::from_schema(&index.schema())?;
        let reader = index
            .reader_builder()
            .reload_policy(tantivy::ReloadPolicy::Manual)
            .try_into()?;
        let users = store.all_users()?;
        Ok(Engine {
            index,
            reader,
            fields,
            store,
            catalog,
            meta,
            users,
        })
    }

    /// Resolve what someone typed after `@` to user ids.
    ///
    /// Scored rather than tiered. The tiered version had a real bug: `@michael`
    /// matched the `michael@fibery.io` service account on its email local part
    /// and shadowed all nine humans called Michael. Matching a *name word*
    /// therefore outranks matching an email local part.
    pub fn resolve_users(&self, needle: &str) -> Vec<(u128, &UserRec)> {
        let raw = needle.trim().trim_matches('"').to_lowercase();
        if raw.is_empty() {
            return Vec::new();
        }
        // `michael.tokar` / `michael-tokar` should read as a full name too.
        let norm = raw.replace(['-', '_', '.'], " ");

        if let Some(id) = parse_uuid(&raw) {
            if let Some((i, u)) = self.users.iter().find(|(uid, _)| *uid == id) {
                return vec![(*i, u)];
            }
        }

        let mut scored: Vec<(u32, bool, u128, &UserRec)> = self
            .users
            .iter()
            .filter_map(|(id, u)| {
                let sc = score_user(&raw, &norm, &u.name, &u.email);
                (sc > 0).then_some((sc, u.active, *id, u))
            })
            .collect();
        if scored.is_empty() {
            return Vec::new();
        }
        let best = scored.iter().map(|(s, ..)| *s).max().unwrap_or(0);
        scored.retain(|(s, ..)| *s == best);
        // Active accounts first, then alphabetical, so output is stable.
        scored.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.3.name.cmp(&b.3.name)));
        scored.into_iter().map(|(_, _, id, u)| (id, u)).collect()
    }

    // ------------------------------------------------------------ planning

    fn text_query(&self, word: &str, scope: TextScope, prefix: bool) -> Box<dyn Query> {
        let f = &self.fields;
        let targets: Vec<(tantivy::schema::Field, Score)> = match scope {
            TextScope::Any => vec![
                (f.title, BOOST_TITLE),
                (f.body, BOOST_BODY),
                (f.comments, BOOST_COMMENTS),
            ],
            TextScope::Title => vec![(f.title, BOOST_TITLE)],
            TextScope::Body => vec![(f.body, BOOST_BODY)],
            TextScope::Comments => vec![(f.comments, BOOST_COMMENTS)],
        };
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::with_capacity(targets.len());
        for (field, boost) in targets {
            let q: Box<dyn Query> = if prefix {
                // Prefix search over the term dictionary FST. This is what makes
                // "it updates the filtering as I type" work on the last word.
                match RegexQuery::from_pattern(&format!("{}.*", regex_escape(word)), field) {
                    Ok(q) => Box::new(q),
                    Err(_) => Box::new(EmptyQuery),
                }
            } else {
                Box::new(TermQuery::new(
                    Term::from_field_text(field, word),
                    IndexRecordOption::WithFreqs,
                ))
            };
            clauses.push((Occur::Should, Box::new(BoostQuery::new(q, boost))));
        }
        Box::new(BooleanQuery::new(clauses))
    }

    fn phrase_query(&self, words: &[String], scope: TextScope) -> Box<dyn Query> {
        let f = &self.fields;
        let targets: Vec<(tantivy::schema::Field, Score)> = match scope {
            TextScope::Any => vec![
                (f.title, BOOST_TITLE),
                (f.body, BOOST_BODY),
                (f.comments, BOOST_COMMENTS),
            ],
            TextScope::Title => vec![(f.title, BOOST_TITLE)],
            TextScope::Body => vec![(f.body, BOOST_BODY)],
            TextScope::Comments => vec![(f.comments, BOOST_COMMENTS)],
        };
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (field, boost) in targets {
            let terms: Vec<Term> = words
                .iter()
                .map(|w| Term::from_field_text(field, w))
                .collect();
            let q: Box<dyn Query> = Box::new(PhraseQuery::new(terms));
            clauses.push((Occur::Should, Box::new(BoostQuery::new(q, boost))));
        }
        Box::new(BooleanQuery::new(clauses))
    }

    fn term(&self, field: tantivy::schema::Field, text: &str) -> Box<dyn Query> {
        Box::new(TermQuery::new(
            Term::from_field_text(field, text),
            IndexRecordOption::Basic,
        ))
    }

    /// Disjunction helper; an empty input matches nothing (rather than
    /// everything, which would silently widen the query).
    fn any_of(&self, qs: Vec<Box<dyn Query>>) -> Box<dyn Query> {
        if qs.is_empty() {
            return Box::new(EmptyQuery);
        }
        if qs.len() == 1 {
            return qs.into_iter().next().expect("len checked");
        }
        Box::new(BooleanQuery::new(
            qs.into_iter().map(|q| (Occur::Should, q)).collect(),
        ))
    }

    /// Compile one AST node. `notes` accumulates human-readable plan lines and
    /// resolution warnings (e.g. "`@sami` -> Sami Kyostila").
    fn compile(&self, node: &Node, notes: &mut Vec<String>) -> Result<Box<dyn Query>> {
        let f = &self.fields;
        Ok(match node {
            Node::All => Box::new(AllQuery),

            Node::And(parts) => {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for p in parts {
                    // `-x` inside a conjunction becomes MustNot rather than a
                    // nested boolean, which keeps the query fast.
                    if let Node::Not(inner) = p {
                        clauses.push((Occur::MustNot, self.compile(inner, notes)?));
                    } else {
                        clauses.push((Occur::Must, self.compile(p, notes)?));
                    }
                }
                if clauses.iter().all(|(o, _)| *o == Occur::MustNot) {
                    clauses.push((Occur::Must, Box::new(AllQuery)));
                }
                Box::new(BooleanQuery::new(clauses))
            }

            Node::Or(parts) => {
                let mut qs = Vec::with_capacity(parts.len());
                for p in parts {
                    qs.push(self.compile(p, notes)?);
                }
                self.any_of(qs)
            }

            Node::Not(inner) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(AllQuery) as Box<dyn Query>),
                (Occur::MustNot, self.compile(inner, notes)?),
            ])),

            Node::Word {
                word,
                scope,
                prefix,
            } => self.text_query(word, *scope, *prefix),

            Node::Phrase { words, scope } => self.phrase_query(words, *scope),

            Node::Db(needle) => {
                let dbs = self.catalog.resolve_db(needle);
                if dbs.is_empty() {
                    notes.push(format!("#{needle} matched no database"));
                    return Ok(Box::new(EmptyQuery));
                }
                notes.push(format!(
                    "#{needle} -> {}",
                    dbs.iter()
                        .map(|d| d.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                self.any_of(dbs.iter().map(|d| self.term(f.db, &d.name)).collect())
            }

            Node::Space(needle) => {
                let n = needle.to_lowercase();
                let spaces: Vec<String> = {
                    let mut s: Vec<String> = self
                        .catalog
                        .searchable()
                        .map(|d| d.space.clone())
                        .filter(|s| s.to_lowercase().starts_with(&n))
                        .collect();
                    s.sort();
                    s.dedup();
                    s
                };
                if spaces.is_empty() {
                    notes.push(format!("space `{needle}` matched nothing"));
                    return Ok(Box::new(EmptyQuery));
                }
                notes.push(format!("space `{needle}` -> {}", spaces.join(", ")));
                self.any_of(spaces.iter().map(|s| self.term(f.space, s)).collect())
            }

            Node::Actor { role, user } => {
                let matched = self.resolve_users(user);
                if matched.is_empty() {
                    notes.push(format!("@{user} matched no user"));
                    return Ok(Box::new(EmptyQuery));
                }
                let names: Vec<&str> =
                    matched.iter().map(|(_, u)| u.name.as_str()).collect();
                notes.push(if names.len() == 1 {
                    format!("@{user} -> {}", names[0])
                } else {
                    // Ambiguity is surfaced, not silently resolved: the query
                    // ORs over all of them and says so.
                    format!(
                        "@{user} -> {} users: {}{}",
                        names.len(),
                        names.iter().take(5).copied().collect::<Vec<_>>().join(", "),
                        if names.len() > 5 { ", …" } else { "" }
                    )
                });
                // Role-agnostic queries hit the `touched` edge, which every
                // actor relationship also writes.
                let roles: Vec<String> = match role.as_deref() {
                    None => vec![crate::index::role::TOUCHED.to_string()],
                    Some(r) => role_aliases(r),
                };
                let mut qs = Vec::new();
                for (uid, _) in &matched {
                    let us = format_uuid(*uid);
                    for r in &roles {
                        qs.push(self.term(f.actor, &actor_term(r, &us)));
                    }
                }
                self.any_of(qs)
            }

            Node::Related { field, sub } => {
                // A *named* relation can only point at certain types, and the
                // catalog knows which. Constraining the subquery to those types
                // makes `team:(platform)` mean "linked via a team-ish field to a
                // Team matching platform" instead of "to anything, anywhere":
                // far more precise, and it keeps the anchor set small.
                let mut inner = self.compile(sub, notes)?;
                if let Some(fname) = field {
                    let aliases = role_aliases(fname);
                    let mut targets: Vec<&str> = self
                        .catalog
                        .searchable()
                        .flat_map(|d| d.fields.iter())
                        .filter(|fd| {
                            fd.kind == crate::catalog::FieldKind::Relation
                                && aliases.contains(&crate::ingest::field_key(&fd.name))
                        })
                        .map(|fd| fd.target.as_str())
                        .collect();
                    targets.sort_unstable();
                    targets.dedup();
                    if !targets.is_empty() {
                        notes.push(format!(
                            "{fname}:(...) targets {}",
                            targets.join(", ")
                        ));
                        let type_filter = self.any_of(
                            targets.iter().map(|t| self.term(f.db, t)).collect(),
                        );
                        inner = Box::new(BooleanQuery::new(vec![
                            (Occur::Must, inner),
                            (Occur::Must, type_filter),
                        ]));
                    }
                }
                let searcher = self.reader.searcher();
                let top = searcher.search(&inner, &TopDocs::with_limit(REL_SUBQUERY_CAP))?;
                let mut ids: Vec<String> = Vec::with_capacity(top.len());
                for (_, addr) in top {
                    let doc: TantivyDocument = searcher.doc(addr)?;
                    if let Some(v) = doc.get_first(f.id).and_then(|v| v.as_str()) {
                        ids.push(v.to_string());
                    }
                }
                notes.push(format!(
                    "related:(...) -> {} anchor entit{}{}",
                    ids.len(),
                    if ids.len() == 1 { "y" } else { "ies" },
                    // Silent truncation would quietly give wrong answers.
                    if ids.len() >= REL_SUBQUERY_CAP {
                        " (CAPPED — results incomplete, narrow the subquery)"
                    } else {
                        ""
                    }
                ));
                if ids.is_empty() {
                    return Ok(Box::new(EmptyQuery));
                }
                let qs: Vec<Box<dyn Query>> = match field {
                    None => ids.iter().map(|id| self.term(f.rel, id)).collect(),
                    Some(fname) => role_aliases(fname)
                        .iter()
                        .flat_map(|fa| {
                            ids.iter()
                                .map(|id| self.term(f.relf, &scoped_term(fa, id)))
                                .collect::<Vec<_>>()
                        })
                        .collect(),
                };
                self.any_of(qs)
            }

            Node::Date { field, from, to } => {
                let name = match field {
                    DateField::Created => "created_at",
                    DateField::Updated => "updated_at",
                };
                let bound = |t: &Option<i64>| match t {
                    Some(t) => Bound::Included(DateTime::from_timestamp_secs(*t)),
                    None => Bound::Unbounded,
                };
                Box::new(RangeQuery::new_date_bounds(
                    name.to_string(),
                    bound(from),
                    bound(to),
                ))
            }

            Node::Enum { field, value } => {
                self.term(f.enumv, &scoped_term(field, value))
            }
            Node::State(s) => self.term(f.state, s),
            Node::Has(h) => self.term(f.has, h),
            Node::Id(id) => self.term(f.id, &id.to_lowercase()),
            Node::PublicId(p) => self.term(f.public_id, p),
        })
    }

    // ----------------------------------------------------------- execution

    pub fn search(&self, spec: &Spec, mut warnings: Vec<String>) -> Result<Results> {
        let t0 = std::time::Instant::now();
        let mut notes = Vec::new();
        let query = self.compile(&spec.root, &mut notes)?;
        let plan = format!("{query:?}");
        warnings.extend(notes);

        let searcher = self.reader.searcher();
        let f = &self.fields;

        // Tantivy's TopDocs panics on a zero limit, and `--limit 0` is a
        // legitimate way to ask "just count it".
        let limit = spec.limit.max(1);

        // Ranking. Relevance keeps Tantivy's BM25 but nudges by inbound
        // reference count, so a widely-cited note beats an incidental mention.
        let (top, total, by_db) = match spec.sort {
            Sort::Relevance => {
                let inbound_field = f.inbound;
                let collector = TopDocs::with_limit(limit).tweak_score(
                    move |seg: &SegmentReader| {
                        let col = seg.fast_fields().u64("inbound").ok();
                        move |doc: DocId, score: Score| {
                            let inb = col
                                .as_ref()
                                .and_then(|c| c.first(doc))
                                .unwrap_or(0);
                            let _ = inbound_field;
                            score * (1.0 + (inb as Score + 1.0).ln() * 0.15)
                        }
                    },
                );
                let (top, total, by_db) = searcher.search(
                    &query,
                    &(collector, Count, DbCount::new("db")),
                )?;
                let top: Vec<(Score, tantivy::DocAddress)> = top;
                (top, total, by_db)
            }
            Sort::Recent | Sort::Created | Sort::Oldest => {
                let (field_name, order) = match spec.sort {
                    Sort::Recent => ("updated_at", tantivy::Order::Desc),
                    Sort::Created => ("created_at", tantivy::Order::Desc),
                    _ => ("created_at", tantivy::Order::Asc),
                };
                let collector = TopDocs::with_limit(limit)
                    .order_by_fast_field::<DateTime>(field_name, order);
                let (top, total, by_db) = searcher.search(
                    &query,
                    &(collector, Count, DbCount::new("db")),
                )?;
                // Carry the sort key through as the displayed score slot.
                let top: Vec<(Score, tantivy::DocAddress)> = top
                    .into_iter()
                    .map(|(d, a): (DateTime, tantivy::DocAddress)| {
                        (d.into_timestamp_secs() as Score, a)
                    })
                    .collect();
                (top, total, by_db)
            }
        };

        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top.into_iter().take(spec.limit) {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let s = |field: tantivy::schema::Field| -> String {
                doc.get_first(field)
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            let d = |field: tantivy::schema::Field| -> i64 {
                doc.get_first(field)
                    .and_then(|v| v.as_datetime())
                    .map(|t| t.into_timestamp_secs())
                    .unwrap_or(0)
            };
            let state = s(f.state);
            hits.push(Hit {
                id: s(f.id),
                db: s(f.db),
                title: s(f.title),
                public_id: s(f.public_id),
                snippet: s(f.snippet),
                created: d(f.created_at),
                updated: d(f.updated_at),
                score,
                state: if state.is_empty() { None } else { Some(state) },
                inbound: doc
                    .get_first(f.inbound)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            });
        }

        let mut by_db: Vec<(String, u64)> = by_db.into_iter().collect();
        by_db.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        Ok(Results {
            hits,
            total,
            by_db: if spec.facets { by_db } else { Vec::new() },
            took_ms: t0.elapsed().as_secs_f64() * 1000.0,
            plan,
            warnings,
        })
    }

    /// Count matches only — used by the benchmark to isolate query cost from
    /// document retrieval cost.
    pub fn count(&self, spec: &Spec) -> Result<(usize, f64)> {
        let mut notes = Vec::new();
        let query = self.compile(&spec.root, &mut notes)?;
        let searcher = self.reader.searcher();
        let t0 = std::time::Instant::now();
        let n = searcher.search(&query, &Count)?;
        Ok((n, t0.elapsed().as_secs_f64() * 1000.0))
    }

    pub fn db_name(&self, idx: u32) -> &str {
        self.catalog
            .dbs
            .get(idx as usize)
            .map(|d| d.name.as_str())
            .unwrap_or("?")
    }

    pub fn field_name(&self, idx: u32) -> &str {
        self.meta
            .field_names
            .get(idx as usize)
            .map(|s| s.as_str())
            .unwrap_or("?")
    }

    pub fn entity(&self, id: u128) -> Result<Option<EntityRec>> {
        self.store.get_entity(id)
    }

    /// Inbound edges: everything whose `rel` field names `id`. Free, because
    /// relations were indexed as terms on both sides.
    pub fn backlinks(&self, id: u128, limit: usize) -> Result<Vec<(String, EntityRec)>> {
        let searcher = self.reader.searcher();
        let q = TermQuery::new(
            Term::from_field_text(self.fields.rel, &format_uuid(id)),
            IndexRecordOption::Basic,
        );
        let top = searcher.search(&q, &TopDocs::with_limit(limit))?;
        let mut out = Vec::new();
        for (_, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            let Some(sid) = doc.get_first(self.fields.id).and_then(|v| v.as_str()) else {
                continue;
            };
            if let Some(uid) = parse_uuid(sid) {
                if let Some(rec) = self.store.get_entity(uid)? {
                    out.push((sid.to_string(), rec));
                }
            }
        }
        Ok(out)
    }
}

/// Tolerate singular/plural and common aliases when someone types a role or
/// relation field name: `assignee:` should find the `assignees` field.
fn role_aliases(r: &str) -> Vec<String> {
    let r = r.to_lowercase();
    let mut out = vec![r.clone()];
    if let Some(stem) = r.strip_suffix('s') {
        out.push(stem.to_string());
    } else {
        out.push(format!("{r}s"));
    }
    match r.as_str() {
        "assignee" | "assignees" | "owner" => {
            out.push("assignees".into());
            out.push("assignee".into());
        }
        "created" | "creator" | "author" => out.push("created".into()),
        _ => {}
    }
    out.sort();
    out.dedup();
    out
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

// ------------------------------------------------------- facet collector

/// Counts matching documents per database by reading the `db` string fast
/// field. Runs alongside the top-docs collector in a single index pass, so the
/// "which databases could I narrow to?" summary is effectively free.
pub struct DbCount {
    field: String,
}

impl DbCount {
    pub fn new(field: &str) -> Self {
        DbCount {
            field: field.to_string(),
        }
    }
}

impl Collector for DbCount {
    type Fruit = FxHashMap<String, u64>;
    type Child = DbCountSegment;

    fn for_segment(
        &self,
        _seg_ord: SegmentOrdinal,
        reader: &SegmentReader,
    ) -> tantivy::Result<Self::Child> {
        let col = reader.fast_fields().str(&self.field)?;
        let n = col.as_ref().map(|c| c.num_terms()).unwrap_or(0);
        Ok(DbCountSegment {
            col,
            counts: vec![0u64; n],
        })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, children: Vec<Self::Fruit>) -> tantivy::Result<Self::Fruit> {
        let mut out: FxHashMap<String, u64> = FxHashMap::default();
        for c in children {
            for (k, v) in c {
                *out.entry(k).or_insert(0) += v;
            }
        }
        Ok(out)
    }
}

pub struct DbCountSegment {
    col: Option<StrColumn>,
    /// Counts indexed by term ordinal; resolved to strings once, at harvest.
    counts: Vec<u64>,
}

impl SegmentCollector for DbCountSegment {
    type Fruit = FxHashMap<String, u64>;

    fn collect(&mut self, doc: DocId, _score: Score) {
        if let Some(col) = &self.col {
            for ord in col.term_ords(doc) {
                if let Some(slot) = self.counts.get_mut(ord as usize) {
                    *slot += 1;
                }
            }
        }
    }

    fn harvest(self) -> Self::Fruit {
        let mut out: FxHashMap<String, u64> = FxHashMap::default();
        if let Some(col) = &self.col {
            let mut buf = Vec::new();
            for (ord, &n) in self.counts.iter().enumerate() {
                if n == 0 {
                    continue;
                }
                buf.clear();
                if col.ord_to_bytes(ord as u64, &mut buf).is_ok() {
                    if let Ok(s) = std::str::from_utf8(&buf) {
                        *out.entry(s.to_string()).or_insert(0) += n;
                    }
                }
            }
        }
        out
    }
}

/// Score how well a typed `@needle` identifies a user. 0 means no match.
///
/// Ordering matters more than the absolute numbers: an exact *name word*
/// (someone's first or last name) must beat an exact email local part, or
/// service accounts named after a common first name swallow the query.
fn score_user(raw: &str, norm: &str, name: &str, email: &str) -> u32 {
    let name_l = name.to_lowercase();
    let email_l = email.to_lowercase();
    let local = email_l.split('@').next().unwrap_or("");

    if name_l == raw || name_l == norm {
        return 100;
    }
    let words: Vec<&str> = name_l.split_whitespace().collect();
    if words.iter().any(|w| *w == raw || *w == norm) {
        return 90;
    }
    if local == raw || local.replace('.', " ") == norm {
        return 80;
    }
    if words.iter().any(|w| w.starts_with(raw)) {
        return 60;
    }
    if local.starts_with(raw) {
        return 50;
    }
    if name_l.contains(raw) {
        return 30;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::score_user;

    /// The bug this scoring model exists to fix.
    #[test]
    fn a_name_word_outranks_an_email_local_part() {
        let human = score_user("michael", "michael", "Michael Tokar", "michael.tokar@neara.com");
        let service = score_user("michael", "michael", "michael@fibery.io", "michael@fibery.io");
        assert!(
            human > service,
            "humans ({human}) must outrank the service account ({service})"
        );
    }

    #[test]
    fn exact_full_name_wins_outright() {
        assert_eq!(
            score_user("sami kyostila", "sami kyostila", "Sami Kyostila", "sami@neara.com"),
            100
        );
    }

    #[test]
    fn dotted_and_hyphenated_input_reads_as_a_full_name() {
        // `@michael.tokar` -> normalised to "michael tokar" -> exact full name.
        assert_eq!(
            score_user("michael.tokar", "michael tokar", "Michael Tokar", "michael.tokar@neara.com"),
            100
        );
    }

    #[test]
    fn surname_alone_resolves() {
        assert_eq!(score_user("tokar", "tokar", "Michael Tokar", "michael.tokar@neara.com"), 90);
    }

    #[test]
    fn prefixes_match_but_rank_lower_than_exact() {
        let prefix = score_user("mich", "mich", "Michael Tokar", "michael.tokar@neara.com");
        let exact = score_user("michael", "michael", "Michael Tokar", "michael.tokar@neara.com");
        assert!(prefix > 0 && prefix < exact);
    }

    #[test]
    fn unrelated_names_do_not_match() {
        assert_eq!(score_user("zzzz", "zzzz", "Michael Tokar", "michael.tokar@neara.com"), 0);
    }
}
