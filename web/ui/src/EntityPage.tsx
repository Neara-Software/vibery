import { lazy, Suspense, useEffect, useState } from "react";
import { Link, useNavigate, useParams } from "react-router-dom";
import { Entity, fmtDate, getEntity, hitCache, invalidateEntity, Person, prefetchEntity, resolve, shortDb } from "./api";
import { DbBadge, DbIcon } from "./HitRow";
import { hue } from "./Sidebar";

const Editor = lazy(() => import("./Editor"));
const LazyMarkdown = lazy(() => import("./Markdown"));

function Markdown({ text }: { text: string }) {
  return (
    <Suspense fallback={<div className="md" style={{ whiteSpace: "pre-wrap" }}>{text}</div>}>
      <LazyMarkdown text={text} />
    </Suspense>
  );
}

export function FiberyUrlPage() {
  const { space = "", db = "", slug = "" } = useParams();
  const nav = useNavigate();
  const [err, setErr] = useState<string | null>(null);
  useEffect(() => {
    const pid = slug.split("-").pop() ?? "";
    const name = `${decodeURIComponent(space)}/${decodeURIComponent(db)}`;
    resolve(name, pid)
      .catch(() => resolve(name.replace(/_/g, " "), pid))
      .then((r) => nav(`/e/${r.id}`, { replace: true }), (e) => setErr(String(e.message ?? e)));
  }, [space, db, slug, nav]);
  return err ? <div className="error">Could not resolve: {err}</div> : null;
}

export default function EntityPage() {
  const { id = "" } = useParams();
  const [ent, setEnt] = useState<Entity | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [rev, setRev] = useState(0);
  useEffect(() => {
    let live = true;
    setErr(null);
    getEntity(id).then((e) => live && setEnt(e), (e) => live && setErr(String(e.message ?? e)));
    return () => { live = false; };
  }, [id, rev]);

  const cur = ent?.id === id ? ent : null;
  const hit = hitCache.get(id);
  const head = cur ?? (hit && { ...hit, created_by: null });
  useEffect(() => { if (head) document.title = head.title || "vibery"; }, [head?.title]);

  if (err) return <div className="error">{err}</div>;
  if (!head) return <div className="entity" />;
  return (
    <div className="entity">
      <div className="entity-main">
        <div className="badges">
          <DbBadge db={head.db} />
          {head.public_id && <span className="badge pid">#{head.public_id}</span>}
          {head.state && <span className="badge state">{head.state}</span>}
          {cur?.url && <a className="badge ext" href={`https://neara.fibery.io${cur.url}`} target="_blank" rel="noreferrer">open in Fibery ↗</a>}
        </div>
        <h1 className="entity-title">{head.title || "Untitled"}</h1>
        {cur ? <FieldRow e={cur} /> : <div className="fieldrow" />}
        {cur?.docs.map((d) => (
          <DocView key={d.secret} doc={d} onSaved={() => { invalidateEntity(id); setRev((r) => r + 1); }} />
        ))}
        {cur?.relations.map((r) => <RelationBox key={r.field} r={r} />)}
        {cur && cur.comments.length > 0 && (
          <section className="comments">
            <div className="section-label">Comments {cur.comments.length}</div>
            {cur.comments.map((c) => (
              <div key={c.id} className="comment">
                <div className="comment-head">
                  <Avatar p={{ id: c.author, name: c.author }} /> <b>{c.author}</b>
                  <span className="muted"> · {fmtDate(c.created)}</span>
                </div>
                <CommentBody c={c} />
              </div>
            ))}
          </section>
        )}
        {cur && cur.backlinks.length > 0 && (
          <section className="backlinks">
            <div className="section-label">Referenced {cur.backlinks.length} time{cur.backlinks.length === 1 ? "" : "s"}</div>
            <div className="box">{cur.backlinks.map((b) => (
              <Link key={b.field + b.id} to={`/e/${b.id}`} className="backlink" onMouseEnter={() => prefetchEntity(b.id)}>
                <DbIcon db={b.db} /> <span>{b.title || "Untitled"}</span>
                <span className="muted small"> via {shortDb(b.field)}</span>
              </Link>
            ))}</div>
          </section>
        )}
      </div>
    </div>
  );
}

function FieldRow({ e }: { e: Entity }) {
  return (
    <div className="fieldrow">
      {e.created_by && <span className="fchip" title="Created by"><Avatar p={e.created_by} /> {e.created_by.name}</span>}
      <span className="fchip" title="Created">{fmtDate(e.created)}</span>
      {e.people.map((p) => p.items.filter((u) => u.id !== e.created_by?.id).map((u) => (
        <span key={p.field + u.id} className="fchip" title={shortDb(p.field)}><Avatar p={u} /> {u.name}</span>
      )))}
      {e.fields.filter((f) => f.value !== false && !f.name.includes("(ignore)")).map((f) => (
        <span key={f.name} className="fchip" title={f.name}><span className="fname">{shortDb(f.name)}</span> {fmtVal(f.value)}</span>
      ))}
    </div>
  );
}

const REL_LIMIT = 15;
function RelationBox({ r }: { r: Entity["relations"][number] }) {
  const [all, setAll] = useState(false);
  const items = all ? r.items : r.items.slice(0, REL_LIMIT);
  return (
    <section className="relation">
      <div className="section-label">{shortDb(r.field)} <span className="muted">{r.items.length}</span></div>
      <div className="box">
        {items.map((it) => (
          <Link key={it.id} to={`/e/${it.id}`} className="backlink" onMouseEnter={() => prefetchEntity(it.id)}>
            <DbIcon db={it.db} /> <span>{it.title || "Untitled"}</span>
          </Link>
        ))}
        {r.items.length > items.length && (
          <button className="link-btn" onClick={() => setAll(true)}>Show {r.items.length - items.length} more</button>
        )}
      </div>
    </section>
  );
}

function fmtVal(v: unknown): string {
  if (typeof v === "boolean") return v ? "Yes" : "No";
  if (typeof v === "string" || typeof v === "number") return String(v);
  return JSON.stringify(v);
}

export function Avatar({ p }: { p: Person }) {
  const initials = p.name.split(/\s+/).map((w) => w[0]).filter(Boolean).slice(0, 2).join("").toUpperCase();
  return <span className="avatar" style={{ background: hue(p.name) }} title={p.name}>{initials}</span>;
}

function DocView({ doc, onSaved }: {doc: Entity["docs"][number]; onSaved: () => void }) {
  const [editing, setEditing] = useState(false);
  return (
    <section className="doc">
      <div className="doc-head">
        <span className="section-label">{shortDb(doc.field)}</span>
        <span className="spacer" />
        {!editing && <button className="btn" onClick={() => setEditing(true)}>Edit</button>}
      </div>
      {editing ? (
        <Suspense fallback={<Markdown text={doc.markdown} />}>
          <Editor secret={doc.secret} initial={doc.markdown} onClose={(saved) => { setEditing(false); if (saved) onSaved(); }} />
        </Suspense>
      ) : (
        <Markdown text={doc.markdown} />
      )}
    </section>
  );
}

function CommentBody({ c }: { c: { secret?: string; markdown: string | null } }) {
  const [md, setMd] = useState(c.markdown);
  useEffect(() => {
    if (md || !c.secret) return;
    fetch(`/api/doc/${c.secret}`).then((r) => r.json()).then((j) => setMd(j.markdown ?? "")).catch(() => {});
  }, [c.secret]);
  return md == null ? <div className="muted small">Loading…</div> : <Markdown text={md} />;
}
