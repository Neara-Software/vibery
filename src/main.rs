//! vibery — an experimental search backend for Fibery-shaped knowledge graphs.
//!
//! Built to test one hypothesis: the complaints in the Neara "fibery search
//! issues" note are mostly *expressiveness* problems wearing a performance
//! problem's clothes. If structure (database, author, relation, recency) is
//! indexed as first-class terms rather than as post-filters over a text hit
//! list, the hard queries become easy and the slow ones become fast.

mod catalog;
mod engine;
mod index;
mod ingest;
mod query;
mod store;
mod util;

use anyhow::{Context, Result};
use clap::{Parser as ClapParser, Subcommand};
use engine::{Engine, Results};
use query::{Parser, Sort, Spec};
use std::io::Write;
use std::path::PathBuf;
use util::{format_age, format_datetime, format_uuid, now_secs, parse_uuid};

#[derive(ClapParser)]
#[command(
    name = "vibery",
    version,
    about = "Fast, expressive search over a Fibery workspace dump",
    long_about = None
)]
struct Cli {
    /// Directory holding the Fibery dump (schema.json, databases/, documents.jsonl).
    #[arg(long, global = true, default_value = "..")]
    data: PathBuf,

    /// Directory holding the built index.
    #[arg(long, global = true, default_value = "index")]
    index: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build the index from the raw dump.
    Ingest {
        /// Tantivy indexing heap, in MB (split across threads).
        #[arg(long, default_value_t = 2000)]
        heap_mb: usize,
        /// Indexing threads.
        #[arg(long, default_value_t = 8)]
        threads: usize,
        /// Cap entities per database, for a fast smoke test.
        #[arg(long)]
        limit: Option<usize>,
    },

    /// Run a query.
    Search {
        /// Query string. See `vibery syntax`.
        query: Vec<String>,
        #[arg(long, short = 'n')]
        limit: Option<usize>,
        /// relevance | recent | created | oldest
        #[arg(long)]
        sort: Option<String>,
        /// Print the compiled Tantivy query.
        #[arg(long)]
        explain: bool,
        /// One line per hit, tab-separated: id, db, updated, title.
        #[arg(long)]
        terse: bool,
    },

    /// "What has this person been working on?" — a cross-database activity feed.
    Activity {
        /// Name, email local part, or UUID.
        who: String,
        /// Only entities touched within this many days.
        #[arg(long, default_value_t = 30)]
        days: i64,
        #[arg(long, short = 'n', default_value_t = 25)]
        limit: usize,
        /// Split the feed by how the person is involved.
        #[arg(long)]
        by_role: bool,
    },

    /// "Where does this thing live, and what points at it?"
    Locate {
        /// Entity UUID, public id, or a title to search for.
        target: String,
        /// Disambiguate a public id or title by database, e.g. `--db note`.
        #[arg(long)]
        db: Option<String>,
    },

    /// Show the parsed AST and compiled query without running it.
    Explain { query: Vec<String> },

    /// Time the queries from the search-issues note.
    Bench {
        /// Iterations per query.
        #[arg(long, default_value_t = 5)]
        iters: usize,
    },

    /// Interactive incremental search — the "updates as I type" demo.
    Repl,

    /// List databases, optionally filtered the way `#name` resolves.
    Dbs {
        /// Show only databases matching this the way `#` would.
        needle: Option<String>,
    },

    /// Index statistics.
    Stats,

    /// Query syntax reference.
    Syntax,
}

fn main() {
    if let Err(e) = real_main() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn real_main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Ingest {
            heap_mb,
            threads,
            limit,
        } => {
            let opts = ingest::IngestOpts {
                root: cli.data.clone(),
                out: cli.index.clone(),
                heap_mb: *heap_mb,
                threads: *threads,
                limit: *limit,
            };
            let meta = ingest::run(&opts)?;
            println!();
            println!("Index built in {:.1}s", meta.ingest_secs);
            println!("  {:>10} entities scanned", meta.entities_scanned);
            println!("  {:>10} entities indexed", meta.entities_indexed);
            println!("  {:>10} prose documents folded in", meta.documents_loaded);
            println!("  {:>10} comment threads folded in", meta.comments_loaded);
            println!("  {:>10} MB of text", meta.text_bytes / 1_000_000);
            println!("  index at {}", cli.index.display());
            Ok(())
        }

        Cmd::Search {
            query,
            limit,
            sort,
            explain,
            terse,
        } => {
            let eng = Engine::open(&cli.index)?;
            let q = query.join(" ");
            let (mut spec, warns) = Parser::parse(&q);
            if let Some(n) = limit {
                spec.limit = *n;
            }
            if let Some(s) = sort {
                spec.sort = match s.as_str() {
                    "recent" | "updated" => Sort::Recent,
                    "created" => Sort::Created,
                    "oldest" => Sort::Oldest,
                    _ => Sort::Relevance,
                };
            }
            let res = eng.search(&spec, warns)?;
            if *terse {
                render_terse(&res);
            } else {
                render(&q, &spec, &res, *explain);
            }
            Ok(())
        }

        Cmd::Activity {
            who,
            days,
            limit,
            by_role,
        } => activity(&cli.index, who, *days, *limit, *by_role),

        Cmd::Locate { target, db } => locate(&cli.index, target, db.as_deref()),

        Cmd::Explain { query } => {
            let eng = Engine::open(&cli.index)?;
            let q = query.join(" ");
            let (spec, warns) = Parser::parse(&q);
            println!("query:  {q}");
            println!("sort:   {:?}   limit: {}", spec.sort, spec.limit);
            println!("ast:    {:#?}", spec.root);
            let res = eng.search(&Spec { limit: 0, ..spec }, warns)?;
            println!("plan:   {}", res.plan);
            for w in &res.warnings {
                println!("note:   {w}");
            }
            println!("matches: {}", res.total);
            Ok(())
        }

        Cmd::Bench { iters } => bench(&cli.index, *iters),
        Cmd::Repl => repl(&cli.index),

        Cmd::Dbs { needle } => {
            let eng = Engine::open(&cli.index)?;
            match needle {
                Some(n) => {
                    let dbs = eng.catalog.resolve_db(n);
                    println!("#{n} resolves to {} database(s):", dbs.len());
                    for d in dbs {
                        println!("  {:<50} {:>8} rows", d.name, d.entity_count);
                    }
                }
                None => {
                    let mut dbs: Vec<_> = eng.catalog.searchable().collect();
                    dbs.sort_by_key(|d| std::cmp::Reverse(d.entity_count));
                    println!("{} searchable databases:", dbs.len());
                    for d in dbs {
                        println!("  {:<50} {:>8} rows", d.name, d.entity_count);
                    }
                }
            }
            Ok(())
        }

        Cmd::Stats => {
            let eng = Engine::open(&cli.index)?;
            let m = &eng.meta;
            let searcher = eng.reader.searcher();
            println!("built           {}", format_datetime(m.built_at));
            println!("ingest time     {:.1}s", m.ingest_secs);
            println!("entities seen   {}", m.entities_scanned);
            println!("entities index  {}", m.entities_indexed);
            println!("prose docs      {}", m.documents_loaded);
            println!("comments        {}", m.comments_loaded);
            println!("text            {} MB", m.text_bytes / 1_000_000);
            println!("segments        {}", searcher.segment_readers().len());
            println!("indexed docs    {}", searcher.num_docs());
            println!("store records   {}", eng.store.entity_count()?);
            println!(
                "index size      {} MB",
                dir_size(&cli.index) / 1_000_000
            );
            Ok(())
        }

        Cmd::Syntax => {
            print!("{SYNTAX}");
            Ok(())
        }
    }
}

// ------------------------------------------------------------- rendering

fn render(q: &str, spec: &Spec, res: &Results, explain: bool) {
    let now = now_secs();
    println!();
    println!(
        "\x1b[1m{}\x1b[0m  —  {} match{} in \x1b[32m{:.1} ms\x1b[0m",
        if q.trim().is_empty() { "(everything)" } else { q },
        res.total,
        if res.total == 1 { "" } else { "es" },
        res.took_ms
    );
    for w in &res.warnings {
        println!("  \x1b[2mnote: {w}\x1b[0m");
    }

    // The database breakdown is what makes "narrow it down" a keyboard move:
    // it tells you what `#…` to type next.
    if !res.by_db.is_empty() && res.by_db.len() > 1 {
        // Two spaces can both define e.g. `Note`; showing "#Note 200 · #Note 200"
        // is useless, so qualify a short name whenever it is ambiguous here.
        let mut seen: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        for (d, _) in &res.by_db {
            *seen.entry(catalog::short_name(d)).or_insert(0) += 1;
        }
        let shown: Vec<String> = res
            .by_db
            .iter()
            .take(8)
            .map(|(d, n)| {
                let short = catalog::short_name(d);
                if seen.get(short).copied().unwrap_or(0) > 1 {
                    format!("#{d} {n}")
                } else {
                    format!("#{short} {n}")
                }
            })
            .collect();
        let more = res.by_db.len().saturating_sub(8);
        println!(
            "  \x1b[2min: {}{}\x1b[0m",
            shown.join("  ·  "),
            if more > 0 { format!("  · +{more} more") } else { String::new() }
        );
    }
    println!();

    if res.hits.is_empty() {
        println!("  no results");
        println!();
        return;
    }

    for (i, h) in res.hits.iter().enumerate() {
        let title = if h.title.is_empty() {
            "(untitled)"
        } else {
            &h.title
        };
        println!(
            "{:>3}. \x1b[1;36m{}\x1b[0m",
            i + 1,
            truncate(title, 100)
        );
        let mut meta = vec![format!("\x1b[33m{}\x1b[0m", h.db)];
        if !h.public_id.is_empty() {
            meta.push(format!("#{}", h.public_id));
        }
        if let Some(s) = &h.state {
            meta.push(format!("[{s}]"));
        }
        meta.push(format_age(h.updated, now));
        if h.inbound > 0 {
            meta.push(format!("{}↩", h.inbound));
        }
        println!("     \x1b[2m{}\x1b[0m", meta.join("  ·  "));
        if !h.snippet.is_empty() {
            println!("     {}", truncate(&h.snippet, 150));
        }
    }
    println!();
    if explain {
        println!("\x1b[2mplan: {}\x1b[0m", res.plan);
        println!("\x1b[2mast:  {:?}\x1b[0m", spec.root);
        println!();
    }
}

fn render_terse(res: &Results) {
    for h in &res.hits {
        println!(
            "{}\t{}\t{}\t{}",
            h.id,
            h.db,
            util::format_date(h.updated),
            h.title
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

fn dir_size(p: &std::path::Path) -> u64 {
    fn walk(p: &std::path::Path, acc: &mut u64) {
        if let Ok(rd) = std::fs::read_dir(p) {
            for e in rd.flatten() {
                if let Ok(md) = e.metadata() {
                    if md.is_dir() {
                        walk(&e.path(), acc);
                    } else {
                        *acc += md.len();
                    }
                }
            }
        }
    }
    let mut acc = 0;
    walk(p, &mut acc);
    acc
}

// -------------------------------------------------------------- activity

/// The "what has my teammate been working on?" report.
///
/// This is one index query, not a view: `touched:<uuid>` sorted by recency,
/// across every database at once.
fn activity(index: &std::path::Path, who: &str, days: i64, limit: usize, by_role: bool) -> Result<()> {
    let eng = Engine::open(index)?;
    let matched = eng.resolve_users(who);
    if matched.is_empty() {
        anyhow::bail!("no user matched `{who}`");
    }
    let (uid, user) = matched[0];
    if matched.len() > 1 {
        eprintln!(
            "note: `{who}` matched {} users; using {}",
            matched.len(),
            user.name
        );
    }
    println!();
    println!(
        "\x1b[1m{}\x1b[0m  <{}>   last {days} days",
        user.name, user.email
    );

    // The user directory is excluded: being named in someone's `Reporters` or
    // `Manager` field is not "work they did", and those rows all move together
    // whenever the directory syncs.
    let u = format_uuid(uid);
    let common = format!("-db:fibery/user updated:-{days}d sort:recent limit:{limit}");
    let roles: Vec<(&str, String)> = if by_role {
        vec![
            ("created", format!("by:@{u} {common}")),
            ("commented", format!("commented-by:@{u} {common}")),
            ("assigned", format!("assignees:@{u} {common}")),
        ]
    } else {
        vec![("touched", format!("touched-by:@{u} {common}"))]
    };

    let now = now_secs();
    for (label, qs) in roles {
        let (spec, warns) = Parser::parse(&qs);
        let res = eng.search(&spec, warns)?;
        println!();
        println!(
            "\x1b[1;35m{}\x1b[0m  —  {} match{} in \x1b[32m{:.1} ms\x1b[0m",
            label,
            res.total,
            if res.total == 1 { "" } else { "es" },
            res.took_ms
        );
        if !res.by_db.is_empty() {
            let shown: Vec<String> = res
                .by_db
                .iter()
                .take(6)
                .map(|(d, n)| format!("{} {}", catalog::short_name(d), n))
                .collect();
            println!("  \x1b[2m{}\x1b[0m", shown.join("  ·  "));
        }
        for h in &res.hits {
            println!(
                "  \x1b[2m{:>8}\x1b[0m  \x1b[33m{:<28}\x1b[0m {}",
                format_age(h.updated, now),
                truncate(catalog::short_name(&h.db), 28),
                truncate(if h.title.is_empty() { "(untitled)" } else { &h.title }, 78)
            );
        }
    }
    println!();
    Ok(())
}

// ---------------------------------------------------------------- locate

/// "Where am I?" — the context an entity sits in, plus what points at it.
fn locate(index: &std::path::Path, target: &str, db: Option<&str>) -> Result<()> {
    let eng = Engine::open(index)?;

    let id = match parse_uuid(target) {
        Some(v) => v,
        None => {
            // A public id is only unique *within* a database in Fibery, so this
            // can legitimately match several entities. Fall back to a title
            // search when it looks like text rather than a number.
            // Quote it: database names contain spaces (`Exchange Rate`), and an
            // unquoted `#exchange rate` would lex `rate` as a free-text word.
            let dbq = db.map(|d| format!(" #\"{d}\"")).unwrap_or_default();
            let q = if target.chars().all(|c| c.is_ascii_digit()) {
                format!("pid:{target}{dbq}")
            } else {
                format!("title:\"{target}\"{dbq}")
            };
            let (spec, warns) = Parser::parse(&q);
            let res = eng.search(&Spec { limit: 10, ..spec }, warns)?;
            match res.hits.len() {
                0 => anyhow::bail!(
                    "nothing found for `{target}`{}",
                    db.map(|d| format!(" in #{d}")).unwrap_or_default()
                ),
                1 => parse_uuid(&res.hits[0].id).context("bad id in index")?,
                _ => {
                    println!();
                    println!(
                        "`{target}` matches {} entities — narrow it with --db, or pass a uuid:",
                        res.total
                    );
                    for h in &res.hits {
                        println!(
                            "  \x1b[33m{:<44}\x1b[0m {}  \x1b[2m{}\x1b[0m",
                            h.db,
                            truncate(if h.title.is_empty() { "(untitled)" } else { &h.title }, 50),
                            h.id
                        );
                    }
                    println!();
                    return Ok(());
                }
            }
        }
    };

    let rec = eng
        .entity(id)
        .context("store lookup")?
        .with_context(|| format!("no entity {}", format_uuid(id)))?;

    let now = now_secs();
    println!();
    println!(
        "\x1b[1;36m{}\x1b[0m",
        if rec.title.is_empty() { "(untitled)" } else { &rec.title }
    );
    println!("  \x1b[33m{}\x1b[0m  #{}", eng.db_name(rec.db), rec.public_id);
    println!("  id      {}", format_uuid(id));
    println!(
        "  created {}   updated {} ({})",
        format_datetime(rec.created),
        format_datetime(rec.updated),
        format_age(rec.updated, now)
    );
    if let Some(cb) = rec.created_by {
        if let Some(u) = eng.store.get_user(cb)? {
            println!("  by      {} <{}>", u.name, u.email);
        }
    }
    if let Some(s) = &rec.state {
        println!("  state   {s}");
    }

    // Outbound context: the named relations this entity sits inside. This is
    // the answer to "what folder is this in?" generalised to a graph — a Fibery
    // entity's location *is* its set of parents.
    if !rec.rels.is_empty() {
        println!();
        println!("  \x1b[1mcontext\x1b[0m  ({} outbound relation{})", rec.rels.len(), if rec.rels.len()==1 {""} else {"s"});
        let ids: Vec<u128> = rec.rels.iter().map(|(_, t)| *t).collect();
        let targets = eng.store.get_entities(&ids)?;
        let mut shown = 0;
        for ((fid, tid), t) in rec.rels.iter().zip(targets.iter()) {
            let Some(t) = t else { continue };
            if t.title.is_empty() {
                continue;
            }
            if shown >= 24 {
                println!("    \x1b[2m… and more\x1b[0m");
                break;
            }
            shown += 1;
            println!(
                "    \x1b[2m{:<28}\x1b[0m {} \x1b[2m({} · {})\x1b[0m",
                truncate(catalog::short_name(eng.field_name(*fid)), 28),
                truncate(&t.title, 60),
                catalog::short_name(eng.db_name(t.db)),
                format_uuid(*tid).split('-').next().unwrap_or("")
            );
        }
    }

    // Inbound: who points here. Free from the `rel` index.
    let back = eng.backlinks(id, 20)?;
    if !back.is_empty() {
        println!();
        println!("  \x1b[1mreferenced by\x1b[0m  ({} shown)", back.len());
        for (_, r) in &back {
            println!(
                "    \x1b[33m{:<28}\x1b[0m {}",
                truncate(catalog::short_name(eng.db_name(r.db)), 28),
                truncate(if r.title.is_empty() { "(untitled)" } else { &r.title }, 70)
            );
        }
    }
    println!();
    Ok(())
}

// ----------------------------------------------------------------- bench

/// Times the queries the note actually complains about, plus the ones it asks
/// for but Fibery cannot express.
fn bench(index: &std::path::Path, iters: usize) -> Result<()> {
    let eng = Engine::open(index)?;

    let cases: Vec<(&str, &str)> = vec![
        ("issue: field filter with empty text", "by:@sami"),
        ("  same, most recent first", "by:@sami sort:recent"),
        ("  same, narrowed to one database", "by:@sami #note"),
        ("issue: plain text search", "interventions"),
        ("issue: two-word text search", "load flow analysis"),
        ("issue: quoted phrase", "\"point cloud\""),
        ("ask: #database filter (RVO #Bet)", "rvo #bet"),
        ("  prefix-as-you-type", "interv*"),
        ("ask: notes related to a team, containing a word",
         "#note related:(#team platform) interventions"),
        ("ask: teammate activity, cross-database",
         "touched-by:@michael updated:-30d sort:recent"),
        ("  narrowed to comments only", "commented-by:@michael updated:-90d"),
        ("expressive: boolean + negation", "crash OR failure -#issue"),
        ("expressive: date window", "#issue created:2026-01..2026-06"),
        ("expressive: state + assignee", "#issue is:open has:assignee"),
        ("expressive: title-scoped", "title:keycloak"),
        ("worst case: match everything", ""),
    ];

    println!();
    println!(
        "{:<50} {:>9} {:>9} {:>9}",
        "query", "matches", "count", "full"
    );
    println!("{}", "─".repeat(80));

    let median = |mut v: Vec<f64>| -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timings"));
        v[v.len() / 2]
    };

    for (label, q) in cases {
        let (spec, _) = Parser::parse(q);
        // Two measurements: matching alone, and the whole user-visible path
        // (match + rank + fetch + decode the top 20 stored documents).
        let full_spec = Spec { limit: 20, ..spec.clone() };
        let mut count_ms = Vec::with_capacity(iters);
        let mut full_ms = Vec::with_capacity(iters);
        let mut total = 0usize;
        for _ in 0..iters {
            let (n, ms) = eng.count(&spec)?;
            total = n;
            count_ms.push(ms);
            let t = std::time::Instant::now();
            let _ = eng.search(&full_spec, Vec::new())?;
            full_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        println!(
            "{:<50} {:>9} {:>7.2}ms {:>7.2}ms",
            truncate(label, 50),
            total,
            median(count_ms),
            median(full_ms)
        );
        if !q.is_empty() {
            println!("  \x1b[2m{q}\x1b[0m");
        }
    }
    println!();
    println!("\x1b[2mcount = matching only.  full = match + rank + fetch top 20 stored docs,");
    println!("i.e. everything behind one keystroke. Median of {iters} runs, single");
    println!("thread, warm page cache.\x1b[0m");
    println!();
    Ok(())
}

// ------------------------------------------------------------------ repl

/// Interactive search. Every keystroke-complete line is re-run from scratch, so
/// the latency you see is the real per-query latency.
fn repl(index: &std::path::Path) -> Result<()> {
    let eng = Engine::open(index)?;
    let mut rl: rustyline::DefaultEditor = rustyline::DefaultEditor::new()?;
    println!();
    println!("vibery repl — {} entities indexed.", eng.meta.entities_indexed);
    println!("Try:  rvo #bet   ·   by:@sami sort:recent   ·   #note related:(#team platform)");
    println!("`\\syntax` for the reference, `\\q` to quit.");
    println!();
    loop {
        let line = match rl.readline("› ") {
            Ok(l) => l,
            Err(rustyline::error::ReadlineError::Interrupted)
            | Err(rustyline::error::ReadlineError::Eof) => break,
            Err(e) => return Err(e.into()),
        };
        let t = line.trim();
        if t == "\\q" || t == "\\quit" {
            break;
        }
        if t == "\\syntax" {
            print!("{SYNTAX}");
            continue;
        }
        if t.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(t);
        let (mut spec, warns) = Parser::parse(t);
        spec.limit = spec.limit.min(10);
        match eng.search(&spec, warns) {
            Ok(res) => render(t, &spec, &res, false),
            Err(e) => println!("  error: {e:#}"),
        }
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

const SYNTAX: &str = "\
vibery query syntax
═══════════════════

TEXT
  interventions              a word, anywhere (title, prose, or comments)
  \"load flow\"                a phrase, in order
  interv*                    prefix — what incremental search-as-you-type uses
  title:keycloak             restrict to the title
  body:scim                  restrict to prose
  comments:regression        restrict to comment threads

DATABASE  (the `RVO #Bet` request — no mouse required)
  #bet                       resolved fuzzily: #bet, #Bet, #Engineering/Bet
  #issue #note               several databases at once (OR)
  space:engineering          every database in a space

PEOPLE  (each of these works with no text term at all)
  @sami                      involved in any way
  by:@sami                   created it
  commented-by:@michael      commented on it
  assignees:@tom             assigned to it
  touched-by:@sami           created, assigned, commented, reviewed, …
  any `fibery/user` field on any database can be used as a filter name

RELATIONS  (the \"things related to other things\" request)
  related:(#team platform)   related to anything matching the subquery
  team:(platform)            related through a *named* relation field
  #note related:(#team platform) interventions
                             — Notes, linked to the Platform team,
                               containing \"interventions\"

TIME
  updated:-14d               touched in the last 14 days
  created:>2026-01-01        after a date
  created:2026-01..2026-06   a window
  updated:today | week | month | quarter | year
  units: h, d, w, mo, y

STRUCTURE
  is:open / is:closed        workflow state
  state:\"In Review\"          an exact state
  has:description            has prose
  has:comments               has discussion
  has:assignee               has somebody on it
  priority:p1                any single-select field, by label
  pid:27735                  Fibery public id
  id:<uuid>                  exact entity

COMBINING
  a b                        both (implicit AND)
  a OR b                     either
  -a  /  NOT a               exclude
  ( … )                      group

OUTPUT
  sort:recent | created | oldest | relevance
  limit:50
";
