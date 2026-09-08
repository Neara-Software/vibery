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
    AllQuery, BooleanQuery, BoostQuery, EmptyQuery, FuzzyTermQuery, Occur, PhraseQuery, Query,
    RangeQuery, RegexQuery, TermQuery,
};
use tantivy::tokenizer::TextAnalyzer;
use tantivy::schema::{IndexRecordOption, Value as _};
use tantivy::{DateTime, DocId, Score, SegmentOrdinal, SegmentReader, TantivyDocument, Term};

/// Boosts: a title hit is worth far more than a body hit, and a hit buried in
/// a comment thread is worth less than either.
const BOOST_TITLE: Score = 6.0;
const BOOST_BODY: Score = 1.0;
const BOOST_COMMENTS: Score = 0.4;

/// A fuzzy variant is real but weaker evidence than an exact stem match, so it
/// scores lower rather than competing head-on.
const BOOST_FUZZY: Score = 0.35;
/// Likewise for a prefix expansion.
const BOOST_PREFIX: Score = 0.6;

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
    /// The same analyzer the index was built with. Cloned per query because
    /// `token_stream` needs `&mut`.
    analyzer: TextAnalyzer,
    /// Curated term -> entity pins, Glean style. Empty unless `pins.json` exists.
    pub pins: Vec<Pin>,
}

/// A curated pin: "when someone searches these terms, this is the canonical
/// answer, whatever BM-25 thinks".
///
/// Asked for directly in the note: *"I have no guarantee that a doc I write
/// will be visible in 6 months, once it's no longer the most recently updated
/// page for a given term."*
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Pin {
    /// Query strings this pin fires on, compared case-insensitively against
    /// the whole free-text part of the query.
    pub terms: Vec<String>,
    /// Entity UUID to pin.
    pub id: String,
    #[serde(default)]
    pub why: String,
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
    /// True when this row was promoted by a curated pin rather than by score.
    pub pinned: bool,
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
        // Query-time analysis must use the same pipeline as index-time.
        index
            .tokenizers()
            .register(crate::text::TOKENIZER, crate::text::analyzer());
        let pins = load_pins(&dir.join("pins.json"));
        Ok(Engine {
            index,
            reader,
            fields,
            store,
            catalog,
            meta,
            users,
            analyzer: crate::text::analyzer(),
            pins,
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

    /// Fields a text clause searches, with their boosts.
    fn scope_fields(&self, scope: TextScope) -> Vec<(tantivy::schema::Field, Score)> {
        let f = &self.fields;
        match scope {
            TextScope::Any => vec![
                (f.title, BOOST_TITLE),
                (f.body, BOOST_BODY),
                (f.comments, BOOST_COMMENTS),
            ],
            TextScope::Title => vec![(f.title, BOOST_TITLE)],
            TextScope::Body => vec![(f.body, BOOST_BODY)],
            TextScope::Comments => vec![(f.comments, BOOST_COMMENTS)],
        }
    }

    /// Analyse raw user text with the index's own analyzer.
    fn analyze(&self, raw: &str) -> Vec<String> {
        crate::text::analyze(&mut self.analyzer.clone(), raw)
    }

    /// Compile one typed word.
    ///
    /// Three tiers, OR-ed together and weighted, so better evidence wins:
    /// exact stem, then prefix (if `*`), then a distance-1 fuzzy match. The
    /// fuzzy tier is what lets `subdivide` find `subdivision` — stemming gets
    /// them to `subdivid` / `subdivis`, one edit apart.
    ///
    /// If the word analyses into several tokens (`DataView` -> `data`, `view`)
    /// it becomes a phrase, which is how it matches a title written
    /// `Data Views`.
    fn word_query(
        &self,
        raw: &str,
        scope: TextScope,
        prefix: bool,
        fuzzy: Option<u8>,
        auto_fuzzy: bool,
    ) -> Box<dyn Query> {
        let toks = self.analyze(raw);
        match toks.len() {
            0 => return Box::new(EmptyQuery),
            1 => {}
            _ => return self.phrase_of(&toks, scope),
        }
        let tok = &toks[0];
        let dist = fuzzy.or_else(|| {
            if auto_fuzzy && !prefix {
                crate::text::auto_fuzz(tok)
            } else {
                None
            }
        });

        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (field, boost) in self.scope_fields(scope) {
            let term = Term::from_field_text(field, tok);
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(term.clone(), IndexRecordOption::WithFreqs)),
                    boost,
                )),
            ));
            if prefix {
                // Prefix expansion over the term dictionary FST: this is what
                // makes "it updates the filtering as I type" work.
                if let Ok(q) = RegexQuery::from_pattern(&format!("{}.*", regex_escape(tok)), field)
                {
                    clauses.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(Box::new(q), boost * BOOST_PREFIX)),
                    ));
                }
            }
            if let Some(d) = dist {
                clauses.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(FuzzyTermQuery::new(term, d, true)),
                        boost * BOOST_FUZZY,
                    )),
                ));
            }
        }
        Box::new(BooleanQuery::new(clauses))
    }

    /// A phrase over already-analysed tokens.
    fn phrase_of(&self, toks: &[String], scope: TextScope) -> Box<dyn Query> {
        if toks.len() < 2 {
            return match toks.first() {
                Some(t) => {
                    let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                    for (field, boost) in self.scope_fields(scope) {
                        clauses.push((
                            Occur::Should,
                            Box::new(BoostQuery::new(
                                Box::new(TermQuery::new(
                                    Term::from_field_text(field, t),
                                    IndexRecordOption::WithFreqs,
                                )),
                                boost,
                            )),
                        ));
                    }
                    Box::new(BooleanQuery::new(clauses))
                }
                None => Box::new(EmptyQuery),
            };
        }
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for (field, boost) in self.scope_fields(scope) {
            let terms: Vec<Term> = toks
                .iter()
                .map(|w| Term::from_field_text(field, w))
                .collect();
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(Box::new(PhraseQuery::new(terms)), boost)),
            ));
        }
        Box::new(BooleanQuery::new(clauses))
    }

    fn phrase_query(&self, raw: &str, scope: TextScope) -> Box<dyn Query> {
        let toks = self.analyze(raw);
        self.phrase_of(&toks, scope)
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
    fn compile(
        &self,
        node: &Node,
        notes: &mut Vec<String>,
        fz: bool,
    ) -> Result<Box<dyn Query>> {
        let f = &self.fields;
        Ok(match node {
            Node::All => Box::new(AllQuery),

            Node::And(parts) => {
                let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
                for p in parts {
                    // `-x` inside a conjunction becomes MustNot rather than a
                    // nested boolean, which keeps the query fast.
                    if let Node::Not(inner) = p {
                        clauses.push((Occur::MustNot, self.compile(inner, notes, fz)?));
                    } else {
                        clauses.push((Occur::Must, self.compile(p, notes, fz)?));
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
                    qs.push(self.compile(p, notes, fz)?);
                }
                self.any_of(qs)
            }

            Node::Not(inner) => Box::new(BooleanQuery::new(vec![
                (Occur::Must, Box::new(AllQuery) as Box<dyn Query>),
                (Occur::MustNot, self.compile(inner, notes, fz)?),
            ])),

            Node::Word {
                raw,
                scope,
                prefix,
                fuzzy,
            } => self.word_query(raw, *scope, *prefix, *fuzzy, fz),

            Node::Phrase { raw, scope } => self.phrase_query(raw, *scope),

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
                let mut inner = self.compile(sub, notes, fz)?;
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
        let query = self.compile(&spec.root, &mut notes, spec.fuzzy)?;
        let plan = format!("{query:?}");

        let searcher = self.reader.searcher();
        let f = &self.fields;

        // Tantivy's TopDocs panics on a zero limit, and `--limit 0` is a
        // legitimate way to ask "just count it".
        let limit = spec.limit.max(1);

        // Ranking. Relevance keeps Tantivy's BM25 but nudges by inbound
        // reference count, so a widely-cited note beats an incidental mention.
        let (top, total, by_db) = match spec.sort {
            Sort::Relevance => {
                // The note's complaint: "search does not prioritise useful
                // information… does not deprioritise guides marked 'not
                // started'", alongside the opposite worry that a good doc
                // disappears once it stops being the most recently updated.
                //
                // So: reward being cited and being substantial, apply only a
                // gentle bounded recency nudge (so an old authoritative page
                // still wins), and actively demote not-started stubs.
                let now = crate::util::now_secs() as Score;
                let collector = TopDocs::with_limit(limit).tweak_score(
                    move |seg: &SegmentReader| {
                        let inbound = seg.fast_fields().u64("inbound").ok();
                        let text_len = seg.fast_fields().u64("text_len").ok();
                        let rflags = seg.fast_fields().u64("rank_flags").ok();
                        let updated = seg.fast_fields().date("updated_at").ok();
                        move |doc: DocId, score: Score| {
                            let g = |c: &Option<tantivy::columnar::Column<u64>>| {
                                c.as_ref().and_then(|c| c.first(doc)).unwrap_or(0)
                            };
                            let inb = g(&inbound) as Score;
                            let len = g(&text_len) as Score;
                            let flags = g(&rflags);

                            // Cited by other entities: strongest authority signal.
                            let authority = 1.0 + (inb + 1.0).ln() * 0.15;
                            // Substance: a 5k-word guide beats a one-line stub.
                            let substance = 1.0 + (1.0 + len / 500.0).ln() * 0.08;
                            // Recency: bounded at +25%, decaying over a year.
                            let recency = match updated
                                .as_ref()
                                .and_then(|c| c.first(doc))
                            {
                                Some(t) => {
                                    let age_days =
                                        ((now - t.into_timestamp_secs() as Score) / 86_400.0)
                                            .max(0.0);
                                    1.0 + 0.25 * (-age_days / 365.0).exp()
                                }
                                None => 1.0,
                            };
                            // Not-started work is a stub by definition.
                            let lifecycle =
                                if flags & crate::index::flags::NOT_STARTED != 0 {
                                    0.5
                                } else {
                                    1.0
                                };
                            score * authority * substance * recency * lifecycle
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
                pinned: false,
                inbound: doc
                    .get_first(f.inbound)
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            });
        }

        // Curated pins are promoted to the top, ahead of anything BM-25 chose.
        // They only fire when the query's free text matches a pin's terms.
        if !self.pins.is_empty() {
            if let Some(text) = free_text(&spec.root) {
                let key = text.trim().to_lowercase();
                if !key.is_empty() {
                    for pin in &self.pins {
                        if !pin.terms.iter().any(|t| t.to_lowercase() == key) {
                            continue;
                        }
                        let Some(pid) = parse_uuid(&pin.id) else { continue };
                        // Don't double-list it if the ranker already found it.
                        hits.retain(|h| parse_uuid(&h.id) != Some(pid));
                        if let Some(mut h) = self.hit_by_id(pid)? {
                            h.pinned = true;
                            notes.push(format!(
                                "pinned `{}`{}",
                                h.title,
                                if pin.why.is_empty() {
                                    String::new()
                                } else {
                                    format!(" — {}", pin.why)
                                }
                            ));
                            hits.insert(0, h);
                        }
                    }
                    hits.truncate(spec.limit.max(1));
                }
            }
        }
        warnings.extend(notes.split_off(0));

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

    /// Fetch a single hit by entity id, for pin promotion.
    fn hit_by_id(&self, id: u128) -> Result<Option<Hit>> {
        let (spec, _) = crate::query::Parser::parse(&format!("id:{}", format_uuid(id)));
        let res = self.search_plain(&crate::query::Spec { limit: 1, ..spec })?;
        Ok(res.hits.into_iter().next())
    }

    /// `search` without pin promotion, to avoid recursing through it.
    fn search_plain(&self, spec: &crate::query::Spec) -> Result<Results> {
        let mut notes = Vec::new();
        let query = self.compile(&spec.root, &mut notes, false)?;
        let searcher = self.reader.searcher();

        let top = searcher.search(&query, &TopDocs::with_limit(spec.limit.max(1)))?;
        let mut hits = Vec::with_capacity(top.len());
        for (score, addr) in top {
            let doc: TantivyDocument = searcher.doc(addr)?;
            hits.push(self.hit_of(&doc, score)?);
        }
        Ok(Results {
            hits,
            total: 0,
            by_db: Vec::new(),
            took_ms: 0.0,
            plan: String::new(),
            warnings: Vec::new(),
        })
    }

    /// Decode a stored Tantivy document into a `Hit`.
    fn hit_of(&self, doc: &TantivyDocument, score: Score) -> Result<Hit> {
        let f = &self.fields;
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
        Ok(Hit {
            id: s(f.id),
            db: s(f.db),
            title: s(f.title),
            public_id: s(f.public_id),
            snippet: s(f.snippet),
            created: d(f.created_at),
            updated: d(f.updated_at),
            score,
            state: if state.is_empty() { None } else { Some(state) },
            inbound: doc.get_first(f.inbound).and_then(|v| v.as_u64()).unwrap_or(0),
            pinned: false,
        })
    }

    /// Count matches only — used by the benchmark to isolate query cost from
    /// document retrieval cost.
    pub fn count(&self, spec: &Spec) -> Result<(usize, f64)> {
        let mut notes = Vec::new();
        let query = self.compile(&spec.root, &mut notes, spec.fuzzy)?;
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

/// Collect the free-text part of a query, for pin matching. Structural filters
/// are ignored: a pin fires on what the user typed as words.
fn free_text(node: &Node) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    fn walk(n: &Node, out: &mut Vec<String>) {
        match n {
            Node::Word { raw, .. } => out.push(raw.clone()),
            Node::Phrase { raw, .. } => out.push(raw.clone()),
            Node::And(ps) | Node::Or(ps) => ps.iter().for_each(|p| walk(p, out)),
            _ => {}
        }
    }
    walk(node, &mut out);
    (!out.is_empty()).then(|| out.join(" "))
}

/// Read `pins.json` if present. A malformed or absent file simply means no
/// pins — this is an optional curation layer, never a hard dependency.
fn load_pins(path: &std::path::Path) -> Vec<Pin> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Vec<Pin>>(&bytes) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("warning: ignoring {}: {e}", path.display());
            Vec::new()
        }
    }
}
