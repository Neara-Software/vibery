# vibery

An experimental search backend for Fibery-shaped knowledge graphs, in Rust.

Built against a local clone of the Neara Fibery workspace (3.17M entities, 568
databases, 183 MB of prose) to test one hypothesis about the complaints in
[fibery search issues](https://neara.fibery.io/Shared/Note/fibery-search-issues-27735):

> The reported problems are mostly **expressiveness** problems wearing a
> performance problem's clothes.

Status: exploratory spike. Read-only, single-node, no auth. See
[Not done yet](#not-done-yet).

---

## The diagnosis

Fibery's search is text-first. Its own API surface gives it away — the search
endpoint accepts a `query` string, an optional `database`, and an optional
`viewType`, and nothing else. No author, no date, no relation. Results come
back with `elastic_highlight` markers, so there is an Elasticsearch behind it
doing BM-25 over titles, descriptions, document bodies and comments.

That shape explains the reported bugs directly:

**"`Created by: Sami Kyostila` with empty text string → only 2 docs."**
If structure is a post-filter over a text-matched candidate set, then an empty
text string produces no candidate set, and there is nothing for the filter to
narrow. The filter isn't broken; it's downstream of a match that never happened.

**"`RVO` as a search term is useless… I will not see it unless I filter by
database, because we use Notes for everything."**
Correct, and the numbers back him up. `rvo` matches 2,569 entities spread over
54 databases in this workspace. The signal he wants — 58 Bets — is 2% of what
he's shown. Database is the highest-value filter in this workspace and it costs
mouse clicks.

**"Finding things related to other things — there is no natural way to do that."**
There isn't, because relations were never indexed as searchable terms. Fibery
makes you leave search and build a view.

There is also a haystack problem underneath all of it. Of 3.17M rows in this
workspace, 2.08M are `Collaboration~Documents/Document` — and only 453k of
those hold any content at all. A further 284k domain rows have no title and no
prose (HubSpot emails with a null subject, empty join rows, placeholder
records). **Roughly 88% of the rows are not things anyone would want to find.**

So the fix isn't "the same search, faster". It's:

1. Index **structure as first-class terms** — database, author, participant,
   relation, state, recency — so a query with no text at all is still an
   ordinary index lookup.
2. **Denormalise text onto the entity that owns it**, so one lookup finds the
   thing you were looking for rather than the document row hanging off it.
3. **Drop the 88%** that is scaffolding.

Do those three and the speed arrives as a side effect.

---

## Results

Every documented issue and request, with the query that answers it. Timings are
the full user-visible path — match, rank, fetch and decode the top 20 stored
documents — median of 9 runs, single thread, warm cache, on 390,814 indexed
entities.

| From the note | vibery query | Result | Full latency |
|---|---|---|---|
| `Created by: Sami` + empty text → **2 docs** | `by:@sami` | **456** matches | **0.27 ms** |
| …most recent first | `by:@sami sort:recent` | 456 | 0.21 ms |
| "`RVO #Bet`… update filtering as I type" | `rvo #bet` | 58 Bets (from 2,569 bare `rvo`) | 0.06 ms |
| …as-you-type prefix | `interv*` | 3,429 | 0.91 ms |
| "all Notes related to the Platform team that contain *Interventions*" | `#note related:(#team platform) interventions` | 21 | 0.28 ms |
| "what has my teammate worked on or commented on recently" | `touched-by:@michael updated:-30d sort:recent` | 2,184 across 67 dbs | 0.58 ms |
| …just their comments | `commented-by:@michael updated:-90d` | 507 | 0.29 ms |
| "locating your current document is basically impossible" | `vibery locate "Keycloak / SCIM Uplift"` | parents, teams, backlinks | 1 ms |
| Search is slow | `"point cloud"` (phrase, 9,374 hits) | worst real query | 1.22 ms |
| — | match everything (390,814) | worst case overall | 3.39 ms |

`vibery bench` reproduces the whole table.

Two of these deserve a note. The `by:@sami` case went from 2 results to 456
purely because the filter no longer needs a text match to stand on. And the
relation query works: its own second-ranked result is the Shared/Note asking
for the feature.

### Full ingest

```
675,109 entities scanned      (3.17M rows minus 2.5M documents/comments/refs)
390,814 entities indexed      (284k contentless rows dropped)
124,058 prose documents folded into their owning entities
154,233 comment threads folded into their owning entities
    183 MB of text
   11.5 s wall clock          (8 threads, 18-core M-series)
    471 MB on disk            (239 MB Tantivy + 233 MB redb)
```

---

## Architecture

```
schema.json ─┐
manifest.json┼─► catalog ──► ingest ──┬──► Tantivy   "which entities match?"
databases/*  ┤                        └──► redb      "tell me about THIS id"
documents.jsonl ┘                                     (rendering, locate, graph walks)
```

**Tantivy** (Rust, Lucene-shaped) for the inverted index. **redb** (pure-Rust
embedded KV, ACID, mmap) for an id → record map. Two stores because they answer
different questions: Tantivy is great at "which docs match these terms" and bad
at "fetch me record X by primary key"; redb is the reverse. `locate` and
relation-name rendering are primary-key lookups.

Four decisions did most of the work:

**Structure is terms, not filters.** `db`, `space`, `actor`, `rel`, `relf`,
`enumv`, `has`, `state` are all indexed string fields; dates are indexed fast
fields. A query with no free text is a normal term lookup, so `by:@sami` costs
0.01 ms to match. This is the whole fix for the reported bug.

**Text is denormalised onto the entity that owns it.** Prose lives in separate
`Document` rows keyed by a `secret`, and discussion lives in `comment` rows.
Ingest folds both into `body`/`comments` on the owning entity. You search for
the Note, not the document row hanging off it. Comments stay a separate field
so `comments:` can scope to them and so a hit inside a thread can be ranked
lower than a hit in the title.

**Every `fibery/user` field is a participation edge.** There are 812 of them
across the schema — assignee, reviewer, tech lead, security owner, last
reviewed by. Rather than enumerating roles, ingest treats all of them uniformly:
each writes `<role>:<uuid>` *and* a role-agnostic `touched:<uuid>`. That single
decision is what makes "what has my teammate been working on" a one-term query
across all 211 databases instead of a view someone has to build.

**Relations are terms on both sides, so traversal is a subquery.**
`related:(#team platform)` runs the inner query, collects up to 4,096 anchor
ids, and rewrites itself as a disjunction over `rel`. One extra index pass per
hop, and hops nest. Doc mentions (`Collaboration~Documents/Reference`) are
folded in as symmetric edges too, so "related to" covers *mentioned alongside*
and not just *typed relation field*.

Smaller things that turned out to matter:

- **`#database` resolution is fuzzy and tiered** — exact name, then exact short
  name, then prefix, then substring, then subsequence — stopping at the first
  tier that hits so an exact match is never diluted. `#bet` → `Engineering/Bet`;
  `#custproj` → `Customer Project`. This is the keyboard-only filtering Michael
  asked for.
- **The per-database facet count rides along in the same index pass**, via a
  custom Tantivy collector reading the `db` fast field by term ordinal. Every
  result set tells you what to type next (`in: #Note 1519 · #Task 208 · …`),
  which is what makes narrowing a keyboard move rather than a mouse hunt.
- **`is:open` reads the schema's own `workflow/Final` flag**, not a list of
  state names. 347 workflow tables each name their states differently; the flag
  is authoritative. It partitions exactly: 612 open + 7,713 closed = 8,325 issues.
- **Ranking** is BM-25 with a title boost (6×) over body (1×) over comments
  (0.4×), nudged by `log(inbound_references)` so a widely-cited note beats an
  incidental mention.

---

## Query language

```
TEXT      interventions · "load flow" · interv* · title:keycloak · comments:regression
DATABASE  #bet · #issue #note · space:engineering
PEOPLE    @sami · by:@sami · commented-by:@michael · assignees:@tom · touched-by:@sami
RELATIONS related:(#team platform) · team:(platform)
TIME      updated:-14d · created:>2026-01-01 · created:2026-01..2026-06 · updated:today
STRUCTURE is:open · is:closed · state:"In Review" · has:description · priority:p1 · pid:27735
COMBINE   a b · a OR b · -a · ( … )
OUTPUT    sort:recent|created|oldest|relevance · limit:50
```

`vibery syntax` prints the full reference. Any `fibery/user` field on any
database works as a filter name, and so does any single-select field.

---

## Usage

```bash
cargo build --release

# Build the index (~12s). --data defaults to .. , --index to ./index
./target/release/vibery ingest

./target/release/vibery search '#note related:(#team platform) interventions'
./target/release/vibery search 'by:@sami sort:recent' -n 10
./target/release/vibery activity tokar --days 21 --by-role
./target/release/vibery locate "Keycloak / SCIM Uplift"
./target/release/vibery explain 'rvo #bet'      # AST + compiled query
./target/release/vibery bench                   # the table above
./target/release/vibery repl                    # interactive
./target/release/vibery dbs note                # what does #note resolve to?
./target/release/vibery stats
```

`cargo test` runs 31 unit tests (lexer, parser, date windows, UUID/date codecs,
markdown flattening, user-resolution scoring).

---

## Not done yet

Flagging these because several are load-bearing for anything real, not just
polish:

- **No permissions.** Fibery marks 602 of 632 types `fibery/secured?` and has
  per-entity ACLs. This index would happily show anyone anything. A real
  version needs ACL terms indexed per document and intersected at query time —
  and that constraint may change the ranking design, since a filter that
  removes most candidates wants to run first.
- **No incremental updates.** Ingest is a full rebuild. It's only 12 seconds,
  so this is fine for a spike, but real use wants Fibery's webhooks driving
  single-document upserts.
- **No semantic search.** 16 types have `semantic-index/enabled`. Everything
  here is lexical; "find the doc about authentication uplift" won't match
  "Keycloak / SCIM" unless the words appear. Worth trying a hybrid.
- **Relation traversal is capped** at 4,096 anchors per hop, and each hop is a
  separate pass. Fine for the queries above; would need a real adjacency
  structure for deep or high-fan-out graph queries.
- **`locate` shows graph context, not folder position.** Michael asked "what
  folder is this in on my sidebar". Folder/view hierarchy isn't in this clone,
  so `locate` answers the generalisation — parents, teams, and backlinks — which
  I think is the more useful answer, but it is not literally what he asked for.
- **Ranking is untuned.** The boosts are guesses that look right on a few dozen
  queries. No relevance judgements, so no evidence.
- **Prefix search uses a regex automaton** over the term dictionary. Fine at
  0.9 ms, but a dedicated n-gram or FST prefix index would be better for true
  per-keystroke latency on the last word.
- Timings are single-node, warm-cache, on one machine, against a static
  snapshot. They are not a like-for-like comparison with Fibery's hosted
  Elasticsearch under real load — the architectural claims are the point, not
  the millisecond counts.
