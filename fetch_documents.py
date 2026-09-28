#!/usr/bin/env python3
"""
Fibery Document Fetcher
=======================
Fetches the full formatted Markdown content for all rich-text documents in Fibery
via the batch API (`POST /api/documents/commands` with `command: "get-documents"`).

Features:
  - Strict rate-limiting (default: max 2.0 requests/second)
  - Efficient batching (fetches 200-250 documents per HTTP request)
  - Resilient retry with exponential backoff on HTTP 429/5xx
  - Checkpointing via `documents_progress.json` (skips already downloaded documents)
  - Dual storage:
      1. Individual markdown files in `documents/{secret}.md`
      2. Streaming single-file index in `documents.jsonl` (ideal for Rust prototypes)
  - `--wait-for-scrape` flag: wait until the background entity scrape finishes before starting
"""

import argparse
import concurrent.futures
import json
import os
import sys
import threading
import time
import urllib.error
import urllib.request
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Set


class TokenBucketRateLimiter:
    """Thread-safe token bucket rate limiter to strictly cap requests per second."""

    def __init__(self, requests_per_second: float = 2.0, burst: float = 2.0):
        self.capacity = burst
        self.tokens = burst
        self.fill_rate = requests_per_second
        self.last_update = time.time()
        self.lock = threading.Lock()

    def acquire(self):
        while True:
            with self.lock:
                now = time.time()
                elapsed = now - self.last_update
                self.tokens = min(self.capacity, self.tokens + elapsed * self.fill_rate)
                self.last_update = now

                if self.tokens >= 1.0:
                    self.tokens -= 1.0
                    return

                sleep_time = (1.0 - self.tokens) / self.fill_rate

            time.sleep(sleep_time)


class DocumentClient:
    """Client for Fibery Document API."""

    def __init__(self, workspace: str, api_key: str, rate_limiter: TokenBucketRateLimiter):
        self.workspace = workspace
        self.api_key = api_key
        self.rate_limiter = rate_limiter
        self.documents_url = f"https://{workspace}.fibery.io/api/documents/commands"
        self.req_lock = threading.Lock()
        self.request_count = 0

    def batch_get_documents(self, secrets: List[str], format: str = "md", max_retries: int = 5) -> List[Dict[str, Any]]:
        if not secrets:
            return []

        payload = {
            "command": "get-documents",
            "args": [{"secret": s, "format": format} for s in secrets],
        }
        data = json.dumps(payload).encode("utf-8")
        headers = {
            "Authorization": f"Token {self.api_key}",
            "Content-Type": "application/json",
            "User-Agent": "Fibery-Document-Fetcher/1.0",
        }

        for attempt in range(max_retries):
            self.rate_limiter.acquire()
            with self.req_lock:
                self.request_count += 1

            req = urllib.request.Request(self.documents_url, data=data, headers=headers, method="POST")
            try:
                with urllib.request.urlopen(req, timeout=120) as resp:
                    raw = resp.read().decode("utf-8")
                    res = json.loads(raw)
                    if isinstance(res, list):
                        return res
                    return []
            except urllib.error.HTTPError as e:
                status = e.code
                err_body = ""
                try:
                    err_body = e.read().decode("utf-8")
                except Exception:
                    pass

                if status in (429, 500, 502, 503, 504) and attempt < max_retries - 1:
                    backoff = (2 ** attempt) + 1.0
                    print(f"  [WARN] HTTP {status} ({err_body[:100]}). Retrying in {backoff:.1f}s (attempt {attempt + 1}/{max_retries})...", file=sys.stderr)
                    time.sleep(backoff)
                    continue
                else:
                    raise RuntimeError(f"HTTP {status} from {self.documents_url}: {err_body}") from e
            except (urllib.error.URLError, TimeoutError, ConnectionResetError) as e:
                if attempt < max_retries - 1:
                    backoff = (2 ** attempt) + 1.0
                    print(f"  [WARN] Network error ({e}). Retrying in {backoff:.1f}s (attempt {attempt + 1}/{max_retries})...", file=sys.stderr)
                    time.sleep(backoff)
                    continue
                else:
                    raise RuntimeError(f"Network error after {max_retries} attempts: {e}") from e

        return []


def load_auth_config(config_path: str) -> tuple[str, str]:
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
        raise ValueError("No Fibery API key found in config or FIBERY_API_KEY env var.")

    return workspace, api_key


def collect_document_secrets(databases_dir: str) -> Set[str]:
    """Collect all unique non-null document secrets from scraped JSONL files."""
    doc_table = os.path.join(databases_dir, "Collaboration~Documents__Document.jsonl")
    secrets: Set[str] = set()

    if os.path.exists(doc_table):
        print(f"[INFO] Scanning {doc_table} for document secrets...")
        with open(doc_table, "r", encoding="utf-8") as f:
            for line in f:
                try:
                    record = json.loads(line)
                    sec = record.get("Collaboration~Documents/secret")
                    if sec:
                        secrets.add(sec)
                except Exception:
                    pass
    else:
        # Fallback: scan all JSONL files in databases dir
        print(f"[INFO] Scanning all JSONL files in {databases_dir}...")
        for fn in os.listdir(databases_dir):
            if fn.endswith(".jsonl"):
                fp = os.path.join(databases_dir, fn)
                with open(fp, "r", encoding="utf-8") as f:
                    for line in f:
                        try:
                            record = json.loads(line)
                            for k, v in record.items():
                                if isinstance(v, dict):
                                    sec = v.get("Collaboration~Documents/secret")
                                    if sec:
                                        secrets.add(sec)
                        except Exception:
                            pass

    return secrets


def wait_for_original_scrape(target_dir: str, poll_interval: float = 10.0):
    """Wait until the original scrape finishes Collaboration~Documents__Document.jsonl."""
    doc_final = os.path.join(target_dir, "databases", "Collaboration~Documents__Document.jsonl")
    doc_tmp = os.path.join(target_dir, "databases", "Collaboration~Documents__Document.jsonl.tmp")

    if os.path.exists(doc_final):
        print(f"[INFO] Original scrape is already complete ({doc_final} exists).")
        return

    print("[INFO] Waiting for the original entity scrape to complete before starting document fetching...")
    while not os.path.exists(doc_final):
        if os.path.exists(doc_tmp):
            try:
                with open(doc_tmp, "rb") as f:
                    lines = sum(1 for _ in f)
                pct = (lines / 2078235.0) * 100.0
                print(f"  [Waiting] Original scrape progress: {lines:,}/2,078,235 ({pct:.1f}%)...")
            except Exception:
                pass
        time.sleep(poll_interval)

    print(f"\n[INFO] Original scrape has finished! Proceeding with document fetch...")


def main():
    parser = argparse.ArgumentParser(
        description="Fetch full Markdown content for all rich-text documents from Fibery."
    )
    parser.add_argument(
        "--target-dir",
        default="~/src/neara/fibery",
        help="Directory where databases/ is located and documents/ will be written",
    )
    parser.add_argument(
        "--config",
        default="~/.phab-helper.json",
        help="Path to phab-helper config containing API key and workspace",
    )
    parser.add_argument(
        "--rate-limit",
        type=float,
        default=2.0,
        help="Maximum requests per second (default: 2.0)",
    )
    parser.add_argument(
        "--batch-size",
        type=int,
        default=200,
        help="Number of documents to fetch per request (default: 200)",
    )
    parser.add_argument(
        "--workers",
        type=int,
        default=4,
        help="Concurrent request workers (default: 4, governed by rate limiter)",
    )
    parser.add_argument(
        "--wait-for-scrape",
        action="store_true",
        default=True,
        help="Wait for background entity scrape to finish before starting (default: True)",
    )
    parser.add_argument(
        "--limit",
        type=int,
        help="Limit total documents to fetch (for testing)",
    )
    parser.add_argument(
        "--verbose", "-v",
        action="store_true",
        help="Verbose logging",
    )

    args = parser.parse_args()

    workspace, api_key = load_auth_config(args.config)
    target_dir = os.path.abspath(os.path.expanduser(args.target_dir))
    databases_dir = os.path.join(target_dir, "databases")
    documents_dir = os.path.join(target_dir, "documents")
    documents_jsonl = os.path.join(target_dir, "documents.jsonl")
    progress_file = os.path.join(target_dir, "documents_progress.json")
    os.makedirs(documents_dir, exist_ok=True)

    print("=" * 70)
    print("FIBERY DOCUMENT FETCHER")
    print("=" * 70)
    print(f"Workspace     : {workspace}")
    print(f"Target Dir    : {target_dir}")
    print(f"Rate Limit    : {args.rate_limit:.2f} req/sec")
    print(f"Batch Size    : {args.batch_size} documents/request")
    print(f"Workers       : {args.workers}")
    print("=" * 70)

    if args.wait_for_scrape:
        wait_for_original_scrape(target_dir)

    all_secrets = collect_document_secrets(databases_dir)
    print(f"[INFO] Total unique document secrets discovered: {len(all_secrets):,}")

    if args.limit:
        all_secrets = set(sorted(list(all_secrets))[:args.limit])
        print(f"[INFO] Limited to {len(all_secrets):,} secrets for this run")

    # Load progress
    downloaded_secrets: Set[str] = set()
    if os.path.exists(progress_file):
        try:
            with open(progress_file, "r", encoding="utf-8") as f:
                data = json.load(f)
                downloaded_secrets = set(data.get("downloaded_secrets", []))
                print(f"[INFO] Loaded progress: {len(downloaded_secrets):,} documents already fetched")
        except Exception as e:
            print(f"[WARN] Failed to load {progress_file}: {e}", file=sys.stderr)

    remaining_secrets = sorted(list(all_secrets - downloaded_secrets))
    total_remaining = len(remaining_secrets)
    print(f"[INFO] Documents remaining to fetch: {total_remaining:,}")

    if total_remaining == 0:
        print("[INFO] All documents are already downloaded! Nothing to do.")
        return

    rate_limiter = TokenBucketRateLimiter(requests_per_second=args.rate_limit, burst=args.rate_limit)
    client = DocumentClient(workspace=workspace, api_key=api_key, rate_limiter=rate_limiter)

    # Chunk secrets into batches
    chunks = [
        remaining_secrets[i:i + args.batch_size]
        for i in range(0, total_remaining, args.batch_size)
    ]

    total_chunks = len(chunks)
    print(f"[INFO] Processing {total_chunks:,} batches of up to {args.batch_size} documents...")

    t0 = time.time()
    fetched_count = len(downloaded_secrets)
    target_total = len(all_secrets)
    file_lock = threading.Lock()
    progress_lock = threading.Lock()
    last_progress_save = time.time()

    jsonl_file = open(documents_jsonl, "a", encoding="utf-8")

    def process_chunk(chunk_idx: int, chunk: List[str]):
        nonlocal fetched_count, last_progress_save
        try:
            docs = client.batch_get_documents(chunk, format="md")
            saved_in_batch = []
            for doc in docs:
                sec = doc.get("secret")
                content = doc.get("content", "")
                if sec:
                    # 1. Write individual markdown file
                    doc_path = os.path.join(documents_dir, f"{sec}.md")
                    with open(doc_path, "w", encoding="utf-8") as f:
                        f.write(content)

                    # 2. Append to documents.jsonl
                    with file_lock:
                        jsonl_file.write(json.dumps(doc, ensure_ascii=False) + "\n")

                    saved_in_batch.append(sec)

            with progress_lock:
                downloaded_secrets.update(saved_in_batch)
                fetched_count += len(saved_in_batch)
                now = time.time()

                # Periodically save progress every 30 seconds
                if now - last_progress_save > 30.0:
                    last_progress_save = now
                    tmp_p = f"{progress_file}.tmp"
                    with open(tmp_p, "w", encoding="utf-8") as pf:
                        json.dump({
                            "downloaded_secrets": list(downloaded_secrets),
                            "total_downloaded": len(downloaded_secrets),
                            "last_updated": datetime.now(timezone.utc).isoformat(),
                        }, pf)
                    os.replace(tmp_p, progress_file)
                    jsonl_file.flush()

                elapsed = now - t0
                pct = (fetched_count / target_total) * 100.0 if target_total > 0 else 100.0
                rate = (fetched_count - (target_total - total_remaining)) / max(elapsed, 0.001)
                rem_docs = target_total - fetched_count
                eta_s = rem_docs / rate if rate > 0 else 0
                print(
                    f"  Documents: {fetched_count:,}/{target_total:,} ({pct:.1f}%) | "
                    f"{rate:.1f} doc/s | ETA: {eta_s / 60:.1f}m",
                    end="\r",
                    flush=True,
                )

        except Exception as e:
            print(f"\n  [ERROR] Batch {chunk_idx + 1}/{total_chunks} failed: {e}", file=sys.stderr)

    with concurrent.futures.ThreadPoolExecutor(max_workers=args.workers) as executor:
        futures = [executor.submit(process_chunk, idx, chunk) for idx, chunk in enumerate(chunks)]
        concurrent.futures.wait(futures)

    jsonl_file.close()

    # Final progress save
    with open(progress_file, "w", encoding="utf-8") as pf:
        json.dump({
            "downloaded_secrets": list(downloaded_secrets),
            "total_downloaded": len(downloaded_secrets),
            "completed_at": datetime.now(timezone.utc).isoformat(),
        }, pf, indent=2)

    total_time = time.time() - t0
    print("\n" + "=" * 70)
    print("DOCUMENT FETCH COMPLETE!")
    print("=" * 70)
    print(f"Total Documents Fetched : {len(downloaded_secrets):,}")
    print(f"Total HTTP Requests     : {client.request_count:,}")
    print(f"Total Elapsed Time      : {total_time / 60:.1f} minutes ({total_time:.1f}s)")
    print(f"Documents Folder        : {documents_dir}")
    print(f"Documents JSONL Index   : {documents_jsonl}")
    print("=" * 70)


if __name__ == "__main__":
    main()
