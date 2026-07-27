# ygrep 3.5.1 vs 4.0.0

Head-to-head measurement of the 3.5.1 release against the 4.0.0 development tree, run on
2026-07-27 with `compare.sh`.

## Setup

| | |
|---|---|
| Machine | Apple M4 Max, 14 cores, 36 GB RAM |
| OS | macOS 26.5.2 (build 25F84) |
| Toolchain | rustc 1.97.1, `cargo build --release` |
| Baseline | tag `v3.5.1`, built in a throwaway `git worktree` |
| Candidate | working tree at the end of phase 6 |

Both binaries ran with `--data-dir` pointing at a throwaway directory and
`XDG_CONFIG_HOME` pointing at an empty config dir, so neither touched real indexes and
both ran on built-in defaults. Queries used `--no-auto-index` so the staleness refresh
never ran inside a timed query.

No `hyperfine` on this host, so timing is a scripted loop: median of N runs after W
warmups, wall time measured around the process with `time.perf_counter()`. Builds use
N=3/W=1, no-op indexing and queries use N=10/W=2. Cold builds delete the data dir before
every iteration. Query tables also report the tool's own `query_time_ms` from `--json`,
which excludes process startup (~5 ms of the wall time).

## Corpora

| Name | Files walked | Indexable (3.5.1 / 4.0.0) | Content | Notes |
|---|---|---|---|---|
| `ygrep` | 68 tracked | 69 / 73 files, 0.73 / 0.75 MB | Rust | this repo |
| `grav-learn` | 9.6k | 8489 / 8517 files, 11.06 MB | Markdown, PHP, Twig, YAML | documentation site checkout |
| `github` | 320k | 44338 / 44814 files, 350 MB | C, C++, JS, HTML, PHP, TXT | ~200 open-source checkouts in one tree, includes 662 symlinks |

4.0.0 walks a slightly different file set on purpose: it no longer skips every dotfile
(`.github/workflows/*.yml`, `.editorconfig`, `.gitignore` are now indexed) and it honours
nested `.gitignore` files rather than only the one at the root. On `github` that is +686
files from dot-paths and -222 files newly excluded by nested ignore rules.

## Indexing

Median wall time.

| Corpus | Workload | 3.5.1 | 4.0.0 | Delta |
|---|---|---|---|---|
| ygrep (73 files) | full build (cold) | 201 ms | 205 ms | +2.1% |
| | rebuild | 203 ms | 214 ms | +5.5% |
| | no-op incremental | 19.0 ms | 18.8 ms | -1.2% |
| grav-learn (8.5k files) | full build (cold) | 902 ms | 447 ms | **-50.4%** |
| | rebuild | 919 ms | 440 ms | **-52.1%** |
| | no-op incremental | 595 ms | 185 ms | **-68.9%** |
| github (44.8k files, 350 MB) | full build (cold) | 10186 ms | 11659 ms | +14.5% |
| | rebuild | 10607 ms | 11280 ms | +6.3% |
| | no-op incremental | 3499 ms | 898 ms | **-74.3%** |

The no-op result is the headline: an unchanged tree is now decided from the walker's own
directory metadata instead of re-reading and re-stat-ing every file, so a workspace that
hasn't changed costs a fraction of what it did.

The large-corpus build regression is the parallel walk, not the walk-metadata work. Same
corpus, 3 runs, re-measured together:

| github, full build | Median | Index on disk |
|---|---|---|
| 3.5.1 | 10156 ms | 179 MB |
| 4.0.0 (default threads) | 11099 ms | 203 MB |
| 4.0.0, `indexer.threads = 1` | 10645 ms | 179 MB |

On a 350 MB tree the build is already saturated by tantivy's own indexing threads, so
walking in parallel adds contention rather than throughput. On the 11 MB tree it is worth
having: 876 ms → 529 ms with parallel walking versus 646 ms single-threaded.

## Index on disk

Measured after the build/rebuild/no-op sequence above.

| Corpus | 3.5.1 | 4.0.0 |
|---|---|---|
| ygrep | 564 KB, 3 segments, 69 files | 568 KB, 3 segments, 73 files |
| grav-learn | 8.2 MB, 7 segments, 8489 files | 8.0 MB, 5 segments, 8517 files |
| github | 180 MB, 1 segment, 44285 files | 204 MB, 10 segments, 44761 files |

Component sizes for a fresh single-segment `github` build show the growth is entirely in
the doc store, not the postings:

| File | 3.5.1 | 4.0.0 | 4.0.0 `threads = 1` |
|---|---|---|---|
| `.store` | 60.52 MB | 68.07 MB | 60.68 MB |
| `.pos` | 72.72 MB | 73.94 MB | 72.80 MB |
| `.idx` | 23.36 MB | 23.90 MB | 23.25 MB |
| `.term` | 16.45 MB | 16.49 MB | 16.48 MB |

The doc store compresses in 64 KB blocks. A sequential walk feeds it files from one
directory at a time, which compress well together; a parallel walk interleaves unrelated
files into the same block. Setting `indexer.threads = 1` restores the old footprint
exactly. Compaction does not recover it — the index is already one segment.

## Queries

Median wall time / tool-reported `query_time_ms` / hits returned, limit 100.

### grav-learn

| Query | 3.5.1 | 4.0.0 | Delta |
|---|---|---|---|
| common word (`page`) | 10.9 ms / 4 ms / 100 | 10.1 ms / 4 ms / 100 | -7.2% |
| rare identifier (`onPageInitialized`) | 8.6 ms / 2 ms / 12 | 7.1 ms / 1 ms / 12 | -16.8% |
| two-word phrase (`page collection`) | 30.5 ms / 23 ms / 91 | 80.3 ms / 73 ms / 91 | **+163%** |
| punctuation (`->`) | 14.9 ms / 8 ms / 100 | 15.1 ms / 8 ms / 100 | +0.9% |
| punctuation (`<<<`) | 29.6 ms / 23 ms / 3 | 14.2 ms / 7 ms / 3 | **-51.9%** |
| regex (`function\s+get`) | 17.7 ms / 11 ms / 100 | 17.0 ms / 11 ms / 100 | -4.4% |
| `-e md` | 10.8 ms / 4 ms / **20** | 11.4 ms / 5 ms / **100** | +5.8% |
| `-p pages/` | 10.8 ms / 4 ms / **52** | 12.3 ms / 6 ms / **100** | +13.7% |

### github

| Query | 3.5.1 | 4.0.0 | Delta |
|---|---|---|---|
| common word (`return`) | 16.6 ms / 10 ms / 100 | 16.5 ms / 10 ms / 100 | -0.4% |
| rare identifier (`MarlinSettings`) | 7.9 ms / 2 ms / 10 | 8.6 ms / 2 ms / 10 | +9.2% |
| two-word phrase (`memory allocation`) | 116 ms / 109 ms / 84 | 232 ms / 225 ms / 84 | **+99.5%** |
| punctuation (`->`) | 15.3 ms / 9 ms / 100 | 15.8 ms / 10 ms / 100 | +3.7% |
| punctuation (`<<<`) | 202 ms / 195 ms / 100 | 53 ms / 46 ms / 100 | **-73.6%** |
| regex (`void\s+\w+_init`) | 41.9 ms / 35 ms / 100 | 52.2 ms / 45 ms / 100 | +24.6% |
| `-e c` (`malloc`) | 23.2 ms / 17 ms / **61** | 15.2 ms / 9 ms / **100** | **-34.4%** |
| `-p Marlin/` (`temperature`) | 16.5 ms / 10 ms / **89** | 15.8 ms / 9 ms / **100** | -4.4% |

`ygrep` (73 files) is too small to separate anything from process startup: every query
runs in 6-11 ms on both, with `->` and `<<<` about 19% faster on 4.0.0.

### Filtered queries return more

3.5.1 applied `-e`/`-p` after truncating to the limit, so a filter could throw away most
of the page it was handed. 4.0.0 pushes the extension into the tantivy query and filters
paths before truncation. Same index, same query, hits returned:

| Corpus | Query | Limit | 3.5.1 | 4.0.0 |
|---|---|---|---|---|
| grav-learn | `-e md config` | 20 | 7 | 20 |
| | | 100 | 20 | 100 |
| | `-p pages/ page` | 20 | 13 | 20 |
| | | 100 | 52 | 100 |
| github | `-e c malloc` | 20 | 20 | 20 |
| | | 100 | 61 | 100 |
| | `-p Marlin/ temperature` | 20 | 20 | 20 |
| | | 100 | 89 | 100 |

The `-p pages/` and `-e md` rows are where the two effects meet: 4.0.0 is a few percent
slower on those queries because it is filling a page that 3.5.1 left half empty.

### The phrase-query regression

Both versions fetch a pool of BM25 candidates and then check each one for the literal.
3.5.1 always fetched `limit × 50`. 4.0.0 fetches `limit × 5` first and only escalates to
`limit × 100` when the first pass came up short, which makes the common case cheaper and
raises the ceiling for genuinely sparse queries.

A query with fewer matches than the limit pays for both passes:

| github, `memory allocation` (84 matches) | 3.5.1 | 4.0.0 |
|---|---|---|
| `-n 100` (starved) | 111 ms | 227 ms |
| `-n 20` (not starved) | 9 ms | 7 ms |

So the cost is confined to multi-term literal queries asking for more results than exist,
where the second pass examines 100× the limit rather than 50×. The same mechanism
explains the regex row (`10×` then `200×` versus a flat `100×`).

## Peak RSS

`/usr/bin/time -l`, full build of `github` into a fresh data dir.

| | Peak RSS |
|---|---|
| 3.5.1 | 501 MB |
| 4.0.0 | 536 MB (+7.0%) |
| 4.0.0, `indexer.threads = 1` | 414 MB (-17.4%) |

Parallel walking holds more file contents in flight at once. Single-threaded, 4.0.0 uses
less memory than 3.5.1 did, because the embedding batch no longer accumulates whole file
contents for the entire walk.

## Anomalies

**Symlinked directories churn on every incremental pass (4.0.0 only).** A workspace where
two directories are symlinks to the same target indexes the target once, under whichever
alias the walk reached first. The sequential walk in 3.5.1 always picked the same alias;
the parallel walk in 4.0.0 picks whichever thread got there first, so the winner changes
run to run. On the `github` corpus (662 symlinks, mostly shared plugin directories) every
no-op `ygrep index` reports 119-256 files indexed and the same number removed, with the
index alternating between alias paths:

```
Files indexed: 256
Files unchanged: 44505
Files removed: 256
```

Nothing is lost — the content stays indexed under one path — but the pass does needless
work and leaves a new segment behind each time, so the segment count climbs (1 → 4 → 5
over three no-op runs) where 3.5.1 stayed at 1. Corpora without symlinks are unaffected:
`grav-learn`, `grav` and this repo all report 0 files indexed on a no-op. Worth fixing by
making alias selection deterministic (e.g. keeping the lexicographically first alias)
rather than order-dependent.

**Index size and build time on large corpora.** Covered above: both come from the
parallel walk, and `indexer.threads = 1` restores 3.5.1's numbers on both counts while
keeping the no-op win, which is where the large-corpus gain actually is.

## Reproducing

```bash
git worktree add /tmp/ygrep-baseline v3.5.1
(cd /tmp/ygrep-baseline && cargo build --release)
cargo build --release

benchmark/compare-3.5-vs-4.0/compare.sh \
  --old /tmp/ygrep-baseline/target/release/ygrep \
  --new target/release/ygrep \
  --corpus ygrep=$PWD \
  --corpus grav-learn=~/Projects/grav/grav-learn \
  --corpus github=~/Projects/github
```

Per-corpus query sets live in `queries_for()` in the script; add a `case` arm for a new
corpus name. Raw output for this run is in `results/`.
