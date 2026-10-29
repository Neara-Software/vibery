# vibery-web: remaining work

Status (2026-10-29): prototype works end to end on 127.0.0.1:7878 (see README.md).
Measured warm: search p50 8 ms, entity p50 1.5 ms, restart 0.14 s, initial JS 87 KB gzip
(markdown 104 KB and highlight 53 KB load lazily).

## Correctness gaps
- **Index is older than the dump.** About 5% of Notes are not in the search index; they open only from
  the raw row. Re-run `vibery ingest` to fix (slow; not done in the timebox).
- **Comment text is not in the dump.** The server fetches it read-only from Fibery on first view
  (`/api/doc/:secret`, ~1 s, then cached in `web/cache/docs`). Fix: scrape comment documents in
  `fetch_documents.py`.
- Fibery highlight spans (`<span bgColor=…>`) now render, one block per span inside table cells.
  That is a guess at Fibery's layout: the md export loses its paragraph breaks in cells.
  Compare with the real page.
- Field row hides `(ignore)` fields and de-dupes creator/assignee, but still shows formula
  fields like "Length of note". Use the schema's hidden/formula flags instead.
- ~4% of mentions point at entities absent from the dump (deleted or unscraped databases); they render as
  "(not in local copy)" and 404. Fibery shows these as inaccessible; could fetch the title live.
- Date-range fields come through as raw JSON.
- URL slugs are approximate (spaces -> `_`); resolution uses only the public id, so links still work.
- `web/cache/offsets.bin` does not rebuild itself when the dump changes. Delete it by hand.
- Unknown `/api/*` paths fall through to the SPA instead of returning a JSON 404.

## Look & feel (vs notes/look.md)
- Missing: Fibery's header row (space name, `»`/close icons, Share/history), ghost "Add <relation>"
  chips for empty relations, Workspace/Personal toggle, sidebar folder grouping (we list spaces
  flat), Inbox/Fibery AI nav.
- Comments sit under the doc, not in a right-hand panel (on purpose: keeps Ctrl-F working).
- Backlinks show title + field only; Fibery shows the paragraph with the mention in it.
- Sidebar databases are not expandable to list entities.
- No dark theme.

## Search
- `sort:recent` results almost all show updated "7 Sept 2026". That is probably a bulk touch (sync or migration)
  in Fibery, not real edits. Consider ranking by last prose/comment change instead.
- Facets list databases only; add person / state / date facets from the same engine.
- No highlighting of matched terms in titles or snippets (engine has no highlighter yet).
- `pins.json` is supported by the engine but has no UI.

## Editing (stretch goal; basic version done)
- Split textarea + live preview, Cmd-S saves to `../documents/<secret>.md` only.
- **The search index is not updated after a save.** Needs an incremental tantivy update for one doc.
- Write-through to Fibery (`VIBERY_WRITE_THROUGH=1`) is wired up but **untested**. Test on a scratch
  note before using. There is no conflict check (no compare of modificationDate before push).
- No editing of title or fields, and no creating entities.

## Live sync (the big one before this could replace Fibery for reading)
- Data is a static snapshot. Next step: poll Fibery for entities with `modification-date > last
  sync` per database, patch the jsonl/offsets/index incrementally, and show "synced N min ago".

## Other databases
- Only Notes were the target. Other databases render through the same generic entity page; check
  Scope/Bet/Issue (workflow state badge, assignee) for obvious breakage.
