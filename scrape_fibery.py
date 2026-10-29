#!/usr/bin/env python3
"""
Fibery Data Scraper
===================
Scrapes all schema and entity data from Fibery REST API (e.g. https://neara.fibery.io)
and saves it to local disk as JSONL files suitable for database backups or prototyping
a reimplementation (such as in Rust).

Features:
  - Strict rate-limiting (default: max 1 request/second)
  - Automatic authentication using ~/.phab-helper.json (or CLI / env vars)
  - Full schema introspection and extraction (saved to schema.json)
  - Dynamic field projection handling primitives, to-one references, collections, and rich-text docs
  - Resilient cursor-based pagination with automatic retry and exponential backoff
  - Streaming JSONL output (1 file per database, memory-efficient for large tables)
  - Checkpointing & resumption via progress.json (can stop and resume anytime)
  - Manifest generation (manifest.json) mapping databases, files, counts, and field types
  - Optional batch fetching of rich-text markdown documents (/api/documents/commands)
  - Filterable by database name and support for entity limits for testing
"""

import argparse
import json
import math
import os
import re
import sys
import time
import urllib.error
import urllib.parse
import urllib.request
from datetime import datetime, timedelta, timezone
from typing import Any, Dict, List, Optional, Set, Tuple

DOC_DB = "Collaboration~Documents/Document"
# Deltas re-read this far behind the last sync: covers clock skew and
# writes that were in flight while the previous run was paging.
WATERMARK_MARGIN = timedelta(minutes=10)


class RateLimiter:
    """Enforces a maximum request rate (requests per second)."""

    def __init__(self, requests_per_second: float = 1.0):
        self.min_interval = 1.0 / max(requests_per_second, 0.001)
        self.last_request_time = 0.0

    def wait(self):
        """Block until min_interval has elapsed since the last request."""
        now = time.time()
        elapsed = now - self.last_request_time
        if elapsed < self.min_interval:
            sleep_time = self.min_interval - elapsed
            time.sleep(sleep_time)
        self.last_request_time = time.time()


class FiberyClient:
    """Client for Fibery HTTP API."""

    def __init__(self, workspace: str, api_key: str, rate_limiter: RateLimiter, verbose: bool = False):
        self.workspace = workspace
        self.api_key = api_key
        self.rate_limiter = rate_limiter
        self.verbose = verbose
        self.commands_url = f"https://{workspace}.fibery.io/api/commands"
        self.documents_url = f"https://{workspace}.fibery.io/api/documents/commands"
        self.request_count = 0

    def _send_request(self, url: str, payload: Any, max_retries: int = 5) -> Any:
        """Send a POST request with rate limiting and retry backoff."""
        data = json.dumps(payload).encode("utf-8")
        headers = {
            "Authorization": f"Token {self.api_key}",
            "Content-Type": "application/json",
            "User-Agent": "Fibery-Data-Scraper/1.0",
        }

        for attempt in range(max_retries):
            self.rate_limiter.wait()
            self.request_count += 1
            req = urllib.request.Request(url, data=data, headers=headers, method="POST")

            try:
                with urllib.request.urlopen(req, timeout=120) as resp:
                    raw = resp.read().decode("utf-8")
                    return json.loads(raw)
            except urllib.error.HTTPError as e:
                status = e.code
                error_body = ""
                try:
                    error_body = e.read().decode("utf-8")
                except Exception:
                    pass

                # Retry on rate limiting (429) or transient server errors (500, 502, 503, 504)
                if status in (429, 500, 502, 503, 504) and attempt < max_retries - 1:
                    backoff = (2 ** attempt) + (attempt * 1.5)
                    print(f"  [WARN] HTTP {status} received ({error_body[:120]}). Retrying in {backoff:.1f}s (attempt {attempt + 1}/{max_retries})...", file=sys.stderr)
                    time.sleep(backoff)
                    continue
                else:
                    raise RuntimeError(f"HTTP {status} from {url}: {error_body}") from e
            except (urllib.error.URLError, TimeoutError, ConnectionResetError) as e:
                if attempt < max_retries - 1:
                    backoff = (2 ** attempt) + 1.0
                    print(f"  [WARN] Network error ({e}). Retrying in {backoff:.1f}s (attempt {attempt + 1}/{max_retries})...", file=sys.stderr)
                    time.sleep(backoff)
                    continue
                else:
                    raise RuntimeError(f"Network error after {max_retries} attempts: {e}") from e

        raise RuntimeError(f"Failed request to {url} after {max_retries} retries")

    def execute_commands(self, commands: List[Dict[str, Any]]) -> List[Dict[str, Any]]:
        """Execute a batch of commands via /api/commands."""
        res = self._send_request(self.commands_url, commands)
        if not isinstance(res, list):
            raise ValueError(f"Expected list response from /api/commands, got: {type(res)}")
        return res

    def execute_single_command(self, command: str, args: Dict[str, Any]) -> Any:
        """Execute a single command via /api/commands and return its result."""
        res = self.execute_commands([{"command": command, "args": args}])
        cmd_res = res[0]
        if not cmd_res.get("success"):
            err = cmd_res.get("result", {})
            err_msg = err.get("message") if isinstance(err, dict) else str(err)
            raise RuntimeError(f"Command '{command}' failed: {err_msg}")
        return cmd_res.get("result")

    def get_schema(self, with_description: bool = True) -> Dict[str, Any]:
        """Fetch full Fibery schema."""
        return self.execute_single_command("fibery.schema/query", {"with-description?": with_description})

    def count_entities(self, db_name: str) -> int:
        """Count total entities in a single database."""
        q = {
            "q/from": db_name,
            "q/select": ["q/count", ["fibery/id"]]
        }
        res = self.execute_single_command("fibery.entity/query", {"query": q})
        return int(res) if isinstance(res, (int, float)) else 0

    def batch_count_entities(self, db_names: List[str], batch_size: int = 50) -> Dict[str, int]:
        """Count entities for multiple databases in batched requests."""
        results = {}
        for i in range(0, len(db_names), batch_size):
            chunk = db_names[i:i + batch_size]
            batch = [
                {"command": "fibery.entity/query", "args": {"query": {"q/from": name, "q/select": ["q/count", ["fibery/id"]]}}}
                for name in chunk
            ]
            try:
                responses = self.execute_commands(batch)
                for name, r in zip(chunk, responses):
                    # Failed counts are left out rather than reported as 0, so
                    # --update can't mistake an API error for "everything deleted".
                    if r.get("success"):
                        cnt = r.get("result", 0)
                        results[name] = int(cnt) if isinstance(cnt, (int, float)) else 0
            except Exception as e:
                print(f"  [WARN] Failed to batch count chunk {i}-{i+len(chunk)}: {e}", file=sys.stderr)
        return results

    def query_page(
        self,
        db_name: str,
        select_fields: List[Any],
        page_size: int,
        last_seen_id: Optional[str] = None,
        since_field: Optional[str] = None,
        since: Optional[str] = None,
    ) -> List[Dict[str, Any]]:
        """Query a page of entities using cursor pagination.

        With since_field/since set, returns rows with since_field >= since, keyset-paged
        on (since_field, fibery/id): `since` and `last_seen_id` are the last row's pair.
        Ordering a filtered 2M-row table by id alone times out server-side."""
        query: Dict[str, Any] = {
            "q/from": db_name,
            "q/select": select_fields,
            "q/order-by": [[["fibery/id"], "q/asc"]],
            "q/limit": page_size + 1,  # Request 1 extra to determine if next page exists
        }

        params: Dict[str, Any] = {}
        if since is not None:
            query["q/order-by"] = [[[since_field], "q/asc"], [["fibery/id"], "q/asc"]]
            query["q/where"] = ["q/or", [">", [since_field], "$since"],
                                ["q/and", ["=", [since_field], "$since"], [">", ["fibery/id"], "$last-seen-id"]]]
            params["$since"] = since
            params["$last-seen-id"] = last_seen_id or "00000000-0000-0000-0000-000000000000"
        elif last_seen_id is not None:
            query["q/where"] = [">", ["fibery/id"], "$last-seen-id"]
            params["$last-seen-id"] = last_seen_id

        args: Dict[str, Any] = {"query": query}
        if params:
            args["params"] = params

        res = self.execute_single_command("fibery.entity/query", args)
        if not isinstance(res, list):
            return []
        return res

    def batch_get_documents(self, secrets: List[str], format: str = "md") -> List[Dict[str, Any]]:
        """Batch fetch markdown/content for rich-text documents via /api/documents/commands."""
        if not secrets:
            return []
        payload = {
            "command": "get-documents",
            "args": [{"secret": s, "format": format} for s in secrets]
        }
        res = self._send_request(self.documents_url, payload)
        if isinstance(res, list):
            return res
        return []


def sanitize_filename(name: str) -> str:
    """Convert a database name like 'Engineering/Issue' into a safe filesystem filename."""
    safe = name.replace("/", "__").replace(" ", "_")
    safe = re.sub(r'[^a-zA-Z0-9_~-]', "_", safe)
    return safe


class SchemaAnalyzer:
    """Analyzes schema and determines queryable databases and field selectors."""

    def __init__(self, raw_schema: Dict[str, Any], collection_limit: int = 500):
        self.raw_schema = raw_schema
        self.types = raw_schema.get("fibery/types", [])
        self.types_by_name = {t["fibery/name"]: t for t in self.types}
        self.collection_limit = collection_limit

    def get_queryable_databases(self) -> List[str]:
        """Return all databases (types) that can be queried for entities."""
        queryable = []
        for t in self.types:
            name = t["fibery/name"]
            meta = t.get("fibery/meta", {})

            # Skip deleted types
            if t.get("fibery/deleted?"):
                continue
            # Skip primitive types
            if meta.get("fibery/primitive?"):
                continue
            # Skip schema-only / mixins
            if meta.get("fibery/schema-only?") or meta.get("app/mixin?"):
                continue
            # Skip internal meta types (schema introspection tables)
            if meta.get("fibery/meta?"):
                continue

            queryable.append(name)
        return sorted(queryable)

    def build_select_clause(self, db_name: str) -> Tuple[List[Any], List[Dict[str, Any]]]:
        """
        Build the q/select list for a database.
        Returns (select_clause, field_metadata_list).
        """
        db_type = self.types_by_name.get(db_name)
        if not db_type:
            return (["fibery/id"], [])

        select_clause: List[Any] = []
        field_metas: List[Dict[str, Any]] = []
        seen_fields: Set[str] = set()

        # Always start with fibery/id
        select_clause.append("fibery/id")
        seen_fields.add("fibery/id")

        for f in db_type.get("fibery/fields", []):
            fname = f["fibery/name"]
            ftype = f["fibery/type"]
            fmeta = f.get("fibery/meta", {})

            if fname in seen_fields:
                continue

            # Skip deleted fields
            if f.get("fibery/deleted?"):
                continue

            target_type = self.types_by_name.get(ftype)

            # Skip fields pointing to schema-only types (like fibery/view or mixins)
            if target_type and target_type.get("fibery/meta", {}).get("fibery/schema-only?"):
                continue
            if fmeta.get("fibery/schema-only?"):
                continue

            is_primitive = target_type.get("fibery/meta", {}).get("fibery/primitive?", False) if target_type else False
            is_collection = fmeta.get("fibery/collection?", False)

            field_info = {
                "name": fname,
                "type": ftype,
                "is_primitive": is_primitive,
                "is_collection": is_collection,
                "is_doc": (ftype == "Collaboration~Documents/Document"),
                "readonly": fmeta.get("fibery/readonly?", False),
                "required": fmeta.get("fibery/required?", False),
            }
            field_metas.append(field_info)
            seen_fields.add(fname)

            if is_primitive:
                select_clause.append(fname)
            elif ftype == "Collaboration~Documents/Document":
                # Rich text document: select secret and id
                select_clause.append({fname: ["Collaboration~Documents/secret", "fibery/id"]})
            elif is_collection:
                # Collection (one-to-many or many-to-many): select IDs with safe limit
                select_clause.append({fname: {"q/select": ["fibery/id"], "q/limit": self.collection_limit}})
            else:
                # To-one reference (foreign key or single select enum): select id
                select_clause.append({fname: ["fibery/id"]})

        return select_clause, field_metas


class ProgressTracker:
    """Persists scraping progress and state across runs."""

    def __init__(self, progress_file: str):
        self.progress_file = progress_file
        self.data: Dict[str, Any] = {
            "completed_databases": {},
            "counts": {},
            "total_entities_scraped": 0,
            "started_at": datetime.now(timezone.utc).isoformat(),
            "last_updated": datetime.now(timezone.utc).isoformat(),
        }
        self.load()

    def load(self):
        if os.path.exists(self.progress_file):
            try:
                with open(self.progress_file, "r", encoding="utf-8") as f:
                    self.data = json.load(f)
            except Exception as e:
                print(f"[WARN] Failed to load {self.progress_file}: {e}. Starting fresh progress.", file=sys.stderr)

    def save(self):
        self.data["last_updated"] = datetime.now(timezone.utc).isoformat()
        tmp_file = f"{self.progress_file}.tmp"
        with open(tmp_file, "w", encoding="utf-8") as f:
            json.dump(self.data, f, indent=2)
        os.replace(tmp_file, self.progress_file)

    def is_completed(self, db_name: str) -> bool:
        return db_name in self.data.get("completed_databases", {})

    def mark_completed(self, db_name: str, count: int, file_path: str, synced_at: datetime):
        """synced_at: a time before the data was read; the next --update fetches changes since then."""
        if "completed_databases" not in self.data:
            self.data["completed_databases"] = {}
        self.data["completed_databases"][db_name] = {
            "entity_count": count,
            "file": file_path,
            "completed_at": datetime.now(timezone.utc).isoformat(),
            "synced_at": synced_at.isoformat(),
        }
        self.data["total_entities_scraped"] = sum(
            item.get("entity_count", 0) for item in self.data["completed_databases"].values()
        )
        self.save()


def load_auth_config(config_path: str) -> Tuple[str, str]:
    """Load API key and workspace from config file or environment."""
    expanded_path = os.path.expanduser(config_path)
    api_key = os.environ.get("FIBERY_API_KEY")
    workspace = os.environ.get("FIBERY_WORKSPACE")

    if os.path.exists(expanded_path):
        try:
            with open(expanded_path, "r", encoding="utf-8") as f:
                cfg = json.load(f)
                fibery_cfg = cfg.get("fibery", {})
                if not api_key:
                    api_key = fibery_cfg.get("api_key")
                if not workspace:
                    workspace = fibery_cfg.get("workspace", "neara")
        except Exception as e:
            print(f"[WARN] Failed to parse {config_path}: {e}", file=sys.stderr)

    if not workspace:
        workspace = "neara"

    if not api_key:
        raise ValueError(
            f"No Fibery API key found! Please specify it in {config_path} under 'fibery.api_key' "
            "or via the FIBERY_API_KEY environment variable."
        )

    return workspace, api_key


def effective_page_size_for(page_size: int, select_fields: List[Any]) -> int:
    size = min(page_size, 2500)
    return min(size, 1000) if len(select_fields) > 50 else size


def delta_field(analyzer: "SchemaAnalyzer", db_name: str) -> Optional[str]:
    """The date field that --update filters on. Types with only a creation date
    (comments, references, files) are treated as append-only."""
    names = {f["fibery/name"] for f in analyzer.types_by_name[db_name].get("fibery/fields", [])}
    for f in ("fibery/modification-date", "Collaboration~Documents/modification-date", "fibery/creation-date"):
        if f in names:
            return f
    return None


def fetch_all(
    client: FiberyClient,
    db_name: str,
    select_fields: List[Any],
    page_size: int,
    since_field: Optional[str] = None,
    since: Optional[str] = None,
) -> List[Dict[str, Any]]:
    """Page through every matching entity into memory. Meant for deltas and id sweeps, not full scrapes."""
    size = effective_page_size_for(page_size, select_fields)
    out: List[Dict[str, Any]] = []
    last_seen_id: Optional[str] = None
    while True:
        page = client.query_page(db_name, select_fields, size, last_seen_id, since_field, since)
        out.extend(page[:size])
        if len(page) <= size:
            return out
        last_seen_id = page[size - 1]["fibery/id"]
        if since_field is not None:
            since = page[size - 1][since_field]


def upsert_jsonl(path: str, key: str, updates: Dict[str, Any], live_keys: Optional[Set[str]] = None) -> int:
    """Rewrite a JSONL file with `updates` (key -> record) merged in, dropping rows
    whose key is not in `live_keys` when given. Returns the resulting row count."""
    if not updates and live_keys is None and os.path.exists(path):
        with open(path, "rb") as f:
            return sum(1 for line in f if line.strip())
    pending = dict(updates)
    n = 0
    tmp = f"{path}.tmp"
    with open(tmp, "w", encoding="utf-8") as out:
        if os.path.exists(path):
            with open(path, "r", encoding="utf-8") as f:
                for line in f:
                    if not line.strip():
                        continue
                    k = json.loads(line).get(key)
                    if live_keys is not None and k not in live_keys:
                        continue
                    rec = pending.pop(k, None)
                    out.write(line if rec is None else json.dumps(rec, ensure_ascii=False) + "\n")
                    n += 1
        for rec in pending.values():
            out.write(json.dumps(rec, ensure_ascii=False) + "\n")
            n += 1
    os.replace(tmp, path)
    return n


def refresh_documents(client: FiberyClient, secrets: List[str], target_dir: str, batch_size: int = 200) -> int:
    """Re-fetch rich-text bodies and merge them into documents.jsonl and documents/*.md."""
    fetched: Dict[str, Any] = {}
    for i in range(0, len(secrets), batch_size):
        for doc in client.batch_get_documents(secrets[i:i + batch_size], format="md"):
            if doc.get("secret"):
                fetched[doc["secret"]] = doc
    documents_dir = os.path.join(target_dir, "documents")
    os.makedirs(documents_dir, exist_ok=True)
    for sec, doc in fetched.items():
        with open(os.path.join(documents_dir, f"{sec}.md"), "w", encoding="utf-8") as f:
            f.write(doc.get("content", ""))
    upsert_jsonl(os.path.join(target_dir, "documents.jsonl"), "secret", fetched)
    return len(fetched)


def update_database(
    client: FiberyClient,
    db_name: str,
    select_fields: List[Any],
    output_path: str,
    since_field: str,
    since: str,
    server_count: Optional[int],
    page_size: int,
    target_dir: str,
) -> int:
    """Merge entities changed since `since` into an existing JSONL file; returns the new row count."""
    changed = fetch_all(client, db_name, select_fields, page_size, since_field, since)
    n = upsert_jsonl(output_path, "fibery/id", {r["fibery/id"]: r for r in changed})

    # A date filter never returns deleted entities. Every create/edit is already
    # merged, so local > server means something was deleted: sweep ids to find it.
    # chisle: Document rows are skipped. They orphan harmlessly when their owner
    # goes, and sweeping 2M+ ids takes ~15 min at 1 req/s.
    deleted = 0
    if server_count is not None and n > server_count and db_name != DOC_DB:
        live = {r["fibery/id"] for r in fetch_all(client, db_name, ["fibery/id"], 2500)}
        before = n
        n = upsert_jsonl(output_path, "fibery/id", {}, live)
        deleted = before - n

    docs = 0
    if db_name == DOC_DB and os.path.exists(os.path.join(target_dir, "documents.jsonl")):
        secrets = sorted({r["Collaboration~Documents/secret"] for r in changed if r.get("Collaboration~Documents/secret")})
        docs = refresh_documents(client, secrets, target_dir)

    print(f"    ✓ {len(changed):,} changed, {deleted:,} deleted, {docs:,} bodies refreshed -> {n:,} total")
    return n


def scrape_database(
    client: FiberyClient,
    db_name: str,
    select_fields: List[Any],
    expected_count: int,
    output_path: str,
    page_size: int = 1000,
    limit: Optional[int] = None,
    verbose: bool = False,
) -> int:
    """Scrape all entities for a database and write to JSONL file."""
    tmp_path = f"{output_path}.tmp"
    total_fetched = 0
    last_seen_id: Optional[str] = None
    t0 = time.time()

    effective_page_size = effective_page_size_for(page_size, select_fields)
    if limit is not None:
        effective_page_size = min(effective_page_size, limit)

    with open(tmp_path, "w", encoding="utf-8") as f:
        while True:
            if limit is not None and total_fetched >= limit:
                break

            fetch_limit = effective_page_size
            if limit is not None:
                fetch_limit = min(effective_page_size, limit - total_fetched)

            page = client.query_page(
                db_name=db_name,
                select_fields=select_fields,
                page_size=fetch_limit,
                last_seen_id=last_seen_id,
            )

            if not page:
                break

            has_more = len(page) > fetch_limit
            items_to_save = page[:fetch_limit] if has_more else page

            for item in items_to_save:
                f.write(json.dumps(item, ensure_ascii=False) + "\n")

            total_fetched += len(items_to_save)

            if items_to_save:
                last_seen_id = items_to_save[-1].get("fibery/id")

            if verbose or expected_count > 1000:
                pct = (total_fetched / expected_count * 100.0) if expected_count > 0 else 100.0
                rate = total_fetched / max(time.time() - t0, 0.001)
                print(f"    [{db_name}] {total_fetched:,}/{expected_count:,} ({pct:.1f}%) entities | {rate:.1f} ent/s", end="\r", flush=True)

            if not has_more or not last_seen_id:
                break

    if verbose or expected_count > 1000:
        print()

    os.replace(tmp_path, output_path)
    return total_fetched


def fetch_all_documents(
    client: FiberyClient,
    secrets: Set[str],
    documents_dir: str,
    batch_size: int = 50,
    verbose: bool = True
):
    """Batch fetch markdown content for all collected rich-text secrets."""
    secret_list = sorted(list(secrets))
    total = len(secret_list)
    print(f"\n[INFO] Fetching content for {total:,} rich-text documents in batches of {batch_size}...")

    fetched = 0
    t0 = time.time()
    for i in range(0, total, batch_size):
        chunk = secret_list[i:i + batch_size]
        try:
            docs = client.batch_get_documents(chunk, format="md")
            for doc in docs:
                sec = doc.get("secret")
                content = doc.get("content", "")
                if sec:
                    doc_path = os.path.join(documents_dir, f"{sec}.md")
                    with open(doc_path, "w", encoding="utf-8") as f:
                        f.write(content)
                    fetched += 1
        except Exception as e:
            print(f"  [WARN] Failed to fetch documents batch {i}-{i+len(chunk)}: {e}", file=sys.stderr)

        if verbose:
            pct = (fetched / total * 100.0) if total > 0 else 100.0
            rate = fetched / max(time.time() - t0, 0.001)
            print(f"    Documents: {fetched:,}/{total:,} ({pct:.1f}%) | {rate:.1f} doc/s", end="\r", flush=True)

    print(f"\n[INFO] Finished downloading {fetched:,} documents to {documents_dir}")


def main():
    parser = argparse.ArgumentParser(
        description="Scrape all data from Fibery REST API with rate limiting and checkpointing for Rust reimplementation."
    )
    parser.add_argument(
        "--target-dir",
        default="~/src/neara/fibery",
        help="Directory to save scraped schema and data (default: ~/src/neara/fibery)",
    )
    parser.add_argument(
        "--config",
        default="~/.phab-helper.json",
        help="Path to phab-helper config containing API key and workspace (default: ~/.phab-helper.json)",
    )
    parser.add_argument(
        "--workspace",
        help="Override Fibery workspace (e.g. 'neara')",
    )
    parser.add_argument(
        "--api-key",
        help="Override Fibery API key",
    )
    parser.add_argument(
        "--rate-limit",
        type=float,
        default=1.0,
        help="Maximum requests per second (default: 1.0, strictly enforced)",
    )
    parser.add_argument(
        "--page-size",
        type=int,
        default=1000,
        help="Default pagination page size (default: 1000, up to 2500 for lean tables)",
    )
    parser.add_argument(
        "--databases",
        nargs="+",
        help="Optional: Scrape only specific database names (e.g. --databases 'Engineering/Issue' 'Engineering/Scope')",
    )
    parser.add_argument(
        "--limit",
        type=int,
        help="Optional: Limit max entities per database (useful for testing on small subset)",
    )
    parser.add_argument(
        "--force",
        action="store_true",
        help="Force re-scraping of already completed databases",
    )
    parser.add_argument(
        "--update",
        action="store_true",
        help="Incremental: fetch only entities changed since the last sync, drop deleted ones, "
             "refresh changed rich-text bodies in documents.jsonl. New or re-shaped databases get a full scrape.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Inspect schema and print entity counts without downloading data",
    )
    parser.add_argument(
        "--fetch-documents",
        action="store_true",
        help="Also fetch full Markdown contents for all rich-text documents",
    )
    parser.add_argument(
        "--verbose", "-v",
        action="store_true",
        help="Verbose logging",
    )

    args = parser.parse_args()

    workspace, api_key = load_auth_config(args.config)
    if args.workspace:
        workspace = args.workspace
    if args.api_key:
        api_key = args.api_key

    target_dir = os.path.abspath(os.path.expanduser(args.target_dir))
    databases_dir = os.path.join(target_dir, "databases")
    documents_dir = os.path.join(target_dir, "documents")
    os.makedirs(databases_dir, exist_ok=True)
    if args.fetch_documents:
        os.makedirs(documents_dir, exist_ok=True)

    print("=" * 70)
    print("FIBERY DATA SCRAPER")
    print("=" * 70)
    print(f"Workspace     : {workspace}")
    print(f"Target Dir    : {target_dir}")
    print(f"Rate Limit    : {args.rate_limit:.2f} req/sec (min {1.0/args.rate_limit:.2f}s between requests)")
    print(f"Page Size     : {args.page_size}")
    if args.databases:
        print(f"DB Filter     : {len(args.databases)} databases specified ({', '.join(args.databases[:3])}...)")
    if args.limit:
        print(f"Entity Limit  : {args.limit} entities max per database (TEST MODE)")
    print("=" * 70)

    rate_limiter = RateLimiter(requests_per_second=args.rate_limit)
    client = FiberyClient(workspace=workspace, api_key=api_key, rate_limiter=rate_limiter, verbose=args.verbose)

    print("\n[Step 1/4] Fetching complete Fibery schema...")
    raw_schema = client.get_schema(with_description=True)
    types_count = len(raw_schema.get("fibery/types", []))
    print(f"  ✓ Schema fetched: {types_count} total types/mixins/primitives in workspace")

    schema_file = os.path.join(target_dir, "schema.json")
    with open(schema_file, "w", encoding="utf-8") as f:
        json.dump(raw_schema, f, indent=2)
    print(f"  ✓ Saved raw schema to: {schema_file}")

    analyzer = SchemaAnalyzer(raw_schema)
    all_queryable_dbs = analyzer.get_queryable_databases()
    print(f"  ✓ Identified {len(all_queryable_dbs)} queryable entity databases")

    target_dbs = all_queryable_dbs
    if args.databases:
        target_set = set(args.databases)
        target_dbs = [db for db in all_queryable_dbs if db in target_set]
        missing = target_set - set(target_dbs)
        if missing:
            print(f"  [WARN] {len(missing)} requested databases not found in schema: {missing}", file=sys.stderr)

    print(f"\n[Step 2/4] Counting entities across {len(target_dbs)} databases (batched)...")
    db_counts = client.batch_count_entities(target_dbs, batch_size=50)

    non_empty_dbs = {name: cnt for name, cnt in db_counts.items() if cnt > 0}
    total_entities_in_workspace = sum(db_counts.values())
    print(f"  ✓ Total entities across selected databases: {total_entities_in_workspace:,}")
    print(f"  ✓ Non-empty databases: {len(non_empty_dbs)} / {len(target_dbs)}")

    counts_file = os.path.join(target_dir, "type_counts.json")
    with open(counts_file, "w", encoding="utf-8") as f:
        json.dump(db_counts, f, indent=2)

    if args.dry_run:
        print("\n[DRY RUN COMPLETE] Top 30 largest databases:")
        for name, cnt in sorted(db_counts.items(), key=lambda x: x[1], reverse=True)[:30]:
            print(f"  {name:50s} : {cnt:>9,}")
        return

    progress_file = os.path.join(target_dir, "progress.json")
    progress = ProgressTracker(progress_file)
    progress.data["counts"] = db_counts

    # Field lists from the previous run: a database whose fields changed needs a
    # full re-scrape, since a delta would leave old rows missing the new fields.
    previous_fields: Dict[str, List[str]] = {}
    previous_manifest: Dict[str, Any] = {}
    manifest_file = os.path.join(target_dir, "manifest.json")
    if os.path.exists(manifest_file):
        with open(manifest_file, "r", encoding="utf-8") as f:
            previous_manifest = json.load(f).get("databases", {})
        for name, entry in previous_manifest.items():
            previous_fields[name] = [fm["name"] for fm in entry.get("fields", [])]

    sorted_dbs = sorted(target_dbs, key=lambda name: db_counts.get(name, 0))

    print(f"\n[Step 3/4] Scraping entity records to JSONL files...")
    all_rich_text_secrets: Set[str] = set()
    # Keep entries for databases outside a --databases subset: ingest reads the
    # manifest to find every file.
    manifest_entries: Dict[str, Any] = {
        name: entry for name, entry in previous_manifest.items() if name in all_queryable_dbs
    }

    start_time = time.time()
    total_dbs = len(sorted_dbs)

    for idx, db_name in enumerate(sorted_dbs, 1):
        count = db_counts.get(db_name, 0)
        safe_name = sanitize_filename(db_name)
        file_name = f"{safe_name}.jsonl"
        rel_file_path = os.path.join("databases", file_name)
        abs_file_path = os.path.join(databases_dir, file_name)

        select_fields, field_metas = analyzer.build_select_clause(db_name)
        manifest_entries[db_name] = {
            "file": rel_file_path,
            "entity_count": count,
            "field_count": len(field_metas),
            "fields": field_metas,
        }

        synced_at = datetime.now(timezone.utc)
        completed_info = progress.data.get("completed_databases", {}).get(db_name)
        since_field = delta_field(analyzer, db_name)
        fields_now = [fm["name"] for fm in field_metas]
        if (
            args.update and not args.force and completed_info is not None and since_field is not None
            and os.path.exists(abs_file_path) and previous_fields.get(db_name) == fields_now
        ):
            # Scrapes from before --update existed have no synced_at; the run's
            # start time predates every one of their reads.
            last = datetime.fromisoformat(completed_info.get("synced_at") or progress.data["started_at"])
            since = (last - WATERMARK_MARGIN).strftime("%Y-%m-%dT%H:%M:%S.000Z")
            print(f"  [{idx}/{total_dbs}] Updating '{db_name}' ({since_field} >= {since})...")
            try:
                n = update_database(
                    client, db_name, select_fields, abs_file_path, since_field, since,
                    db_counts.get(db_name), args.page_size, target_dir,
                )
            except Exception as e:
                print(f"    ✗ ERROR updating '{db_name}': {e}", file=sys.stderr)
                manifest_entries[db_name]["scraped_count"] = completed_info.get("entity_count", count)
                continue
            progress.mark_completed(db_name, n, rel_file_path, synced_at)
            manifest_entries[db_name]["scraped_count"] = n
            continue

        if not args.force and not args.update and completed_info is not None:
            manifest_entries[db_name]["scraped_count"] = completed_info.get("entity_count", count)
            if args.verbose:
                print(f"  [{idx}/{total_dbs}] SKIPPING {db_name} (already completed: {completed_info.get('entity_count')} entities)")
            continue

        if count == 0 and db_name in db_counts:
            with open(abs_file_path, "w", encoding="utf-8") as f:
                pass
            progress.mark_completed(db_name, 0, rel_file_path, synced_at)
            manifest_entries[db_name]["scraped_count"] = 0
            continue

        print(f"  [{idx}/{total_dbs}] Scraping '{db_name}' ({count:,} entities, {len(select_fields)} fields)...")

        try:
            fetched_count = scrape_database(
                client=client,
                db_name=db_name,
                select_fields=select_fields,
                expected_count=count,
                output_path=abs_file_path,
                page_size=args.page_size,
                limit=args.limit,
                verbose=args.verbose,
            )

            progress.mark_completed(db_name, fetched_count, rel_file_path, synced_at)
            manifest_entries[db_name]["scraped_count"] = fetched_count
            print(f"    ✓ Saved {fetched_count:,} entities -> {rel_file_path}")

        except Exception as e:
            print(f"    ✗ ERROR scraping '{db_name}': {e}", file=sys.stderr)
            continue

    print("\n[Step 4/4] Writing database manifest (manifest.json)...")
    manifest = {
        "workspace": workspace,
        "scraped_at": datetime.now(timezone.utc).isoformat(),
        "total_databases": len(manifest_entries),
        "total_entities_scraped": progress.data.get("total_entities_scraped", 0),
        "databases": manifest_entries,
    }
    with open(manifest_file, "w", encoding="utf-8") as f:
        json.dump(manifest, f, indent=2)
    print(f"  ✓ Manifest saved to: {manifest_file}")

    if args.fetch_documents and not args.update:
        print("\n[Optional] Extracting rich-text document secrets from scraped JSONL files...")
        doc_fields_by_db = {}
        for db_name, entry in manifest_entries.items():
            doc_fields = [f["name"] for f in entry.get("fields", []) if f.get("is_doc")]
            if doc_fields:
                doc_fields_by_db[db_name] = doc_fields

        for db_name, doc_fields in doc_fields_by_db.items():
            rel_file = manifest_entries[db_name]["file"]
            abs_file = os.path.join(target_dir, rel_file)
            if not os.path.exists(abs_file):
                continue
            with open(abs_file, "r", encoding="utf-8") as f:
                for line in f:
                    try:
                        record = json.loads(line)
                        for df in doc_fields:
                            val = record.get(df)
                            if isinstance(val, dict):
                                sec = val.get("Collaboration~Documents/secret")
                                if sec:
                                    all_rich_text_secrets.add(sec)
                    except Exception:
                        pass

        print(f"  ✓ Found {len(all_rich_text_secrets):,} unique rich-text document secrets")
        if all_rich_text_secrets:
            fetch_all_documents(client, all_rich_text_secrets, documents_dir, verbose=args.verbose)

    elapsed = time.time() - start_time
    total_reqs = client.request_count
    print("\n" + "=" * 70)
    print("SCRAPE COMPLETE!")
    print("=" * 70)
    print(f"Total Entities Scraped : {progress.data.get('total_entities_scraped', 0):,}")
    print(f"Total HTTP Requests    : {total_reqs:,}")
    print(f"Total Elapsed Time     : {elapsed / 60:.1f} minutes ({elapsed:.1f} seconds)")
    print(f"Data Directory         : {databases_dir}")
    print(f"Schema File            : {schema_file}")
    print(f"Manifest File          : {manifest_file}")
    print("=" * 70)


if __name__ == "__main__":
    main()
