import { memo, useEffect, useRef, useState, ReactNode, ComponentProps } from "react";
import ReactMarkdown, { Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import rehypeRaw from "rehype-raw";
import rehypeSanitize, { defaultSchema } from "rehype-sanitize";
import { visit } from "unist-util-visit";
import { Link } from "react-router-dom";

const schema = {
  ...defaultSchema,
  attributes: {
    ...defaultSchema.attributes,
    blockquote: [...(defaultSchema.attributes?.blockquote ?? []), "dataColor", "dataIcon"],
    code: [["className", /^language-./]],
    span: [...(defaultSchema.attributes?.span ?? []), "bgColor"],
  },
};

// `> [//]: # (callout;icon-type=icon;icon=…;color=#hex)` parses as a definition node
// inside the blockquote; lift it into attributes and drop it.
function remarkCallouts() {
  return (tree: any) => {
    visit(tree, "blockquote", (node: any) => {
      const first = node.children?.[0];
      if (first?.type !== "definition" || !first.title?.startsWith("callout")) return;
      const kv = Object.fromEntries(
        first.title.split(";").slice(1).map((p: string) => {
          const i = p.indexOf("=");
          return [p.slice(0, i), p.slice(i + 1)];
        }),
      );
      node.children.shift();
      node.data = { ...node.data, hProperties: { dataColor: kv.color ?? "", dataIcon: kv.icon ?? "" } };
    });
  };
}

const EMOJI: Record<string, string> = { ":exclamation:": "❗", ":warning:": "⚠️", ":bulb:": "💡", ":information_source:": "ℹ️" };

let hljsP: Promise<typeof import("./highlight")> | null = null;

function CodeBlock({ lang, code }: { lang: string; code: string }) {
  const ref = useRef<HTMLElement>(null);
  useEffect(() => {
    let live = true;
    (hljsP ??= import("./highlight")).then((m) => {
      if (live && ref.current) ref.current.innerHTML = m.highlight(code, lang);
    });
    return () => { live = false; };
  }, [code, lang]);
  return <code ref={ref} className={"hljs" + (lang ? ` language-${lang}` : "")}>{code}</code>;
}

function FileImage(props: ComponentProps<"img">) {
  const src = props.src ?? "";
  const [base, frag = ""] = src.split("#");
  const width = new URLSearchParams(frag).get("width");
  return (
    <img src={base} alt={props.alt} loading="lazy" decoding="async"
      style={width ? { width: `min(${width}px, 100%)` } : undefined} />
  );
}

const HL: Record<string, string> = { YELLOW: "#ffecc4", BLUE: "#e1eeff", GREEN: "#daf6ca", RED: "#ffe8e6", GREY: "#eee" };

const components: Components = {
  span(p) {
    const bg = (p as any).bgColor as string | undefined;
    if (!bg) return <span>{p.children}</span>;
    return <span className="hl" style={{ background: HL[bg.toUpperCase()] ?? "#daf6ca" }}>{p.children}</span>;
  },
  a({ href = "", children }) {
    if (href.startsWith("/") && !href.startsWith("/api/")) return <Link to={href}>{children}</Link>;
    return <a href={href} target="_blank" rel="noreferrer">{children}</a>;
  },
  img: (p) => <FileImage {...p} />,
  code({ className, children }) {
    const text = String(children ?? "");
    const m = /language-(\S+)/.exec(className ?? "");
    if (!m && !text.includes("\n")) return <code>{children}</code>;
    return <CodeBlock lang={m?.[1] ?? ""} code={text.replace(/\n$/, "")} />;
  },
  blockquote(p) {
    const color = (p as any)["data-color"] as string | undefined;
    if (!color) return <blockquote>{p.children}</blockquote>;
    const icon = (p as any)["data-icon"] as string;
    return (
      <div className="callout" style={{ borderColor: color, background: `color-mix(in srgb, ${color} 9%, white)` }}>
        <span className="callout-icon" style={{ color }}>{EMOJI[icon] ?? (icon?.startsWith(":") ? "❗" : "ⓘ")}</span>
        <div className="callout-body">{p.children as ReactNode}</div>
      </div>
    );
  },
  table: ({ children }) => <div className="table-wrap"><table>{children}</table></div>,
};

const Markdown = memo(function Markdown({ text }: { text: string }) {
  return (
    <div className="md">
      <ReactMarkdown
        remarkPlugins={[remarkGfm, remarkCallouts]}
        rehypePlugins={[rehypeRaw, [rehypeSanitize, schema]]}
        components={components}
      >
        {text}
      </ReactMarkdown>
    </div>
  );
});
export default Markdown;

export function useDeferredText(text: string, ms: number) {
  const [v, setV] = useState(text);
  useEffect(() => { const t = setTimeout(() => setV(text), ms); return () => clearTimeout(t); }, [text, ms]);
  return v;
}
