import { useEffect, useRef, useState } from "react";
import { useNavigate, useSearchParams } from "react-router-dom";
import { dbFilter, getSyntax, hitCache, prefetchEntity, search, SearchResp } from "./api";
import { HitRow } from "./HitRow";

let lastKey = 0;
export const markKeystroke = () => { lastKey = performance.now(); };
const DEBOUNCE_MS = 90;

export function useSearch(q: string, debounce = DEBOUNCE_MS) {
  const [res, setRes] = useState<SearchResp | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [latency, setLatency] = useState<number | null>(null);
  useEffect(() => {
    if (!q.trim()) { setRes(null); setErr(null); return; }
    const ctl = new AbortController();
    const t = setTimeout(() => {
      search(q, ctl.signal).then(
        (r) => {
          r.hits.forEach((h) => hitCache.set(h.id, h));
          setRes(r);
          setErr(null);
          setLatency(lastKey ? performance.now() - lastKey : null);
          lastKey = 0;
        },
        (e) => { if (e.name !== "AbortError") setErr(String(e.message ?? e)); },
      );
    }, debounce);
    return () => { clearTimeout(t); ctl.abort(); };
  }, [q, debounce]);
  return { res, err, latency };
}

export default function SearchPage() {
  const [params] = useSearchParams();
  const q = params.get("q") ?? "";
  const { res, err, latency } = useSearch(q);
  const [sel, setSel] = useState(0);
  const [showSyntax, setShowSyntax] = useState(false);
  const nav = useNavigate();
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => setSel(0), [res]);
  const hits = res?.hits ?? [];
  useEffect(() => {
    const h = hits[sel];
    if (!h) return;
    prefetchEntity(h.id);
    listRef.current?.querySelector(`[data-i="${sel}"]`)?.scrollIntoView({ block: "nearest" });
  }, [sel, hits]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.target instanceof HTMLTextAreaElement) return;
      if (e.key === "ArrowDown") { e.preventDefault(); setSel((s) => Math.min(s + 1, hits.length - 1)); }
      else if (e.key === "ArrowUp") { e.preventDefault(); setSel((s) => Math.max(s - 1, 0)); }
      else if (e.key === "Enter" && hits[sel]) {
        e.preventDefault();
        (document.activeElement as HTMLElement | null)?.blur();
        nav(`/e/${hits[sel].id}`);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [hits, sel, nav]);

  const addFacet = (db: string) => nav(`/search?q=${encodeURIComponent(`${q.trim()} ${dbFilter(db)}`.trim())}`);

  return (
    <div className="search-page">
      <div className="results">
        <div className="results-meta">
          {res && <span><b>{res.total.toLocaleString()}</b> results</span>}
          {res && <span className="muted">server {res.took_ms.toFixed(1)} ms</span>}
          {latency != null && <span className="muted" title="keystroke → results rendered, incl. debounce">key→render {latency.toFixed(0)} ms</span>}
          <span className="spacer" />
          <button className="link-btn" onClick={() => setShowSyntax((s) => !s)}>{showSyntax ? "Hide syntax" : "Syntax ?"}</button>
        </div>
        {err && <div className="error">{err}</div>}
        {res?.warnings.map((w) => <div key={w} className="warning">{w}</div>)}
        {!q.trim() && <div className="empty">Type to search. Try <code>#note rvo</code>, <code>@sami is:open</code>, <code>updated:-7d</code>.</div>}
        <div ref={listRef} className="hitlist">
          {hits.map((h, i) => (
            <HitRow key={h.id} hit={h} selected={i === sel} i={i} onHover={() => setSel(i)} />
          ))}
        </div>
        <div className="keyhints"><kbd>↑</kbd><kbd>↓</kbd> navigate <kbd>↵</kbd> open <kbd>/</kbd> <kbd>⌘K</kbd> focus search</div>
      </div>
      <aside className="facets">
        {showSyntax ? <Syntax /> : res && res.by_db.length > 0 && (
          <>
            <div className="facet-title">Databases</div>
            {res.by_db.map(([db, n]) => (
              <div key={db} className="facet" onClick={() => addFacet(db)} title={`add ${dbFilter(db)}`}>
                <span className="facet-name">{db}</span>
                <span className="facet-count">{n.toLocaleString()}</span>
              </div>
            ))}
          </>
        )}
      </aside>
    </div>
  );
}

function Syntax() {
  const [t, setT] = useState<string>("");
  useEffect(() => { getSyntax().then(setT, (e) => setT(String(e))); }, []);
  return <pre className="syntax">{t}</pre>;
}
