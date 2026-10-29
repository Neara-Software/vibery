import { memo } from "react";
import { Link } from "react-router-dom";
import { fmtDate, Hit, prefetchEntity, shortDb } from "./api";
import { hue } from "./Sidebar";

export const HitRow = memo(function HitRow({ hit, selected, i, onHover }: {
  hit: Hit; selected?: boolean; i?: number; onHover?: () => void;
}) {
  return (
    <Link
      to={`/e/${hit.id}`}
      className={"hit" + (selected ? " sel" : "")}
      data-i={i}
      onMouseEnter={() => { prefetchEntity(hit.id); onHover?.(); }}
    >
      <DbIcon db={hit.db} />
      <div className="hit-body">
        <div className="hit-top">
          <span className="hit-title">{hit.title || "Untitled"}</span>
          {hit.state && <span className="badge state">{hit.state}</span>}
          <span className="spacer" />
          <span className="hit-date">{fmtDate(hit.updated)}</span>
          {hit.public_id && <span className="pid">#{hit.public_id}</span>}
        </div>
        <div className="hit-path">{hit.db.split("/").join(" › ")}</div>
        {hit.snippet && <div className="hit-snippet" dangerouslySetInnerHTML={{ __html: snippetHtml(hit.snippet) }} />}
      </div>
    </Link>
  );
});

export function DbIcon({ db }: { db: string }) {
  return <span className="dbicon" style={{ background: hue(db) }} title={db}>{shortDb(db)[0]}</span>;
}

export function DbBadge({ db }: { db: string }) {
  return <span className="dbbadge" style={{ background: hue(db) }} title={db}>{shortDb(db)}</span>;
}

// Snippets may carry <b> highlight tags from tantivy; escape everything else.
function snippetHtml(raw: string): string {
  const s = raw
    .replace(/\[\/\/\]: # \([^)]*\)/g, "")
    .replace(/<(?!\/?b>)[^>]*>/g, "")
    .replace(/(^|\s)(#{1,6}|>|[-*] \[[ x]\]|\|)(?=\s)/g, "$1")
    .replace(/\|?\s*-{3,}\s*/g, " ")
    .replace(/\*\*|__|`/g, "")
    .replace(/!?\[([^\]]*)\]\([^)]*\)/g, "$1")
    .replace(/\s+/g, " ")
    .trim();
  const esc = s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
  return esc.replace(/&lt;(\/?)b&gt;/g, "<$1b>");
}
