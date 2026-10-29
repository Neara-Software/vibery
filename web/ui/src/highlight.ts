import hljs from "highlight.js/lib/common";
import "highlight.js/styles/github.css";

export function highlight(code: string, lang: string): string {
  if (lang && hljs.getLanguage(lang)) return hljs.highlight(code, { language: lang, ignoreIllegals: true }).value;
  return hljs.highlightAuto(code).value;
}
