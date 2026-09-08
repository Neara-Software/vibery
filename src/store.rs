//! The redb side of the world: a durable id -> record map.
//!
//! Tantivy answers "which entities match?"; redb answers "tell me everything
//! about *this* entity, by id, right now" without a term lookup. That's what
//! `locate` walks and what result rendering uses to name related entities.

use anyhow::{Context, Result};
use redb::{Database, ReadableTable, ReadableTableMetadata, TableDefinition};
use serde::{Deserialize, Serialize};
use std::path::Path;

const ENTITIES: TableDefinition<u128, &[u8]> = TableDefinition::new("entities");
const USERS: TableDefinition<u128, &[u8]> = TableDefinition::new("users");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");
/// Full folded prose per entity, for in-entity search (`vibery find`).
/// Kept out of `entities` so ordinary result rendering never pays to decode it.
const TEXTS: TableDefinition<u128, &[u8]> = TableDefinition::new("texts");

/// Everything needed to render an entity, or to walk out of it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EntityRec {
    /// Index into `Catalog::dbs`.
    pub db: u32,
    pub title: String,
    pub public_id: String,
    pub created: i64,
    pub updated: i64,
    pub created_by: Option<u128>,
    /// Outbound relation edges as `(field-name index, target id)`.
    pub rels: Vec<(u32, u128)>,
    /// First ~200 chars of prose, for result listings.
    pub snippet: String,
    pub state: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserRec {
    pub name: String,
    pub email: String,
    pub active: bool,
}

/// Ingest-time counters plus the field-name table, persisted alongside the data.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Meta {
    pub db_names: Vec<String>,
    pub field_names: Vec<String>,
    pub entities_indexed: u64,
    pub entities_scanned: u64,
    pub documents_loaded: u64,
    pub comments_loaded: u64,
    /// Of `comments_loaded`, how many had retrievable prose. Zero means the
    /// dump carries comment authorship but not comment text.
    pub comments_with_text: u64,
    pub text_bytes: u64,
    pub ingest_secs: f64,
    pub built_at: i64,
}

pub struct Store {
    db: Database,
}

impl Store {
    pub fn create(path: &Path) -> Result<Self> {
        let db = Database::create(path).context("create redb database")?;
        // Materialise tables so read transactions never hit a missing table.
        let w = db.begin_write()?;
        {
            w.open_table(ENTITIES)?;
            w.open_table(USERS)?;
            w.open_table(META)?;
            w.open_table(TEXTS)?;
        }
        w.commit()?;
        Ok(Store { db })
    }

    pub fn open(path: &Path) -> Result<Self> {
        let db = Database::open(path)
            .with_context(|| format!("open {} (run `vibery ingest` first)", path.display()))?;
        Ok(Store { db })
    }

    /// Bulk-load entity records. Called once per database file during ingest.
    pub fn put_entities(&self, batch: &[(u128, EntityRec)]) -> Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut t = w.open_table(ENTITIES)?;
            let mut buf = Vec::with_capacity(512);
            for (id, rec) in batch {
                buf.clear();
                bincode::serialize_into(&mut buf, rec)?;
                t.insert(*id, buf.as_slice())?;
            }
        }
        w.commit()?;
        Ok(())
    }

    pub fn put_users(&self, batch: &[(u128, UserRec)]) -> Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut t = w.open_table(USERS)?;
            let mut buf = Vec::with_capacity(128);
            for (id, rec) in batch {
                buf.clear();
                bincode::serialize_into(&mut buf, rec)?;
                t.insert(*id, buf.as_slice())?;
            }
        }
        w.commit()?;
        Ok(())
    }

    /// Store folded prose and comment text for one entity.
    pub fn put_texts(&self, batch: &[(u128, String, String)]) -> Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut t = w.open_table(TEXTS)?;
            let mut buf = Vec::with_capacity(4096);
            for (id, body, comments) in batch {
                buf.clear();
                bincode::serialize_into(&mut buf, &(body, comments))?;
                t.insert(*id, buf.as_slice())?;
            }
        }
        w.commit()?;
        Ok(())
    }

    /// `(body, comments)` for one entity, if any was indexed.
    pub fn get_texts(&self, id: u128) -> Result<Option<(String, String)>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(TEXTS)?;
        Ok(match t.get(id)? {
            Some(v) => Some(bincode::deserialize(v.value())?),
            None => None,
        })
    }

    pub fn get_entity(&self, id: u128) -> Result<Option<EntityRec>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(ENTITIES)?;
        Ok(match t.get(id)? {
            Some(v) => Some(bincode::deserialize(v.value())?),
            None => None,
        })
    }

    /// Batch variant that reuses one read transaction — matters when rendering
    /// a page of results that each name several related entities.
    pub fn get_entities(&self, ids: &[u128]) -> Result<Vec<Option<EntityRec>>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(ENTITIES)?;
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            out.push(match t.get(id)? {
                Some(v) => Some(bincode::deserialize(v.value())?),
                None => None,
            });
        }
        Ok(out)
    }

    pub fn get_user(&self, id: u128) -> Result<Option<UserRec>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(USERS)?;
        Ok(match t.get(id)? {
            Some(v) => Some(bincode::deserialize(v.value())?),
            None => None,
        })
    }

    pub fn all_users(&self) -> Result<Vec<(u128, UserRec)>> {
        let r = self.db.begin_read()?;
        let t = r.open_table(USERS)?;
        let mut out = Vec::new();
        for row in t.iter()? {
            let (k, v) = row?;
            out.push((k.value(), bincode::deserialize(v.value())?));
        }
        Ok(out)
    }

    pub fn entity_count(&self) -> Result<u64> {
        let r = self.db.begin_read()?;
        Ok(r.open_table(ENTITIES)?.len()?)
    }

    pub fn put_meta(&self, meta: &Meta) -> Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut t = w.open_table(META)?;
            let bytes = bincode::serialize(meta)?;
            t.insert("meta", bytes.as_slice())?;
        }
        w.commit()?;
        Ok(())
    }

    pub fn get_meta(&self) -> Result<Meta> {
        let r = self.db.begin_read()?;
        let t = r.open_table(META)?;
        Ok(match t.get("meta")? {
            Some(v) => bincode::deserialize(v.value())?,
            None => Meta::default(),
        })
    }

    /// Persist the catalog next to the data so queries don't need the original
    /// `schema.json` to resolve `#database` names.
    pub fn put_catalog(&self, cat: &crate::catalog::Catalog) -> Result<()> {
        let w = self.db.begin_write()?;
        {
            let mut t = w.open_table(META)?;
            let bytes = bincode::serialize(cat)?;
            t.insert("catalog", bytes.as_slice())?;
        }
        w.commit()?;
        Ok(())
    }

    pub fn get_catalog(&self) -> Result<crate::catalog::Catalog> {
        let r = self.db.begin_read()?;
        let t = r.open_table(META)?;
        let v = t
            .get("catalog")?
            .context("no catalog in store; run `vibery ingest`")?;
        Ok(bincode::deserialize(v.value())?)
    }
}
