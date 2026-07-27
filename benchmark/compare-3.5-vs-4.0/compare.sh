#!/usr/bin/env bash
#
# Head-to-head benchmark: ygrep 3.5.1 (baseline) vs 4.0.0 (this tree).
#
# Every run uses a throwaway data dir and a throwaway config dir, so the real
# ~/.local/share/ygrep indexes and ~/.config/ygrep/config.toml are never touched.
#
# Usage:
#   ./compare.sh --old <binary> --new <binary> \
#                --corpus name=/path/to/tree [--corpus ...] \
#                [--work <scratch dir>] [--out <results dir>] \
#                [--build-runs N] [--build-warmup N] \
#                [--query-runs N] [--query-warmup N] \
#                [--skip-builds] [--skip-queries] [--skip-rss]
#
# Example:
#   ./compare.sh --old /tmp/ygrep-baseline/target/release/ygrep \
#                --new ../../target/release/ygrep \
#                --corpus ygrep=/Users/me/Projects/ygrep \
#                --corpus grav-learn=/Users/me/Projects/grav/grav-learn \
#                --corpus github=/Users/me/Projects/github
#
# Results land in ./results/raw-<timestamp>.csv plus tables on stdout.

set -o pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

OLD_BIN=""
NEW_BIN=""
CORPORA=()
WORK_DIR="/tmp/ygrep-bench"
OUT_DIR="$SCRIPT_DIR/results"
BUILD_RUNS=3
BUILD_WARMUP=1
QUERY_RUNS=10
QUERY_WARMUP=2
SKIP_BUILDS=0
SKIP_QUERIES=0
SKIP_RSS=0
RSS_CORPUS=""

while [ $# -gt 0 ]; do
    case "$1" in
        --old) OLD_BIN="$2"; shift 2 ;;
        --new) NEW_BIN="$2"; shift 2 ;;
        --corpus) CORPORA+=("$2"); shift 2 ;;
        --work) WORK_DIR="$2"; shift 2 ;;
        --out) OUT_DIR="$2"; shift 2 ;;
        --build-runs) BUILD_RUNS="$2"; shift 2 ;;
        --build-warmup) BUILD_WARMUP="$2"; shift 2 ;;
        --query-runs) QUERY_RUNS="$2"; shift 2 ;;
        --query-warmup) QUERY_WARMUP="$2"; shift 2 ;;
        --rss-corpus) RSS_CORPUS="$2"; shift 2 ;;
        --skip-builds) SKIP_BUILDS=1; shift ;;
        --skip-queries) SKIP_QUERIES=1; shift ;;
        --skip-rss) SKIP_RSS=1; shift ;;
        -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

if [ -z "$OLD_BIN" ] || [ -z "$NEW_BIN" ] || [ ${#CORPORA[@]} -eq 0 ]; then
    echo "error: --old, --new and at least one --corpus are required" >&2
    exit 2
fi

OLD_BIN="$(cd "$(dirname "$OLD_BIN")" && pwd)/$(basename "$OLD_BIN")"
NEW_BIN="$(cd "$(dirname "$NEW_BIN")" && pwd)/$(basename "$NEW_BIN")"

for b in "$OLD_BIN" "$NEW_BIN"; do
    [ -x "$b" ] || { echo "error: not executable: $b" >&2; exit 2; }
done

mkdir -p "$OUT_DIR" "$WORK_DIR"

# Isolated (empty) config dir so both binaries run on built-in defaults.
CONFIG_HOME="$WORK_DIR/config"
mkdir -p "$CONFIG_HOME/ygrep"
export XDG_CONFIG_HOME="$CONFIG_HOME"
unset YGREP_HOME

TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
CSV="$OUT_DIR/raw-$TIMESTAMP.csv"
echo "metric,corpus,variant,label,median_ms,min_ms,max_ms,query_time_ms,hits,runs" > "$CSV"

BOLD='\033[1m'; NC='\033[0m'; CYAN='\033[0;36m'

# ---------------------------------------------------------------- timing core

read -r -d '' PYTIMER <<'PY'
import json, statistics, subprocess, sys, time

runs = int(sys.argv[1])
warm = int(sys.argv[2])
prep = sys.argv[3]
cwd = sys.argv[4]
cmd = sys.argv[5:]

walls, qms = [], []
hits = -1
rc = 0
for i in range(runs + warm):
    if prep:
        subprocess.run(["/bin/bash", "-c", prep], check=False)
    t0 = time.perf_counter()
    p = subprocess.run(cmd, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    dt = (time.perf_counter() - t0) * 1000.0
    rc = p.returncode
    if i < warm:
        continue
    walls.append(dt)
    try:
        d = json.loads(p.stdout.decode("utf-8", "replace"))
    except Exception:
        continue
    if isinstance(d, dict) and "query_time_ms" in d:
        qms.append(float(d["query_time_ms"]))
        hits = len(d.get("hits", []))

print("%.2f %.2f %.2f %s %d %d" % (
    statistics.median(walls), min(walls), max(walls),
    ("%.1f" % statistics.median(qms)) if qms else "-1",
    hits, rc,
))
PY

# timeit <runs> <warmup> <prepare-shell|""> <cwd> <cmd...>
# echoes: median min max query_time_ms hits rc
timeit() {
    python3 -c "$PYTIMER" "$@"
}

# record <metric> <corpus> <variant> <label> <runs> <result line>
record() {
    local metric="$1" corpus="$2" variant="$3" label="$4" runs="$5"
    shift 5
    local med min max qms hits rc
    read -r med min max qms hits rc <<<"$*"
    echo "$metric,$corpus,$variant,\"$label\",$med,$min,$max,$qms,$hits,$runs" >> "$CSV"
    RES_MED="$med"; RES_QMS="$qms"; RES_HITS="$hits"; RES_RC="$rc"
}

delta() { # delta <old> <new> -> percent change, negative = new is faster
    awk -v o="$1" -v n="$2" 'BEGIN { if (o+0 == 0) print "n/a"; else printf "%+.1f%%", (n-o)/o*100 }'
}

row() { printf "%-26s %12s %12s %10s\n" "$1" "$2" "$3" "$4"; }

# ------------------------------------------------------------------- queries
#
# Per-corpus query sets. Each line is: label|arg...|arg...
# Queries are passed after `search` with `--` so leading-dash literals survive.

queries_for() {
    case "$1" in
        ygrep)
            cat <<'EOF'
common word|index
rare identifier|split_subtokens
two-word phrase|search index
punctuation ->|->
punctuation <<<|<<<
regex|--regex|fn\s+search
ext filter (-e rs)|--ext|rs|Searcher
path filter (-p crates/ygrep-core/)|--path|crates/ygrep-core/|index
EOF
            ;;
        grav-learn)
            cat <<'EOF'
common word|page
rare identifier|onPageInitialized
two-word phrase|page collection
punctuation ->|->
punctuation <<<|<<<
regex|--regex|function\s+get
ext filter (-e md)|--ext|md|config
path filter (-p pages/)|--path|pages/|page
EOF
            ;;
        github)
            cat <<'EOF'
common word|return
rare identifier|MarlinSettings
two-word phrase|memory allocation
punctuation ->|->
punctuation <<<|<<<
regex|--regex|void\s+\w+_init
ext filter (-e c)|--ext|c|malloc
path filter (-p Marlin/)|--path|Marlin/|temperature
EOF
            ;;
        *)
            cat <<'EOF'
common word|function
rare identifier|initialize
two-word phrase|error handling
punctuation ->|->
punctuation <<<|<<<
regex|--regex|function\s+get
EOF
            ;;
    esac
}

# Build the argv for one query line: the trailing token is the literal query,
# any leading tokens are flags (--regex / --ext <v> / --path <v>).
query_argv() {
    local IFS='|'
    read -r -a parts <<<"$1"
    local n=${#parts[@]}
    QUERY_ARGS=()
    local i
    for ((i = 1; i < n - 1; i++)); do QUERY_ARGS+=("${parts[$i]}"); done
    QUERY_LITERAL="${parts[$((n - 1))]}"
    QUERY_LABEL="${parts[0]}"
}

# --------------------------------------------------------------------- report

echo -e "${BOLD}ygrep 3.5.1 vs 4.0.0${NC}"
echo "old:  $OLD_BIN"
echo "new:  $NEW_BIN"
echo "host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(uname -s) $(uname -r)"
echo "work: $WORK_DIR"
echo "csv:  $CSV"
echo "build runs: $BUILD_RUNS (+$BUILD_WARMUP warmup)   query runs: $QUERY_RUNS (+$QUERY_WARMUP warmup)"
echo ""

for spec in "${CORPORA[@]}"; do
    name="${spec%%=*}"
    path="${spec#*=}"
    [ -d "$path" ] || { echo "skipping missing corpus: $path" >&2; continue; }

    old_dd="$WORK_DIR/$name-old"
    new_dd="$WORK_DIR/$name-new"

    echo -e "${BOLD}== corpus: $name ($path)${NC}"

    # --- what each binary considers indexable
    for variant in old new; do
        bin="$OLD_BIN"; [ "$variant" = new ] && bin="$NEW_BIN"
        summary=$(cd "$path" && "$bin" --data-dir "$WORK_DIR/dryrun" index --dry-run 2>/dev/null | head -1)
        echo "   dry-run ($variant): $summary"
    done
    rm -rf "$WORK_DIR/dryrun"
    echo ""

    if [ "$SKIP_BUILDS" -eq 0 ]; then
        echo -e "${CYAN}-- indexing${NC}"
        row "workload" "3.5.1" "4.0.0" "delta"

        # cold full build: fresh data dir per iteration
        record build_cold "$name" old "full build (cold)" "$BUILD_RUNS" \
            "$(timeit "$BUILD_RUNS" "$BUILD_WARMUP" "rm -rf '$old_dd'" "$path" "$OLD_BIN" --data-dir "$old_dd" index)"
        o="$RES_MED"
        record build_cold "$name" new "full build (cold)" "$BUILD_RUNS" \
            "$(timeit "$BUILD_RUNS" "$BUILD_WARMUP" "rm -rf '$new_dd'" "$path" "$NEW_BIN" --data-dir "$new_dd" index)"
        n="$RES_MED"
        row "full build (cold)" "${o}ms" "${n}ms" "$(delta "$o" "$n")"

        # leave a warm index in place for the remaining measurements
        (cd "$path" && "$OLD_BIN" --data-dir "$old_dd" index >/dev/null 2>&1)
        (cd "$path" && "$NEW_BIN" --data-dir "$new_dd" index >/dev/null 2>&1)

        record build_rebuild "$name" old "rebuild" "$BUILD_RUNS" \
            "$(timeit "$BUILD_RUNS" "$BUILD_WARMUP" "" "$path" "$OLD_BIN" --data-dir "$old_dd" index --rebuild)"
        o="$RES_MED"
        record build_rebuild "$name" new "rebuild" "$BUILD_RUNS" \
            "$(timeit "$BUILD_RUNS" "$BUILD_WARMUP" "" "$path" "$NEW_BIN" --data-dir "$new_dd" index --rebuild)"
        n="$RES_MED"
        row "rebuild" "${o}ms" "${n}ms" "$(delta "$o" "$n")"

        record build_noop "$name" old "no-op incremental" "$QUERY_RUNS" \
            "$(timeit "$QUERY_RUNS" "$QUERY_WARMUP" "" "$path" "$OLD_BIN" --data-dir "$old_dd" index)"
        o="$RES_MED"
        record build_noop "$name" new "no-op incremental" "$QUERY_RUNS" \
            "$(timeit "$QUERY_RUNS" "$QUERY_WARMUP" "" "$path" "$NEW_BIN" --data-dir "$new_dd" index)"
        n="$RES_MED"
        row "no-op incremental" "${o}ms" "${n}ms" "$(delta "$o" "$n")"
        echo ""

        # --- index footprint
        echo -e "${CYAN}-- index on disk${NC}"
        for variant in old new; do
            dd="$old_dd"; bin="$OLD_BIN"
            [ "$variant" = new ] && { dd="$new_dd"; bin="$NEW_BIN"; }
            idx=$(find "$dd/indexes" -maxdepth 1 -mindepth 1 -type d 2>/dev/null | head -1)
            size=$(du -sh "$idx" 2>/dev/null | cut -f1 | tr -d ' ')
            segs=$(python3 - "$idx/meta.json" <<'PY' 2>/dev/null
import json, sys
try:
    print(len(json.load(open(sys.argv[1]))["segments"]))
except Exception:
    print("?")
PY
)
            files=$("$bin" --data-dir "$dd" indexes list 2>/dev/null | grep -oE '[0-9]+ files' | head -1)
            echo "   $variant: $size on disk, $segs segments, $files"
            echo "index_disk,$name,$variant,\"$size / $segs segments / $files\",0,0,0,-1,-1,1" >> "$CSV"
        done
        echo ""
    fi

    if [ "$SKIP_QUERIES" -eq 0 ]; then
        # make sure both indexes exist even when --skip-builds was used
        (cd "$path" && "$OLD_BIN" --data-dir "$old_dd" index >/dev/null 2>&1)
        (cd "$path" && "$NEW_BIN" --data-dir "$new_dd" index >/dev/null 2>&1)

        echo -e "${CYAN}-- queries (median wall / tool-reported query_time_ms / hits)${NC}"
        printf "%-36s %22s %22s %10s\n" "query" "3.5.1" "4.0.0" "delta"
        while IFS= read -r line; do
            [ -n "$line" ] || continue
            query_argv "$line"
            record query "$name" old "$QUERY_LABEL" "$QUERY_RUNS" \
                "$(timeit "$QUERY_RUNS" "$QUERY_WARMUP" "" "$path" \
                    "$OLD_BIN" --data-dir "$old_dd" --no-auto-index --json search -n 100 \
                    "${QUERY_ARGS[@]}" -- "$QUERY_LITERAL")"
            o="$RES_MED"; oq="$RES_QMS"; oh="$RES_HITS"
            record query "$name" new "$QUERY_LABEL" "$QUERY_RUNS" \
                "$(timeit "$QUERY_RUNS" "$QUERY_WARMUP" "" "$path" \
                    "$NEW_BIN" --data-dir "$new_dd" --no-auto-index --json search -n 100 \
                    "${QUERY_ARGS[@]}" -- "$QUERY_LITERAL")"
            n="$RES_MED"; nq="$RES_QMS"; nh="$RES_HITS"
            printf "%-36s %22s %22s %10s\n" "$QUERY_LABEL" \
                "${o}ms / ${oq}ms / ${oh}h" "${n}ms / ${nq}ms / ${nh}h" "$(delta "$o" "$n")"
        done < <(queries_for "$name")
        echo ""

        # --- how many results each version can actually return once the -e/-p
        #     filter is applied, at the limits people really use
        echo -e "${CYAN}-- filtered result counts${NC}"
        printf "%-36s %8s %12s %12s\n" "query" "limit" "3.5.1" "4.0.0"
        while IFS= read -r line; do
            case "$line" in
                "ext filter"*|"path filter"*) ;;
                *) continue ;;
            esac
            query_argv "$line"
            for lim in 20 100; do
                record filter_hits "$name" old "$QUERY_LABEL -n $lim" 1 \
                    "$(timeit 1 0 "" "$path" \
                        "$OLD_BIN" --data-dir "$old_dd" --no-auto-index --json -n "$lim" search \
                        "${QUERY_ARGS[@]}" -- "$QUERY_LITERAL")"
                oh="$RES_HITS"
                record filter_hits "$name" new "$QUERY_LABEL -n $lim" 1 \
                    "$(timeit 1 0 "" "$path" \
                        "$NEW_BIN" --data-dir "$new_dd" --no-auto-index --json -n "$lim" search \
                        "${QUERY_ARGS[@]}" -- "$QUERY_LITERAL")"
                nh="$RES_HITS"
                printf "%-36s %8s %12s %12s\n" "$QUERY_LABEL" "$lim" "$oh hits" "$nh hits"
            done
        done < <(queries_for "$name")
        echo ""
    fi
done

# ------------------------------------------------------------------ peak RSS

if [ "$SKIP_RSS" -eq 0 ]; then
    if [ -z "$RSS_CORPUS" ]; then
        RSS_CORPUS="${CORPORA[${#CORPORA[@]}-1]}"
    fi
    name="${RSS_CORPUS%%=*}"
    path="${RSS_CORPUS#*=}"
    echo -e "${BOLD}== peak RSS, full build on $name${NC}"
    peak() { # peak <bin> <datadir> <extra env prefix>
        rm -rf "$2"
        ( cd "$path" && /usr/bin/time -l "$1" --data-dir "$2" index ) 2>&1 >/dev/null \
            | awk '/maximum resident set size/ { printf "%.0f", $1/1048576 }'
    }
    o=$(peak "$OLD_BIN" "$WORK_DIR/rss-old")
    n=$(peak "$NEW_BIN" "$WORK_DIR/rss-new")
    # 4.0.0 with the single-threaded fallback (config.indexer.threads = 1)
    printf '[indexer]\nthreads = 1\n' > "$CONFIG_HOME/ygrep/config.toml"
    n1=$(peak "$NEW_BIN" "$WORK_DIR/rss-new1")
    rm -f "$CONFIG_HOME/ygrep/config.toml"
    echo "   3.5.1:                     ${o} MB"
    echo "   4.0.0:                     ${n} MB  ($(delta "$o" "$n"))"
    echo "   4.0.0 (indexer.threads=1): ${n1} MB  ($(delta "$o" "$n1"))"
    echo "peak_rss,$name,old,\"peak RSS MB\",$o,$o,$o,-1,-1,1" >> "$CSV"
    echo "peak_rss,$name,new,\"peak RSS MB\",$n,$n,$n,-1,-1,1" >> "$CSV"
    echo "peak_rss,$name,new-threads1,\"peak RSS MB\",$n1,$n1,$n1,-1,-1,1" >> "$CSV"
    echo ""
fi

echo "raw results: $CSV"
