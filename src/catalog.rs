//! Parses the raw Fibery `schema.json` + `manifest.json` into a compact catalog
//! that tells the ingester what each field *means*.
//!
//! Fibery's schema is deeply generic: a field's semantics live in its target
//! type name and a `fibery/meta` map. We resolve that once, up front, so the
//! hot ingest loop is a series of cheap lookups.

use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// How the ingester should treat one field of one database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FieldKind {
    /// The entity's human-readable title (`ui/title? = true`).
    Title,
    /// Free text worth putting in the full-text index.
    Text,
    /// Rich-text handle: `{fibery/id, Collaboration~Documents/secret}`.
    RichText,
    /// Points at `fibery/user` — a participation edge.
    User,
    /// Points at an enum table — resolve to a label.
    Enum,
    /// Points at another domain database — a relation edge.
    Relation,
    /// The `comments/comments` collection.
    Comments,
    /// A timestamp we surface and sort on.
    DateTime,
    Date,
    Int,
    Decimal,
    Bool,
    /// Present in the data but not useful for search (rank, color, uuid...).
    Ignore,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldDef {
    pub name: String,
    /// Last path segment, lowercased — what users actually type.
    pub short: String,
    pub kind: FieldKind,
    pub target: String,
    pub is_collection: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DbDef {
    /// Fully-qualified Fibery name, e.g. `Engineering/Issue`.
    pub name: String,
    /// Space part, e.g. `Engineering`.
    pub space: String,
    /// Type part, e.g. `Issue`.
    pub short: String,
    pub file: Option<PathBuf>,
    pub entity_count: u64,
    pub fields: Vec<FieldDef>,
    pub title_field: Option<String>,
    /// True for the 370 enum/label tables; we load their names but don't index
    /// them as search targets.
    pub is_enum: bool,
    /// True for domain databases users actually search.
    pub is_domain: bool,
    /// Internal plumbing (documents, references, recent-items) — excluded from
    /// search results but still mined for text and edges.
    pub is_system: bool,
}

impl DbDef {
    pub fn field(&self, name: &str) -> Option<&FieldDef> {
        self.fields.iter().find(|f| f.name == name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Catalog {
    pub dbs: Vec<DbDef>,
    pub root: PathBuf,
}

/// Types whose rows exist only to support other rows. We still read them (for
/// text, labels and edges) but they never appear as a search hit.
const SYSTEM_TYPES: &[&str] = &[
    "Collaboration~Documents/Document",
    "Collaboration~Documents/Reference",
    "comments/comment",
    "comments/user-settings",
    "fibery/Recent~Item",
    "fibery/favorite-page",
    "fibery/user-group",
    "fibery/email-domain-user-group",
    "fibery/view",
    "fibery/subscription",
];

fn is_primitive_type(t: &str) -> bool {
    matches!(
        t,
        "fibery/text"
            | "fibery/int"
            | "fibery/decimal"
            | "fibery/bool"
            | "fibery/date"
            | "fibery/date-time"
            | "fibery/date-range"
            | "fibery/uuid"
            | "fibery/rank"
            | "fibery/emoji"
            | "fibery/color"
            | "fibery/url"
            | "fibery/email"
            | "fibery/json-value"
            | "fibery/id"
    )
}

/// Fields that are structurally present on everything and carry no search value.
fn is_noise_field(name: &str) -> bool {
    matches!(
        name,
        "fibery/rank" | "fibery/id" | "icon/icon" | "enum/color" | "enum/icon"
    ) || name.ends_with("/rank")
}

pub fn short_name(qualified: &str) -> &str {
    qualified.rsplit('/').next().unwrap_or(qualified)
}

pub fn space_name(qualified: &str) -> &str {
    qualified.split('/').next().unwrap_or(qualified)
}

impl Catalog {
    /// Build the catalog by joining `schema.json` (semantics) against
    /// `manifest.json` (which file holds which rows).
    pub fn load(root: &Path) -> Result<Self> {
        let schema: serde_json::Value =
            serde_json::from_reader(std::io::BufReader::new(
                std::fs::File::open(root.join("schema.json"))
                    .context("open schema.json")?,
            ))
            .context("parse schema.json")?;
        let manifest: serde_json::Value =
            serde_json::from_reader(std::io::BufReader::new(
                std::fs::File::open(root.join("manifest.json"))
                    .context("open manifest.json")?,
            ))
            .context("parse manifest.json")?;

        let types = schema["fibery/types"]
            .as_array()
            .context("schema.json: fibery/types missing")?;

        // First pass: classify every type so field targets can be resolved.
        let mut enum_types: FxHashSet<&str> = FxHashSet::default();
        let mut domain_types: FxHashSet<&str> = FxHashSet::default();
        for t in types {
            let name = t["fibery/name"].as_str().unwrap_or_default();
            let meta = &t["fibery/meta"];
            if meta["fibery/enum?"].as_bool().unwrap_or(false) {
                enum_types.insert(name);
            } else if meta["fibery/domain?"].as_bool().unwrap_or(false) {
                domain_types.insert(name);
            }
        }

        let manifest_dbs = manifest["databases"].as_object();
        let system: FxHashSet<&str> = SYSTEM_TYPES.iter().copied().collect();
        let mut dbs = Vec::with_capacity(types.len());

        for t in types {
            let name = t["fibery/name"].as_str().unwrap_or_default().to_string();
            if name.is_empty() {
                continue;
            }
            let meta = &t["fibery/meta"];
            if meta["fibery/primitive?"].as_bool().unwrap_or(false) {
                continue;
            }
            let is_enum = enum_types.contains(name.as_str());
            let is_domain = domain_types.contains(name.as_str());
            // Mixins and schema-only types have no rows of their own.
            if !is_enum && !is_domain {
                continue;
            }
            let is_system = system.contains(name.as_str());

            let mut title_field = None;
            let mut fields = Vec::new();
            for f in t["fibery/fields"].as_array().into_iter().flatten() {
                let fname = f["fibery/name"].as_str().unwrap_or_default();
                if fname.is_empty() || is_noise_field(fname) {
                    continue;
                }
                let target = f["fibery/type"].as_str().unwrap_or_default();
                let fmeta = &f["fibery/meta"];
                let is_collection =
                    fmeta["fibery/collection?"].as_bool().unwrap_or(false);
                let is_title = fmeta["ui/title?"].as_bool().unwrap_or(false);

                let kind = if is_title {
                    title_field = Some(fname.to_string());
                    FieldKind::Title
                } else if target == "Collaboration~Documents/Document" {
                    FieldKind::RichText
                } else if target == "comments/comment" {
                    FieldKind::Comments
                } else if target == "fibery/user" {
                    FieldKind::User
                } else if enum_types.contains(target) {
                    FieldKind::Enum
                } else if is_primitive_type(target) {
                    match target {
                        "fibery/text" => FieldKind::Text,
                        "fibery/date-time" => FieldKind::DateTime,
                        "fibery/date" | "fibery/date-range" => FieldKind::Date,
                        "fibery/int" => FieldKind::Int,
                        "fibery/decimal" => FieldKind::Decimal,
                        "fibery/bool" => FieldKind::Bool,
                        _ => FieldKind::Ignore,
                    }
                } else if domain_types.contains(target) {
                    FieldKind::Relation
                } else {
                    FieldKind::Ignore
                };

                fields.push(FieldDef {
                    name: fname.to_string(),
                    short: short_name(fname).to_lowercase(),
                    kind,
                    target: target.to_string(),
                    is_collection,
                });
            }

            // Enum tables keep their label in `enum/name`, which carries no
            // `ui/title?` marker.
            if is_enum && title_field.is_none() {
                if fields.iter().any(|f| f.name == "enum/name") {
                    title_field = Some("enum/name".to_string());
                }
            }
            // `fibery/user` is a domain type but its title is `user/name`.
            if title_field.is_none() {
                if let Some(f) = fields
                    .iter()
                    .find(|f| f.name.ends_with("/name") && f.kind != FieldKind::Ignore)
                {
                    title_field = Some(f.name.clone());
                }
            }

            let (file, entity_count) = manifest_dbs
                .and_then(|m| m.get(&name))
                .map(|d| {
                    (
                        d["file"].as_str().map(|f| root.join(f)),
                        d["entity_count"].as_u64().unwrap_or(0),
                    )
                })
                .unwrap_or((None, 0));

            dbs.push(DbDef {
                space: space_name(&name).to_string(),
                short: short_name(&name).to_string(),
                name,
                file,
                entity_count,
                fields,
                title_field,
                is_enum,
                is_domain,
                is_system,
            });
        }

        dbs.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Catalog {
            dbs,
            root: root.to_path_buf(),
        })
    }

    /// Databases that produce search results.
    pub fn searchable(&self) -> impl Iterator<Item = &DbDef> {
        self.dbs
            .iter()
            .filter(|d| d.is_domain && !d.is_system && d.file.is_some())
    }

    pub fn by_name(&self, name: &str) -> Option<&DbDef> {
        self.dbs.iter().find(|d| d.name == name)
    }

    /// Resolve what a user typed after `#` to concrete database names.
    ///
    /// This is the core of the "filter by database without using the mouse"
    /// request: `#bet` -> `Engineering/Bet`, `#issue` -> `Engineering/Issue`.
    /// We try progressively looser matches and stop at the first tier that
    /// hits, so an exact name never gets diluted by fuzzy neighbours.
    pub fn resolve_db(&self, needle: &str) -> Vec<&DbDef> {
        let n = needle.trim().trim_matches('"').to_lowercase();
        if n.is_empty() {
            return Vec::new();
        }
        let cands: Vec<&DbDef> = self.searchable().collect();

        // Tier 1: exact full name or exact short name.
        let t1: Vec<&DbDef> = cands
            .iter()
            .copied()
            .filter(|d| d.name.to_lowercase() == n || d.short.to_lowercase() == n)
            .collect();
        if !t1.is_empty() {
            return t1;
        }
        // Tier 2: short name starts with the needle.
        let t2: Vec<&DbDef> = cands
            .iter()
            .copied()
            .filter(|d| d.short.to_lowercase().starts_with(&n))
            .collect();
        if !t2.is_empty() {
            return t2;
        }
        // Tier 3: needle appears anywhere in the qualified name.
        let t3: Vec<&DbDef> = cands
            .iter()
            .copied()
            .filter(|d| d.name.to_lowercase().contains(&n))
            .collect();
        if !t3.is_empty() {
            return t3;
        }
        // Tier 4: subsequence match, so `#custproj` finds `Customer Project`.
        cands
            .into_iter()
            .filter(|d| is_subsequence(&n, &d.short.to_lowercase()))
            .collect()
    }

    /// Map of database name -> index, for compact storage.
    pub fn name_index(&self) -> FxHashMap<String, u32> {
        self.dbs
            .iter()
            .enumerate()
            .map(|(i, d)| (d.name.clone(), i as u32))
            .collect()
    }
}

/// `abc` is a subsequence of `axbxc`.
fn is_subsequence(needle: &str, hay: &str) -> bool {
    let mut it = hay.chars();
    needle.chars().all(|c| it.any(|h| h == c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsequence_matching() {
        assert!(is_subsequence("cp", "customer project"));
        assert!(is_subsequence("custproj", "customer project"));
        assert!(!is_subsequence("zzz", "customer project"));
    }

    #[test]
    fn name_splitting() {
        assert_eq!(short_name("Engineering/Issue"), "Issue");
        assert_eq!(space_name("Engineering/Issue"), "Engineering");
        assert_eq!(short_name("nodelimiter"), "nodelimiter");
    }
}
