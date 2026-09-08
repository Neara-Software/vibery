//! Text analysis, shared by the indexer and the query planner.
//!
//! Two complaints from the note land here:
//!
//! * *"I would want to search for `subdivide` and have `…Geometry subdivision
//!   in wasm` show up."* — English stemming reduces both to near-identical
//!   stems (`subdivid` / `subdivis`), which a distance-1 fuzzy match then
//!   bridges. Stemming alone is not enough; that pair is why the fuzzy tier
//!   exists.
//! * *"if someone searches `DataView`, the docs are named `Data Views` so it
//!   does not show up."* — splitting camelCase normalises both sides to the
//!   same two tokens, so the phrase matches with no fuzziness at all.
//!
//! Index-time and query-time analysis MUST agree, so both call [`analyze`]
//! against the same [`TextAnalyzer`]. Nothing here re-implements the pipeline.

use tantivy::tokenizer::{
    Language, LowerCaser, RemoveLongFilter, Stemmer, TextAnalyzer, Token, TokenStream, Tokenizer,
};

/// Name under which the analyzer is registered on the index.
pub const TOKENIZER: &str = "vib";

/// Longest token we keep; longer ones are base64 blobs and hashes.
const MAX_TOKEN_LEN: usize = 40;

/// Splits on non-alphanumeric boundaries *and* inside camelCase runs.
///
/// `DataView` -> `Data`, `View`; `HTTPServer` -> `HTTP`, `Server`.
/// Digits stay attached (`wasm32`, `P1`, `5x5` are single tokens) because
/// splitting them produced more noise than signal on this corpus.
#[derive(Clone, Default)]
pub struct CamelSplit;

impl Tokenizer for CamelSplit {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> Self::TokenStream<'a> {
        let mut tokens: Vec<Token> = Vec::new();
        let bytes = text.as_bytes();
        let mut pos = 0usize;
        let mut start: Option<usize> = None;

        // Byte offsets are what Token wants; we only ever split on ASCII
        // boundaries, and non-ASCII chars are treated as alphanumeric content.
        let mut prev: Option<char> = None;
        let mut it = text.char_indices().peekable();
        while let Some((i, c)) = it.next() {
            let alnum = c.is_alphanumeric();
            if !alnum {
                if let Some(s) = start.take() {
                    push(&mut tokens, text, s, i, &mut pos);
                }
                prev = None;
                continue;
            }
            // camelCase boundary detection.
            let boundary = match prev {
                Some(p) => {
                    let lower_to_upper = (p.is_lowercase() || p.is_numeric())
                        && c.is_uppercase();
                    // `HTTPServer`: split before the S, not after it.
                    let acronym_end = p.is_uppercase()
                        && c.is_uppercase()
                        && it.peek().is_some_and(|(_, n)| n.is_lowercase());
                    lower_to_upper || acronym_end
                }
                None => false,
            };
            if boundary {
                if let Some(s) = start.take() {
                    push(&mut tokens, text, s, i, &mut pos);
                }
            }
            if start.is_none() {
                start = Some(i);
            }
            prev = Some(c);
            let _ = bytes;
        }
        if let Some(s) = start {
            push(&mut tokens, text, s, text.len(), &mut pos);
        }

        VecTokenStream { tokens, idx: 0 }
    }
}

fn push(tokens: &mut Vec<Token>, text: &str, from: usize, to: usize, pos: &mut usize) {
    let slice = &text[from..to];
    if slice.is_empty() || slice.len() > MAX_TOKEN_LEN {
        return;
    }
    tokens.push(Token {
        offset_from: from,
        offset_to: to,
        position: *pos,
        text: slice.to_string(),
        position_length: 1,
    });
    *pos += 1;
}

pub struct VecTokenStream {
    tokens: Vec<Token>,
    /// Number of tokens consumed; `token()` returns `tokens[idx - 1]`.
    idx: usize,
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        if self.idx < self.tokens.len() {
            self.idx += 1;
            true
        } else {
            false
        }
    }
    fn token(&self) -> &Token {
        &self.tokens[self.idx - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.idx - 1]
    }
}

/// The analyzer used for `title`, `body` and `comments`, on both sides.
pub fn analyzer() -> TextAnalyzer {
    TextAnalyzer::builder(CamelSplit)
        .filter(RemoveLongFilter::limit(MAX_TOKEN_LEN))
        .filter(LowerCaser)
        .filter(Stemmer::new(Language::English))
        .build()
}

/// Run the analyzer and collect the resulting terms.
pub fn analyze(a: &mut TextAnalyzer, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut stream = a.token_stream(text);
    while stream.advance() {
        out.push(stream.token().text.clone());
    }
    out
}

/// Elasticsearch-style auto fuzziness: short words are too dangerous to fuzz.
///
/// `cat` at distance 1 matches car, bat, cats, cot… — pure noise. Long words
/// have enough signal that one edit is almost always a real variant.
pub fn auto_fuzz(term: &str) -> Option<u8> {
    match term.chars().count() {
        0..=4 => None,
        _ => Some(1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<String> {
        analyze(&mut analyzer(), s)
    }

    /// Toby's case: `DataView` must find a doc titled `Data Views`.
    #[test]
    fn camel_case_normalises_both_sides() {
        assert_eq!(toks("DataView"), vec!["data", "view"]);
        assert_eq!(toks("Data Views"), vec!["data", "view"]);
        assert_eq!(toks("DataView"), toks("Data Views"));
    }

    #[test]
    fn acronyms_split_before_the_capitalised_word() {
        assert_eq!(toks("HTTPServer"), vec!["http", "server"]);
        assert_eq!(toks("RVOPod"), vec!["rvo", "pod"]);
    }

    #[test]
    fn digits_stay_attached() {
        assert_eq!(toks("wasm32"), vec!["wasm32"]);
        assert_eq!(toks("P1"), vec!["p1"]);
    }

    /// The `subdivide` / `subdivision` case: stemming gets them to within one
    /// edit of each other, which is what makes the fuzzy tier able to bridge it.
    #[test]
    fn stemming_brings_subdivide_within_one_edit_of_subdivision() {
        let a = toks("subdivide");
        let b = toks("subdivision");
        assert_eq!(a.len(), 1);
        assert_eq!(b.len(), 1);
        let d = levenshtein(&a[0], &b[0]);
        assert!(
            d <= 1,
            "stems {:?} and {:?} are {d} edits apart; the fuzzy tier uses distance 1",
            a[0],
            b[0]
        );
    }

    #[test]
    fn ordinary_tenses_and_plurals_collapse_without_fuzz() {
        assert_eq!(toks("running"), toks("run"));
        assert_eq!(toks("interventions"), toks("intervention"));
        assert_eq!(toks("analysed"), toks("analyse"));
    }

    #[test]
    fn punctuation_and_case_are_boundaries() {
        assert_eq!(toks("load-flow, ANALYSIS"), vec!["load", "flow", "analysi"]);
    }

    #[test]
    fn short_words_are_not_fuzzed() {
        assert_eq!(auto_fuzz("cat"), None);
        assert_eq!(auto_fuzz("data"), None);
        assert_eq!(auto_fuzz("subdivid"), Some(1));
    }

    #[test]
    fn overlong_tokens_are_dropped() {
        let long = "a".repeat(80);
        assert!(toks(&long).is_empty());
    }

    /// Test-only reference implementation, used to assert the stem distance.
    fn levenshtein(a: &str, b: &str) -> usize {
        let a: Vec<char> = a.chars().collect();
        let b: Vec<char> = b.chars().collect();
        let mut prev: Vec<usize> = (0..=b.len()).collect();
        let mut cur = vec![0usize; b.len() + 1];
        for i in 1..=a.len() {
            cur[0] = i;
            for j in 1..=b.len() {
                let cost = usize::from(a[i - 1] != b[j - 1]);
                cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            }
            std::mem::swap(&mut prev, &mut cur);
        }
        prev[b.len()]
    }
}
