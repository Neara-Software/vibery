import { useEffect, useRef, useState } from "react";
import { saveDoc } from "./api";
import Markdown, { useDeferredText } from "./Markdown";

export default function Editor({ secret, initial, onClose }: {
  secret: string; initial: string; onClose: (saved: boolean) => void;
}) {
  const [text, setText] = useState(initial);
  const [status, setStatus] = useState("");
  const [saved, setSaved] = useState(false);
  const preview = useDeferredText(text, 150);
  const ref = useRef<HTMLTextAreaElement>(null);
  const textRef = useRef(text);
  textRef.current = text;

  const save = async () => {
    setStatus("Saving…");
    try {
      const r = await saveDoc(secret, textRef.current);
      setSaved(true);
      setStatus(r.pushed ? "Saved + pushed to Fibery" : "Saved locally");
    } catch (e) {
      setStatus(`Save failed: ${(e as Error).message}`);
    }
  };

  useEffect(() => {
    ref.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "s" && (e.metaKey || e.ctrlKey)) { e.preventDefault(); save(); }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="editor">
      <div className="editor-bar">
        <button className="btn primary" onClick={save}>Save ⌘S</button>
        <button className="btn" onClick={() => onClose(saved)}>Done</button>
        <span className="muted">{status}</span>
      </div>
      <div className="editor-split">
        <textarea ref={ref} value={text} spellCheck={false} onChange={(e) => setText(e.target.value)} />
        <div className="editor-preview"><Markdown text={preview} /></div>
      </div>
    </div>
  );
}
