//! Raw JSONL row access: id -> (file, byte offset, len), built once and persisted.
use crate::catalog::Catalog;
use crate::util::parse_uuid;
use anyhow::{Context, Result};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize, Default)]
pub struct Rows {
    /// Relative to data root: `databases/X.jsonl`, `documents.jsonl`.
    pub files: Vec<String>,
    /// Catalog db index per file (u32::MAX when not in the catalog).
    pub file_db: Vec<u32>,
    /// Sorted by id. (id, file, offset, len)
    pub offs: Vec<(u128, u16, u64, u32)>,
    /// Sorted. (db index, public id, entity id)
    pub pids: Vec<(u32, u32, u128)>,
    #[serde(skip)]
    root: PathBuf,
    #[serde(skip)]
    handles: Vec<Option<std::fs::File>>,
}

#[derive(Deserialize)]
struct Head {
    #[serde(rename = "fibery/id")]
    id: Option<String>,
    #[serde(rename = "fibery/public-id")]
    pid: Option<String>,
    secret: Option<String>,
}

impl Rows {
    pub fn load_or_build(root: &Path, cache: &Path, cat: &Catalog) -> Result<Rows> {
        let mut rows = match std::fs::read(cache) {
            Ok(b) => bincode::deserialize(&b).context("decode offsets cache")?,
            Err(_) => {
                let t = std::time::Instant::now();
                let r = Self::build(root, cat)?;
                std::fs::create_dir_all(cache.parent().unwrap())?;
                std::fs::write(cache, bincode::serialize(&r)?)?;
                eprintln!("built offsets in {:.1}s", t.elapsed().as_secs_f64());
                r
            }
        };
        rows.root = root.to_path_buf();
        rows.handles = rows
            .files
            .iter()
            .map(|f| std::fs::File::open(root.join(f)).ok())
            .collect();
        Ok(rows)
    }

    fn build(root: &Path, cat: &Catalog) -> Result<Rows> {
        let by_file: FxHashMap<String, u32> = cat
            .dbs
            .iter()
            .enumerate()
            .filter_map(|(i, d)| {
                let f = d.file.as_ref()?.file_name()?.to_str()?.to_string();
                Some((f, i as u32))
            })
            .collect();
        let mut files: Vec<String> = std::fs::read_dir(root.join("databases"))?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".jsonl"))
            .map(|n| format!("databases/{n}"))
            .collect();
        files.sort();
        files.push("documents.jsonl".into());
        let file_db: Vec<u32> = files
            .iter()
            .map(|f| {
                let n = f.rsplit('/').next().unwrap();
                *by_file.get(n).unwrap_or(&u32::MAX)
            })
            .collect();
        let mut offs = Vec::new();
        let mut pids = Vec::new();
        for (fi, f) in files.iter().enumerate() {
            let bytes = std::fs::read(root.join(f)).with_context(|| f.clone())?;
            let mut lines = Vec::new();
            let mut start = 0usize;
            for nl in memchr::memchr_iter(b'\n', &bytes).chain(std::iter::once(bytes.len())) {
                if nl > start {
                    lines.push((start, nl));
                }
                start = nl + 1;
            }
            let db = file_db[fi];
            let got: Vec<(u128, u64, u32, Option<u32>)> = lines
                .par_iter()
                .filter_map(|&(s, e)| {
                    let h: Head = serde_json::from_slice(&bytes[s..e]).ok()?;
                    let id = parse_uuid(h.id.as_deref().or(h.secret.as_deref())?)?;
                    let pid = h.pid.and_then(|p| p.parse().ok());
                    Some((id, s as u64, (e - s) as u32, pid))
                })
                .collect();
            for (id, s, l, pid) in got {
                offs.push((id, fi as u16, s, l));
                if let (Some(p), true) = (pid, db != u32::MAX) {
                    pids.push((db, p, id));
                }
            }
        }
        offs.par_sort_unstable_by_key(|o| o.0);
        pids.par_sort_unstable();
        Ok(Rows { files, file_db, offs, pids, ..Default::default() })
    }

    fn find(&self, id: u128) -> Option<&(u128, u16, u64, u32)> {
        let i = self.offs.binary_search_by_key(&id, |o| o.0).ok()?;
        self.offs.get(i)
    }

    /// The raw JSON row for an id, plus the catalog db index of its file.
    pub fn get(&self, id: u128) -> Option<(serde_json::Value, u32)> {
        let &(_, fi, off, len) = self.find(id)?;
        let fh = self.handles.get(fi as usize)?.as_ref()?;
        let mut buf = vec![0u8; len as usize];
        fh.read_exact_at(&mut buf, off).ok()?;
        Some((serde_json::from_slice(&buf).ok()?, self.file_db[fi as usize]))
    }

    pub fn resolve_pid(&self, db: u32, pid: u32) -> Option<u128> {
        let i = self.pids.partition_point(|p| (p.0, p.1) < (db, pid));
        let p = self.pids.get(i)?;
        (p.0 == db && p.1 == pid).then_some(p.2)
    }
}
