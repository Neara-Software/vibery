//! The Tantivy index schema.
//!
//! Two decisions here do most of the work of fixing the reported search
//! problems:
//!
//! 1. **Everything is a filterable term, including structure.** `db`, `actor`,
//!    `rel`, `enumv` are indexed STRING fields. That means a query with *no*
//!    free text at all is still a normal, fast index lookup — which is the
//!    direct fix for "`Created by: Sami Kyostila` with an empty text string
//!    returns 2 results".
//!
//! 2. **Text is denormalised onto the entity.** A Fibery entity's prose lives
//!    in separate `Document` rows and its discussion lives in `comment` rows.
//!    We fold both into `body`/`comments` on the entity itself, so one term
//!    lookup finds the thing you were actually looking for.

use tantivy::schema::{
    DateOptions, Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing,
    TextOptions, FAST, INDEXED, STORED, STRING, TEXT,
};

/// Named handles onto the Tantivy schema, resolved once.
#[derive(Clone, Copy, Debug)]
pub struct Fields {
    pub id: Field,
    pub public_id: Field,
    pub db: Field,
    pub space: Field,
    pub title: Field,
    pub body: Field,
    pub comments: Field,
    /// Multi-valued: `created:<uuid>`, `assignee:<uuid>`, `commented:<uuid>`,
    /// and a role-agnostic `touched:<uuid>` for every one of them.
    pub actor: Field,
    /// Multi-valued: every related entity id, one hop out, any relation.
    pub rel: Field,
    /// Multi-valued: `<field-short>:<uuid>` for field-scoped relation queries.
    pub relf: Field,
    /// Multi-valued: `<field-short>:<label>` for enum/select filters.
    pub enumv: Field,
    /// Multi-valued: `description`, `comments`, `assignee`, ...
    pub has: Field,
    pub created_at: Field,
    pub updated_at: Field,
    /// Stored-only display payload (snippet, state label, url).
    pub snippet: Field,
    pub state: Field,
    /// Number of inbound references — a cheap authority signal for ranking.
    pub inbound: Field,
}

pub fn build_schema() -> (Schema, Fields) {
    let mut sb = Schema::builder();

    // Untokenised, indexed + stored: exact-match identity.
    let id = sb.add_text_field("id", STRING | STORED);
    let public_id = sb.add_text_field("public_id", STRING | STORED);
    let db = sb.add_text_field("db", STRING | STORED | FAST);
    let space = sb.add_text_field("space", STRING | STORED | FAST);

    // Tokenised prose. `title` is stored so results can be rendered without a
    // second lookup; `body`/`comments` are index-only (we store a short
    // snippet separately rather than the whole document).
    let title_opts: TextOptions = TextOptions::default()
        .set_stored()
        .set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer("default")
                .set_index_option(IndexRecordOption::WithFreqsAndPositions),
        );
    let title = sb.add_text_field("title", title_opts);
    let body = sb.add_text_field("body", TEXT);
    let comments = sb.add_text_field("comments", TEXT);

    // Structural facets. Multi-valued simply by adding the field twice.
    let actor = sb.add_text_field("actor", STRING);
    let rel = sb.add_text_field("rel", STRING);
    let relf = sb.add_text_field("relf", STRING);
    let enumv = sb.add_text_field("enumv", STRING);
    let has = sb.add_text_field("has", STRING);

    // FAST so we can sort and range-filter without touching stored docs.
    let date_opts: DateOptions = DateOptions::from(INDEXED)
        .set_stored()
        .set_fast()
        .set_precision(tantivy::schema::DateTimePrecision::Seconds);
    let created_at = sb.add_date_field("created_at", date_opts.clone());
    let updated_at = sb.add_date_field("updated_at", date_opts);

    let snippet = sb.add_text_field("snippet", STORED);
    let state = sb.add_text_field("state", STRING | STORED);
    let inbound = sb.add_u64_field(
        "inbound",
        NumericOptions::default().set_stored().set_fast(),
    );

    let schema = sb.build();
    let fields = Fields {
        id,
        public_id,
        db,
        space,
        title,
        body,
        comments,
        actor,
        rel,
        relf,
        enumv,
        has,
        created_at,
        updated_at,
        snippet,
        state,
        inbound,
    };
    (schema, fields)
}

impl Fields {
    /// Re-resolve handles from an already-opened index.
    pub fn from_schema(schema: &Schema) -> anyhow::Result<Self> {
        let g = |n: &str| -> anyhow::Result<Field> {
            schema
                .get_field(n)
                .map_err(|_| anyhow::anyhow!("index schema missing field `{n}`; re-run `vibery ingest`"))
        };
        Ok(Fields {
            id: g("id")?,
            public_id: g("public_id")?,
            db: g("db")?,
            space: g("space")?,
            title: g("title")?,
            body: g("body")?,
            comments: g("comments")?,
            actor: g("actor")?,
            rel: g("rel")?,
            relf: g("relf")?,
            enumv: g("enumv")?,
            has: g("has")?,
            created_at: g("created_at")?,
            updated_at: g("updated_at")?,
            snippet: g("snippet")?,
            state: g("state")?,
            inbound: g("inbound")?,
        })
    }
}

/// Role prefixes for the `actor` field.
pub mod role {
    pub const CREATED: &str = "created";
    pub const COMMENTED: &str = "commented";
    /// Role-agnostic: set for every actor edge, whatever the field was.
    pub const TOUCHED: &str = "touched";
}

/// `actor` term encoding: `role:uuid`.
pub fn actor_term(role: &str, user_id: &str) -> String {
    let mut s = String::with_capacity(role.len() + 1 + user_id.len());
    s.push_str(role);
    s.push(':');
    s.push_str(user_id);
    s
}

/// `relf`/`enumv` term encoding: `field:value`, field lowercased.
pub fn scoped_term(field_short: &str, value: &str) -> String {
    let mut s = String::with_capacity(field_short.len() + 1 + value.len());
    s.push_str(field_short);
    s.push(':');
    s.push_str(value);
    s
}
