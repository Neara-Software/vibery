#!/usr/bin/env bash
#
# demo.sh — walk through every issue documented in
# https://neara.fibery.io/Shared/Note/fibery-search-issues-27735
# and show what vibery does about it.
#
#   ./demo.sh            full narrated walkthrough
#   ./demo.sh --table    just regenerate the README results table (Markdown)
#   ./demo.sh --quick    the walkthrough without the timing table at the end
#
# Builds the index first if it is missing.

set -uo pipefail
cd "$(dirname "$0")"

BIN=./target/release/vibery
INDEX=${VIBERY_INDEX:-index}
DATA=${VIBERY_DATA:-..}
ITERS=${ITERS:-7}

bold=$'\e[1m'; dim=$'\e[2m'; cyan=$'\e[1;36m'; mag=$'\e[1;35m'
yellow=$'\e[33m'; green=$'\e[32m'; off=$'\e[0m'

die() { printf '%s\n' "$*" >&2; exit 1; }

v() { "$BIN" --data "$DATA" --index "$INDEX" "$@"; }

# Print a query, then run it. This is the whole point of the demo: the query
# string is short enough to be the explanation.
show() {
  local desc=$1 query=$2 n=${3:-4}
  printf '\n%s%s%s\n' "$bold" "$desc" "$off"
  printf "  %s\$ vibery search '%s'%s\n" "$dim" "$query" "$off"
  v search "$query" -n "$n" | sed 's/^/  /'
}

run() {
  local desc=$1; shift
  printf '\n%s%s%s\n' "$bold" "$desc" "$off"
  # Quote any argument containing a space, so the printed command is one you
  # can actually paste back into a shell.
  local shown=() a
  for a in "$@"; do
    if [[ $a == *" "* ]]; then shown+=("'$a'"); else shown+=("$a"); fi
  done
  printf '  %s$ vibery %s%s\n' "$dim" "${shown[*]}" "$off"
  v "$@" | sed 's/^/  /'
}

section() {
  printf '\n\n%s╔══════════════════════════════════════════════════════════════════════════╗%s\n' "$mag" "$off"
  printf '%s║%s %-72s %s║%s\n' "$mag" "$off" "$1" "$mag" "$off"
  printf '%s╚══════════════════════════════════════════════════════════════════════════╝%s\n' "$mag" "$off"
}

quote() { printf '\n  %s“%s”%s\n' "$dim" "$1" "$off"; }

# ---------------------------------------------------------------- preflight

[[ -x $BIN ]] || { echo "building..."; cargo build --release --quiet || die "build failed"; }

if [[ ! -d $INDEX/tantivy ]]; then
  printf '%sNo index at %s — building it now (about 20s).%s\n\n' "$yellow" "$INDEX" "$off"
  v ingest --heap-mb 3000 --threads 8 || die "ingest failed"
fi

# --table: regenerate the README table and stop.
if [[ ${1:-} == --table ]]; then
  exec "$BIN" --data "$DATA" --index "$INDEX" bench --iters "$ITERS" --format md
fi

printf '%s' "$cyan"
cat <<'BANNER'
        _ _
 __   _(_) |__   ___ _ __ _   _
 \ \ / / | '_ \ / _ \ '__| | | |
  \ V /| | |_) |  __/ |  | |_| |
   \_/ |_|_.__/ \___|_|   \__, |
                          |___/
BANNER
printf '%s' "$off"
echo "  Every complaint from the fibery-search-issues note, and what happens now."
run "The index" stats

# ============================================================================
section "1. \"Created by: Sami Kyostila\" with empty text string -> only 2 docs"
quote "Expected: a lot of docs. Actual: only 2 docs."
echo "  Fibery's search takes text + database + viewType, and nothing else. A field"
echo "  filter is applied *after* a text match, so an empty text string leaves"
echo "  nothing to filter. Here structure is indexed as terms, so no text is needed:"
show "456 entities, no text term at all" 'by:@sami' 4
show "Composes with everything else" 'by:@sami #note sort:recent' 3

# ============================================================================
section "2. Search is slow"
echo "  Every query in this demo prints its own latency. The worst realistic case:"
show "A phrase with 11k hits" '"point cloud"' 3
echo
echo "  ${dim}Full timing table for all 24 cases: ./demo.sh --table${off}"

# ============================================================================
section "3. Fuzzy search / synonyms / tenses aren't handled"
quote "I would want to search for \"subdivide\" and have my scope titled \"Dynamic World Elevation Geometry subdivision in wasm\" show up. Testing just now, it didn't."
echo "  English stemming reduces the pair to 'subdivid' and 'subdivis' — one edit"
echo "  apart — and a distance-1 fuzzy tier bridges that. The named scope, found:"
printf "\n  %s\$ vibery search 'subdivide #scope' --terse%s\n" "$dim" "$off"
v search 'subdivide #scope' -n 12 --terse | grep -i 'subdivision in wasm' | sed 's/^/  ✓ /'
show "All the morphological variants at once" 'subdivide #scope' 5
printf '\n  %sProof the fuzzy tier is what does it:%s\n' "$bold" "$off"
printf '    fuzzy on  : %s matches\n' "$(v search 'subdivide #scope' -n 0 | grep -oE '[0-9]+ match' | head -1 | cut -d' ' -f1)"
printf '    strict    : %s matches\n' "$(v search 'subdivide #scope' -n 0 --strict | grep -oE '[0-9]+ match' | head -1 | cut -d' ' -f1)"

# ============================================================================
section "4. (Toby) \"DataView\" doesn't find docs named \"Data Views\""
quote "if someone searches \"DataView\", then because the docs are named \"Data Views\" it does not show up."
echo "  A camelCase-splitting tokenizer normalises both sides to the same two"
echo "  tokens, so this matches as an ordinary phrase — no fuzziness involved:"
show "Top hit is the thing itself" 'DataView' 4

# ============================================================================
section "5. Searching does not prioritise useful information"
quote "Expected: order by likelihood of utility — most current, updated, or detailed first. Actual: ordered by most recently accessed, and does not deprioritise guides marked 'not started'."
echo "  Ranking is now BM25 x authority(inbound refs) x substance(length)"
echo "  x bounded recency x lifecycle. Watch the state labels descend:"
show "Live, then Draft, then Not started" '#guide claude' 6
echo "  ${dim}Recency is capped at +25% decaying over a year, which is Toby's other"
echo "  concern: a good doc shouldn't vanish once it stops being the newest.${off}"

# ============================================================================
section "6. (Michael) Filter by database without using the mouse"
quote "RVO as a search term is useless… I will not see it unless I filter by database, because we use Notes for everything. But doing this requires mouse clicks."
show "Bare 'rvo' — his complaint, quantified: spread over 54 databases" 'rvo' 3
show "What he actually wants to type" 'rvo #bet' 4
quote "What would be ideal is syntax in the search bar… It will update the filtering as I type."
show "Prefix matching, for per-keystroke narrowing" 'interv* #note' 3
run "And #names resolve fuzzily, so you never need the full path" dbs bet

# ============================================================================
section "7. (Michael) Search for entities related to other things"
quote "all Notes related to the Platform team that contain the word \"Interventions\""
echo "  A subquery resolves the anchor entities, then the outer query filters on"
echo "  the relation. Note result #2 — it is the very note asking for this."
show "The literal request, as one line" '#note related:(#team platform) interventions' 4
echo "  ${dim}Naming the relation lets the catalog constrain the subquery to the types"
echo "  that field can point at — 4096 anchors and 33ms became 5 anchors and 3ms:${off}"
show "Type-constrained named traversal" '#note team:(platform)' 3

# ============================================================================
section "8. (Michael) What has my teammate worked on or commented on recently?"
quote "The only way to do this is to create views. But I am just trying to find something for that specific person at that specific time."
echo "  Every one of the schema's 812 fibery/user fields is indexed as a"
echo "  participation edge, so this is one term lookup across all 211 databases:"
run "One command, no view required" activity tokar --days 21 -n 6
run "Split by how they were involved" activity sami --days 14 -n 3 --by-role

# ============================================================================
section "9. Locating your current entity/document is basically impossible"
quote "is there a button in fibery to locate your current document? e.g. I have a guide and I want to find what folder it is located inside"
echo "  Folder hierarchy isn't in this dump, so this answers the generalisation:"
echo "  an entity's location IS its parents, plus everything pointing at it."
run "Where does this live?" locate "Keycloak / SCIM Uplift"

# ============================================================================
section "10. Can't Ctrl-F inside a single entity, especially entity feeds"
quote "you can't hit Ctrl-F to find stuff in a single entity reliably, especially in entity feeds like this one"
echo "  The feed is the hard part: a browser's Ctrl-F only sees what is rendered."
echo "  Ingest folded the whole entity together, so all of it is searchable:"
run "Find within one entity, with line numbers" find "Keycloak / SCIM Uplift" scim
printf '  %sCaveat: this dump contains no comment text at all — comment bodies live\n' "$yellow"
printf '  under comment/document-secret, which the scraper never fetched. So\n'
printf '  commented-by: works but comments:<term> cannot match. See `vibery stats`.%s\n' "$off"

# ============================================================================
section "11. The sidebar search tool is a lie"
quote "Try typing in something that's definitely in the list (e.g. \`Experience\`). Did you expect it to show up? Joke's on you!"
show "Prefix matching on titles, which is what a filter box should do" 'title:experience*' 4

# ============================================================================
section "12. (Toby) Pinned pages for specific search terms"
quote "I would like to see pages pinned at the top for specific search terms… I have no guarantee that a doc I write will be visible in 6 months time."
echo "  An optional pins.json promotes a curated entity above whatever BM25 picks."
if [[ ! -f $INDEX/pins.json && -f pins.example.json ]]; then
  cp pins.example.json "$INDEX/pins.json"
  printf '  %s(installed pins.example.json -> %s/pins.json for this demo)%s\n' "$dim" "$INDEX" "$off"
fi
run "Configured pins" pins
show "Unpinned, the workshop note led; now the setup guide does" 'claude' 3

# ============================================================================
section "13. (Lloyd) Two kinds of search"
quote "1. You know the thing exists… I can find it if I make a private view (Table, entity types, filters). I want to use Search in this way -> as a quick disposable query builder."
echo "  That is exactly what the query language is. A whole view, typed inline:"
show "Open issues, assigned, first half of 2026, not Rollbar noise, newest first" \
     '#issue is:open has:assignee created:2026-01..2026-06 -title:rollbar sort:recent' 4
quote "2. You aren't sure the thing exists… the existing fuzzy search which we aren't fond of."
echo "  Covered by sections 3 and 4 above: stemming, camelCase and a fuzzy tier."

# ============================================================================
if [[ ${1:-} != --quick ]]; then
  section "Timings — all 24 cases"
  v bench --iters "$ITERS"
fi

section "Try it yourself"
cat <<EOF
  $BIN --index $INDEX repl        interactive, prints latency per query
  $BIN --index $INDEX syntax      the full query language
  ./demo.sh --table               regenerate the README results table
EOF
echo
