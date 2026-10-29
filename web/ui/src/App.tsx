import { useEffect, useRef } from "react";
import { Route, Routes, useLocation, useNavigate, useSearchParams } from "react-router-dom";
import Sidebar from "./Sidebar";
import SearchPage, { markKeystroke } from "./SearchPage";
import EntityPage, { FiberyUrlPage } from "./EntityPage";
import Home from "./Home";

function SearchBar() {
  const [params] = useSearchParams();
  const loc = useLocation();
  const nav = useNavigate();
  const ref = useRef<HTMLInputElement>(null);
  const q = loc.pathname === "/search" ? params.get("q") ?? "" : "";

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const typing = e.target instanceof HTMLElement && /^(INPUT|TEXTAREA)$/.test(e.target.tagName);
      if ((e.key === "k" && (e.metaKey || e.ctrlKey)) || (e.key === "/" && !typing)) {
        e.preventDefault();
        ref.current?.focus();
        ref.current?.select();
      } else if (e.key === "Escape" && e.target === ref.current) {
        ref.current?.blur();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="topbar">
      <div className="searchbox">
        <svg width="14" height="14" viewBox="0 0 16 16" className="searchicon">
          <circle cx="7" cy="7" r="5" fill="none" stroke="currentColor" strokeWidth="1.6" />
          <path d="M11 11l3.5 3.5" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" />
        </svg>
        <input
          ref={ref}
          value={q}
          placeholder="Search  (/ or ⌘K)   e.g.  #note rvo sort:recent"
          spellCheck={false}
          onChange={(e) => {
            markKeystroke();
            const v = e.target.value;
            nav(`/search?q=${encodeURIComponent(v)}`, { replace: loc.pathname === "/search" });
          }}
        />
      </div>
    </div>
  );
}

export default function App() {
  return (
    <div className="app">
      <Sidebar />
      <div className="main">
        <SearchBar />
        <div className="content">
          <Routes>
            <Route path="/" element={<Home />} />
            <Route path="/search" element={<SearchPage />} />
            <Route path="/e/:id" element={<EntityPage />} />
            <Route path="/:space/:db/:slug" element={<FiberyUrlPage />} />
            <Route path="*" element={<div className="empty">Not found</div>} />
          </Routes>
        </div>
      </div>
    </div>
  );
}
