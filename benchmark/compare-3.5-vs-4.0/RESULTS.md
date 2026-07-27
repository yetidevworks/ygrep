# ygrep 3.5.1 vs 4.0.0

Head-to-head measurement of the 3.5.1 release against the 4.0.0 release tree, run on
2026-07-27 with `compare.sh`.

## Setup

| | |
|---|---|
| Machine | Apple M4 Max, 14 cores, 36 GB RAM |
| OS | macOS 26.5.2 (build 25F84) |
| Toolchain | rustc 1.97.1, `cargo build --release` |
| Baseline | tag `v3.5.1`, built in a throwaway `git worktree` |
| Candidate | the 4.0.0 release tree |

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

| Name | Indexable (3.5.1 / 4.0.0) | Content | Notes |
|---|---|---|---|
| `ygrep` | 72 / 76 files, 759 / 780 KB | Rust | this repo |
| `grav-learn` | 8489 / 8517 files, 11.05 / 11.06 MB | Markdown, PHP, Twig, YAML | documentation site checkout |
| `github` | 44338 / 44814 files, 350.3 / 350.7 MB | C, C++, JS, HTML, PHP, TXT | ~200 open-source checkouts in one tree, includes 662 symlinks |

4.0.0 walks a slightly different file set on purpose: it no longer skips every dotfile
(`.github/workflows/*.yml`, `.editorconfig`, `.gitignore` are now indexed) and it honours
nested `.gitignore` files rather than only the one at the root. On `github` that is +686
files from dot-paths and -222 files newly excluded by nested ignore rules.

## Indexing

Median wall time.

| Corpus | Workload | 3.5.1 | 4.0.0 | Delta |
|---|---|---|---|---|
| ygrep (76 files) | full build (cold) | 187 ms | 208 ms | +11.3% |
| | rebuild | 235 ms | 209 ms | -10.8% |
| | no-op incremental | 19.0 ms | 18.0 ms | -4.9% |
| grav-learn (8.5k files) | full build (cold) | 892 ms | 433 ms | **-51.4%** |
| | rebuild | 886 ms | 425 ms | **-52.0%** |
| | no-op incremental | 615 ms | 249 ms | **-59.4%** |
| github (44.8k files, 350 MB) | full build (cold) | 10161 ms | 11087 ms | +9.1% |
| | rebuild | 10582 ms | 11599 ms | +9.6% |
| | no-op incremental | 3451 ms | 842 ms | **-75.6%** |

The no-op result is the headline: an unchanged tree is now decided from the walker's own
directory metadata instead of re-reading and re-stat-ing every file, so a workspace that
hasn't changed costs a fraction of what it did.

The large-corpus build regression is the parallel walk, not the walk-metadata work:

| github, full build | Median | Index on disk |
|---|---|---|
| 3.5.1 | 10161 ms | 180 MB |
| 4.0.0 (default threads) | 11087 ms | 202 MB |
| 4.0.0, `indexer.threads = 1` | 10615 ms | 180 MB |

On a 350 MB tree the build is already saturated by tantivy's own indexing threads, so
walking in parallel adds contention rather than throughput. On the 11 MB tree it is worth
having: a cold build takes 433 ms with the parallel walk against 670 ms single-threaded,
where 3.5.1 took 892 ms.

## Index on disk

Measured after the build/rebuild/no-op sequence above.

| Corpus | 3.5.1 | 4.0.0 |
|---|---|---|
| ygrep | 612 KB, 4 segments, 72 files | 616 KB, 4 segments, 76 files |
| grav-learn | 8.1 MB, 6 segments, 8489 files | 8.1 MB, 6 segments, 8517 files |
| github | 180 MB, 1 segment, 44285 files | 202 MB, 1 segment, 44761 files |

Component sizes for the single-segment `github` index show the growth is entirely in the
doc store, not the postings:

| File | 3.5.1 | 4.0.0 | 4.0.0 `threads = 1` |
|---|---|---|---|
| `.store` | 57.71 MB | 65.61 MB | 57.86 MB |
| `.pos` | 69.43 MB | 70.70 MB | 69.47 MB |
| `.idx` | 22.28 MB | 22.87 MB | 22.50 MB |
| `.term` | 15.69 MB | 15.72 MB | 15.73 MB |

The doc store compresses in 64 KB blocks. A sequential walk feeds it files from one
directory at a time, which compress well together; a parallel walk interleaves unrelated
files into the same block. Setting `indexer.threads = 1` restores the old footprint
exactly. Compaction does not recover it — the index is already one segment.

## Queries

Median wall time / tool-reported `query_time_ms` / hits returned, limit 100.

### grav-learn

| Query | 3.5.1 | 4.0.0 | Delta |
|---|---|---|---|
| common word (`page`) | 10.9 ms / 5 ms / 100 | 10.1 ms / 4 ms / 100 | -7.8% |
| rare identifier (`onPageInitialized`) | 7.9 ms / 2 ms / 12 | 7.8 ms / 2 ms / 12 | -1.3% |
| two-word phrase (`page collection`) | 29.8 ms / 23 ms / 91 | 34.0 ms / 28 ms / 91 | +14.0% |
| punctuation (`->`) | 16.4 ms / 10 ms / 100 | 12.0 ms / 6 ms / 100 | **-26.6%** |
| punctuation (`<<<`) | 29.3 ms / 23 ms / 3 | 11.9 ms / 6 ms / 3 | **-59.4%** |
| regex (`function\s+get`) | 16.5 ms / 10 ms / 100 | 16.2 ms / 10 ms / 100 | -1.9% |
| `-e md` | 10.3 ms / 4 ms / **20** | 10.8 ms / 5 ms / **100** | +4.5% |
| `-p pages/` | 11.0 ms / 5 ms / **52** | 12.7 ms / 6 ms / **100** | +15.6% |

### github

| Query | 3.5.1 | 4.0.0 | Delta |
|---|---|---|---|
| common word (`return`) | 16.6 ms / 10 ms / 100 | 14.5 ms / 8 ms / 100 | -12.9% |
| rare identifier (`MarlinSettings`) | 8.1 ms / 2 ms / 10 | 7.2 ms / 1 ms / 10 | -10.9% |
| two-word phrase (`memory allocation`) | 121 ms / 113 ms / 84 | 145 ms / 138 ms / 84 | +19.9% |
| punctuation (`->`) | 14.6 ms / 8 ms / 100 | 10.5 ms / 4 ms / 100 | **-27.8%** |
| punctuation (`<<<`) | 197 ms / 190 ms / 100 | 45 ms / 38 ms / 100 | **-76.9%** |
| regex (`void\s+\w+_init`) | 41.6 ms / 35 ms / 100 | 51.9 ms / 45 ms / 100 | +24.9% |
| `-e c` (`malloc`) | 24.6 ms / 18 ms / **61** | 13.3 ms / 7 ms / **100** | **-46.1%** |
| `-p Marlin/` (`temperature`) | 18.7 ms / 12 ms / **89** | 14.8 ms / 9 ms / **100** | **-20.8%** |

`ygrep` (76 files) is too small to separate anything from process startup: every query
runs in 6-10 ms on both, with `->` and `-p crates/ygrep-core/` about 20% faster on 4.0.0.

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

### The phrase-query tradeoff

Both versions fetch a pool of BM25 candidates and then check each one for the literal.
3.5.1 always fetched `limit × 50`. 4.0.0 fetches `limit × 5` first and escalates only
when the first pass came up short, which makes the common case cheaper.

A query with fewer matches than the limit never fills its page, so it always escalates.
The escalation resumes where the first pass stopped rather than re-reading the pool from
the top, and its deepest step is the same `limit × 50` the fixed pass used, so the
starved case costs one deep pass rather than two:

| github, `memory allocation` (84 matches) | 3.5.1 | 4.0.0 |
|---|---|---|
| `-n 100` (starved) | 113 ms | 138 ms |
| `-n 20` (not starved) | 9 ms | 7 ms |

Before the escalation was made resume-aware the same query cost 227 ms. What is left is
not the extra pass: forcing 4.0.0 to run a single `limit × 50` pass, exactly 3.5.1's
algorithm, measures 134 ms on the same index against the shipped 137 ms. The remainder is
the doc store, which the parallel walk leaves 14% larger and less locally compressed, so
reading 5,000 scattered candidates out of it costs more — the same query against an index
built with `indexer.threads = 1` takes 120 ms. The regex row has the same explanation,
plus a `10×` first pass where 3.5.1 used a flat `100×`.

## Peak RSS

`/usr/bin/time -l`, full build of `github` into a fresh data dir.

| | Peak RSS |
|---|---|
| 3.5.1 | 506 MB |
| 4.0.0 | 504 MB (-0.4%) |
| 4.0.0, `indexer.threads = 1` | 498 MB (-1.6%) |

Parallel walking holds more file contents in flight at once, and the embedding batch no
longer accumulates whole file contents for the entire walk; on this corpus the two cancel
out.

## Anomalies

**Symlinked directories churned on every incremental pass — fixed before release.** A
workspace where two directories are symlinks to the same target indexes the target once,
under one of its aliases. The sequential walk in 3.5.1 always picked the same alias; the
parallel walk picked whichever thread got there first, so the winner changed run to run.
On the `github` corpus (662 symlinks, mostly shared plugin directories) every no-op
`ygrep index` reported 119-256 files indexed and the same number removed, and left a new
segment behind, so the segment count climbed 1 → 4 → 5 over three no-op runs where 3.5.1
stayed at 1.

The walk now steps over links rather than following them, collects them by canonical
target, and walks them in a later pass keeping the lexicographically first alias, so the
choice is identical on every run. Three consecutive no-op passes over the same corpus now
report 0 indexed and 0 removed, and the index stays at one segment — which is what the
segment counts in the table above show.

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
