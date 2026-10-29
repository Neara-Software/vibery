import { useSearch } from "./SearchPage";
import { HitRow } from "./HitRow";

export default function Home() {
  const { res, err } = useSearch("sort:recent #note", 0);
  return (
    <div className="home">
      <h1 className="page-title">Recent notes</h1>
      {err && <div className="error">{err}</div>}
      {res?.hits.map((h) => <HitRow key={h.id} hit={h} />)}
    </div>
  );
}
