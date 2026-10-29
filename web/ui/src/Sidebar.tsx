import { useEffect, useState } from "react";
import { Link, useNavigate } from "react-router-dom";
import { Dbs, dbFilter, getDbs } from "./api";

const COLLAPSE_KEY = "vibery.collapsed";

export default function Sidebar() {
  const [dbs, setDbs] = useState<Dbs | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [collapsed, setCollapsed] = useState<Record<string, boolean>>(() => {
    try { return JSON.parse(localStorage.getItem(COLLAPSE_KEY) ?? "{}"); } catch { return {}; }
  });
  const nav = useNavigate();
  const isClosed = (s: string) => collapsed[s] ?? true;
  useEffect(() => { getDbs().then(setDbs, (e) => setErr(String(e))); }, []);
  const toggle = (s: string) => {
    const next = { ...collapsed, [s]: !isClosed(s) };
    setCollapsed(next);
    localStorage.setItem(COLLAPSE_KEY, JSON.stringify(next));
  };
  return (
    <nav className="sidebar">
      <Link to="/" className="ws">
        <span className="ws-logo">n</span>
        <span className="ws-name">neara</span>
      </Link>
      <div className="side-section">
        <Link to="/" className="side-item">Home</Link>
        <Link to="/search?q=sort%3Arecent" className="side-item">Recent</Link>
      </div>
      <div className="side-label">Spaces</div>
      {err && <div className="side-err">{err}</div>}
      <div className="side-tree">
        {dbs?.spaces.map((s) => (
          <div key={s.name}>
            <div className="side-space" onClick={() => toggle(s.name)}>
              <span className={"caret" + (isClosed(s.name) ? " closed" : "")}>▾</span>
              <span className="space-icon" style={{ background: hue(s.name) }}>{s.name[0]}</span>
              <span className="side-text">{s.name}</span>
            </div>
            {!isClosed(s.name) &&
              s.dbs.map((d) => (
                <div
                  key={d.name}
                  className="side-db"
                  title={d.name}
                  onClick={() => nav(`/search?q=${encodeURIComponent(`${dbFilter(d.name)} sort:recent`)}`)}
                >
                  <span className="side-text">{d.short}</span>
                  {d.count != null && <span className="side-count">{d.count}</span>}
                </div>
              ))}
          </div>
        ))}
      </div>
    </nav>
  );
}

const DB_COLOURS: Record<string, string> = {
  Note: "#f09520", Snippet: "#9a3fab", Scope: "#4aa564", Issue: "#cc3d33", Bet: "#5b44a8",
  Solution: "#7a2a8c", Event: "#d24a6e",
};

export function hue(s: string): string {
  const fixed = DB_COLOURS[s.split("/").pop() ?? ""];
  if (fixed) return fixed;
  let h = 0;
  for (const c of s) h = (h * 31 + c.charCodeAt(0)) % 360;
  return `hsl(${h} 55% 55%)`;
}
