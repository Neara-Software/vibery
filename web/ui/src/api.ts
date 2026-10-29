export type Hit = {
  id: string; db: string; title: string; public_id: string | null; snippet: string;
  created: number; updated: number; state: string | null; inbound: number; pinned: boolean;
};
export type SearchResp = {
  hits: Hit[]; total: number; by_db: [string, number][]; took_ms: number;
  warnings: string[]; plan?: string;
};
export type Ref = { id: string; title: string; db: string };
export type Person = { id: string; name: string };
export type Entity = {
  id: string; db: string; title: string; public_id: string | null;
  created: number; updated: number; created_by: Person | null; state: string | null; url?: string;
  fields: { name: string; value: unknown }[];
  relations: { field: string; items: Ref[] }[];
  people: { field: string; items: Person[] }[];
  docs: { field: string; secret: string; markdown: string }[];
  comments: { id: string; author: string; created: number; markdown: string }[];
  backlinks: { field: string; id: string; title: string; db: string }[];
};
export type Dbs = { spaces: { name: string; dbs: { name: string; short: string; count?: number }[] }[] };

async function getJson<T>(url: string, signal?: AbortSignal): Promise<T> {
  const r = await fetch(url, { signal });
  if (!r.ok) {
    let msg = `${r.status} ${r.statusText}`;
    try { msg = (await r.json()).error ?? msg; } catch { /* not json */ }
    throw new Error(msg);
  }
  return r.json();
}

export function search(q: string, signal?: AbortSignal, limit = 50): Promise<SearchResp> {
  return getJson(`/api/search?q=${encodeURIComponent(q)}&limit=${limit}`, signal);
}

// Hits seen in search results; lets the entity page paint its header before the fetch lands.
export const hitCache = new Map<string, Hit>();

const entityCache = new Map<string, Promise<Entity>>();
export function getEntity(id: string): Promise<Entity> {
  let p = entityCache.get(id);
  if (!p) {
    p = getJson<Entity>(`/api/entity/${id}`);
    p.catch(() => entityCache.delete(id));
    entityCache.set(id, p);
  }
  return p;
}
export const prefetchEntity = (id: string) => void getEntity(id).catch(() => {});
export const invalidateEntity = (id: string) => entityCache.delete(id);

let dbsP: Promise<Dbs> | null = null;
export const getDbs = () => (dbsP ??= getJson<Dbs>("/api/dbs"));
let syntaxP: Promise<string> | null = null;
export const getSyntax = () => (syntaxP ??= getJson<{ text: string }>("/api/syntax").then((r) => r.text));

export const resolve = (db: string, pid: string) =>
  getJson<{ id: string }>(`/api/resolve?db=${encodeURIComponent(db)}&pid=${encodeURIComponent(pid)}`);

export async function saveDoc(secret: string, markdown: string): Promise<{ ok: boolean; pushed: boolean }> {
  const r = await fetch(`/api/doc/${secret}`, {
    method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify({ markdown }),
  });
  if (!r.ok) throw new Error(`${r.status} ${await r.text()}`);
  return r.json();
}

export function fmtDate(t: number): string {
  if (!t) return "";
  return new Date(t * 1000).toLocaleDateString(undefined, { year: "numeric", month: "short", day: "numeric" });
}
export const shortDb = (db: string) => db.split("/").pop() ?? db;

export const dbFilter = (db: string) => (/\s/.test(db) ? `db:"${db}"` : `#${db}`);
