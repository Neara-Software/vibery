# vibery-web API contract (source of truth between server and ui)

Server: Rust (axum), `web/server`, listens on `127.0.0.1:7878`. Serves `web/ui/dist` as static
files with SPA fallback (any non-/api GET -> index.html). UI dev server (vite) proxies `/api` to 7878.

All JSON. Times are unix seconds (i64). Ids are hyphenated lowercase UUID strings.

## GET /api/search?q=<vibery query>&limit=50&sort=relevance|recent|created|oldest
Uses the existing engine (`Parser::parse` + `Engine::search`), same syntax as `vibery syntax`.
```json
{ "hits": [{"id":"…","db":"Shared/Note","title":"…","public_id":"27735","snippet":"…",
            "created":0,"updated":0,"state":null,"inbound":17,"pinned":false}],
  "total": 1526, "by_db": [["Shared/Note", 1200], ["Engineering/Bet", 58]],
  "took_ms": 21.7, "warnings": ["#note -> HubSpot/Note, Shared/Note"], "plan": "…" }
```

## GET /api/entity/:id
```json
{ "id":"…", "db":"Shared/Note", "title":"…", "public_id":"27735",
  "created":0, "updated":0, "created_by": {"id":"…","name":"Sami K"} , "state": null,
  "url": "/Shared/Note/fibery-search-issues-27735",
  "fields":    [{"name":"Shared/Date","value":"2026-07-01"}],
  "relations": [{"field":"Shared/Parent note","items":[{"id":"…","title":"…","db":"Shared/Note"}]}],
  "people":    [{"field":"assignments/assignees","items":[{"id":"…","name":"…"}]}],
  "docs":      [{"field":"Shared/Description","secret":"…","markdown":"…"}],
  "comments":  [{"id":"…","author":"…","created":0,"markdown":"…"}],
  "backlinks": [{"field":"Shared/Child notes","id":"…","title":"…","db":"…"}] }
```
- `fields`: primitive values only (text, number, bool, date), skip null/empty, skip `fibery/*` internals
  except as already surfaced above. Enum/select values show their label.
- `relations`/`people`: resolved via the store (entity titles / user names); skip unresolvable.
- `docs[].markdown`: raw Fibery markdown from `../documents/<secret>.md` (fallback documents.jsonl),
  preprocessed server-side: Fibery mentions `[[label#^typeid/entityid]]`, `[[#^typeid/entityid]]`,
  `[[#@typeid/userid]]` -> `[Title](/e/<entityid>)` (or `@Name` for users). Images stay as
  `/api/files/<id>…` markdown links (the UI requests them via the proxy below).
- 404 `{ "error": "…" }` when unknown.

## GET /api/resolve?db=Shared/Note&pid=27735  ->  {"id":"…"}
Maps Fibery-style URLs (`/<Space>/<Db>/<slug>-<publicid>`) to entity ids.

## GET /api/dbs
`{ "spaces": [{"name":"Shared","dbs":[{"name":"Shared/Note","short":"Note","count":27265}]}] }`
Only databases with indexed entities (count = indexed count is fine if cheap, else omit).

## GET /api/syntax  ->  `{"text": "<the SYNTAX reference string>"}`

## GET /api/files/:fileid
Proxies `https://neara.fibery.io/api/files/<fileid>` with header `Authorization: Token <key>`,
key read from `~/.phab-helper.json` -> `.fibery.api_key`. Streams body + content-type. Cache to
`web/cache/files/` on disk.

## GET /api/doc/:secret  ->  `{"markdown":"…"}` (mentions preprocessed)
Local `../documents/<secret>.md`, else a read-only fetch of
`GET https://neara.fibery.io/api/documents/<secret>?format=md`, cached in `web/cache/docs/`.
Comment prose is NOT in the dump: `/api/entity` `comments[]` carry `"secret"` and `"markdown": null`
until fetched once; the UI should lazily call this endpoint for comments with null markdown
(~1 s first time, <1 ms after; afterwards /api/entity inlines it).

## PUT /api/doc/:secret   body `{"markdown":"…"}`  ->  `{"ok":true,"pushed":false}`
Writes the local copy (`../documents/<secret>.md`). Pushes to Fibery ONLY when env
`VIBERY_WRITE_THROUGH=1` (`PUT https://neara.fibery.io/api/documents/<secret>?format=md` with
`{"content": markdown}`), so the prototype never writes to the shared workspace by default.
