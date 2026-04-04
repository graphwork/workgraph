#!/usr/bin/env bash
# Pre-pull all Terminal Bench Docker images to avoid Docker Hub rate limiting.
#
# Docker Hub limits anonymous pulls to ~100 per 6 hours (200 for authenticated).
# A full TB run (89 tasks x 3 trials = 267 container starts) will hit this limit
# when images aren't cached locally. This script pulls every unique image once so
# that `docker compose up` uses the local cache instead of pulling.
#
# Strategy: For alexgshaw/* images on Docker Hub, tries GHCR mirror first
# (ghcr.io/laude-institute/terminal-bench/*:2.0) to avoid rate limits, then
# falls back to Docker Hub with retry logic.
#
# Usage:
#   bash terminal-bench/pre-pull-images.sh              # pull all images
#   bash terminal-bench/pre-pull-images.sh --check      # just check which are missing
#   bash terminal-bench/pre-pull-images.sh --login       # docker login first, then pull
#
# Run this BEFORE any `harbor run` invocation.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
HARBOR_CACHE="${HARBOR_TASKS_DIR:-$HOME/.cache/harbor/tasks}"

# ── Parse args ───────────────────────────────────────────────────────────────
CHECK_ONLY=false
DO_LOGIN=false
RETRY_WAIT=60
MAX_RETRIES=3
NO_GHCR_FALLBACK=false

while [[ $# -gt 0 ]]; do
    case "$1" in
        --check)            CHECK_ONLY=true; shift ;;
        --login)            DO_LOGIN=true; shift ;;
        --retry-wait)       RETRY_WAIT="$2"; shift 2 ;;
        --max-retries)      MAX_RETRIES="$2"; shift 2 ;;
        --no-ghcr-fallback) NO_GHCR_FALLBACK=true; shift ;;
        -h|--help)
            echo "Usage: $0 [--check] [--login] [--retry-wait SECS] [--no-ghcr-fallback]"
            echo ""
            echo "Pre-pull all Terminal Bench Docker images to local cache."
            echo ""
            echo "Options:"
            echo "  --check             Only report which images are missing (don't pull)"
            echo "  --login             Run 'docker login' before pulling (recommended)"
            echo "  --retry-wait S      Seconds to wait between retries (default: 60)"
            echo "  --max-retries N     Max retries per image (default: 3)"
            echo "  --no-ghcr-fallback  Don't use ghcr.io as fallback for Docker Hub images"
            echo ""
            echo "Environment:"
            echo "  HARBOR_TASKS_DIR  Override Harbor task cache location"
            echo "                    (default: ~/.cache/harbor/tasks)"
            echo ""
            echo "Rate limit info:"
            echo "  Anonymous Docker Hub: ~100 pulls / 6 hours"
            echo "  Authenticated Docker Hub: ~200 pulls / 6 hours"
            echo "  GHCR (ghcr.io): No rate limit for public images"
            echo ""
            echo "The script automatically tries GHCR first for alexgshaw/* images,"
            echo "avoiding Docker Hub rate limits entirely for most images."
            exit 0
            ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

# ── Docker login (if requested) ─────────────────────────────────────────────
if [[ "$DO_LOGIN" == true ]]; then
    echo "Authenticating with Docker Hub (doubles rate limit to 200/6h)..."
    docker login
    echo ""
fi

# ── Check Docker auth status ────────────────────────────────────────────────
if docker system info 2>/dev/null | grep -q "Username:"; then
    echo "Docker Hub: authenticated (200 pulls/6h limit)"
else
    echo "Docker Hub: anonymous (100 pulls/6h limit)"
    echo "  Tip: run with --login to authenticate and double your rate limit."
fi
echo ""

# ── Extract unique image names from task configs ─────────────────────────────
if [[ ! -d "$HARBOR_CACHE" ]]; then
    echo "ERROR: Harbor task cache not found at $HARBOR_CACHE"
    echo "Run 'harbor download terminal-bench@2.0' first to populate the task cache."
    exit 1
fi

echo "Scanning task configs in $HARBOR_CACHE ..."

IMAGES=()
while IFS= read -r img; do
    [[ -n "$img" ]] && IMAGES+=("$img")
done < <(
    for dir in "$HARBOR_CACHE"/*/; do
        task=$(ls "$dir" 2>/dev/null | head -1)
        toml="$dir/$task/task.toml"
        if [[ -f "$toml" ]]; then
            grep '^docker_image' "$toml" 2>/dev/null | sed 's/.*= *"//;s/".*//'
        fi
    done | sort -u
)

# Also add base images used in Dockerfiles (for tasks that build locally)
BASE_IMAGES=()
while IFS= read -r img; do
    [[ -n "$img" ]] && BASE_IMAGES+=("$img")
done < <(
    for dir in "$HARBOR_CACHE"/*/; do
        task=$(ls "$dir" 2>/dev/null | head -1)
        dockerf="$dir/$task/environment/Dockerfile"
        if [[ -f "$dockerf" ]]; then
            grep -i '^FROM' "$dockerf" | sed 's/FROM *//;s/ *[Aa][Ss] .*//;s/ *$//' \
                | grep -v '^\$' | grep -v '^--platform'
        fi
    done | sort -u
)

# Merge and deduplicate
ALL_IMAGES=($(printf '%s\n' "${IMAGES[@]}" "${BASE_IMAGES[@]}" | sort -u))

echo "Found ${#ALL_IMAGES[@]} unique images (${#IMAGES[@]} prebuilt + ${#BASE_IMAGES[@]} base)"
echo ""

# ── Check which images are already cached ────────────────────────────────────
CACHED=()
MISSING=()

for img in "${ALL_IMAGES[@]}"; do
    if docker image inspect "$img" &>/dev/null; then
        CACHED+=("$img")
    else
        MISSING+=("$img")
    fi
done

echo "Already cached: ${#CACHED[@]}"
echo "Need to pull:   ${#MISSING[@]}"
echo ""

if [[ ${#MISSING[@]} -eq 0 ]]; then
    echo "All images are cached locally. Ready to run."
    exit 0
fi

if [[ "$CHECK_ONLY" == true ]]; then
    echo "Missing images:"
    for img in "${MISSING[@]}"; do
        echo "  $img"
    done
    exit 0
fi

# ── Helper: try GHCR mirror for alexgshaw/* images ──────────────────────────
# Terminal Bench images exist on both Docker Hub (alexgshaw/*:20251031) and
# GHCR (ghcr.io/laude-institute/terminal-bench/*:2.0). GHCR has no rate limit
# for public images, so we try it first to avoid Docker Hub throttling.
ghcr_mirror_for() {
    local img="$1"
    if [[ "$img" == alexgshaw/* ]]; then
        local task_name="${img#alexgshaw/}"
        task_name="${task_name%%:*}"
        echo "ghcr.io/laude-institute/terminal-bench/${task_name}:2.0"
    fi
}

try_pull_with_ghcr_fallback() {
    local img="$1"
    local ghcr_img

    # Try GHCR mirror first for alexgshaw/* images
    if [[ "$NO_GHCR_FALLBACK" == false ]]; then
        ghcr_img=$(ghcr_mirror_for "$img")
        if [[ -n "$ghcr_img" ]]; then
            if docker pull "$ghcr_img" &>/dev/null && docker image inspect "$ghcr_img" &>/dev/null; then
                docker tag "$ghcr_img" "$img"
                echo "  (pulled from GHCR mirror, tagged as $img)"
                return 0
            fi
        fi
    fi

    # Direct pull with retries
    for attempt in $(seq 1 "$MAX_RETRIES"); do
        if docker pull "$img" 2>&1 | tail -1; then
            if docker image inspect "$img" &>/dev/null; then
                return 0
            fi
        fi

        if [[ $attempt -lt $MAX_RETRIES ]]; then
            echo "  Attempt $attempt/$MAX_RETRIES failed. Waiting ${RETRY_WAIT}s before retry..."
            sleep "$RETRY_WAIT"
        fi
    done

    return 1
}

# ── Pull missing images ─────────────────────────────────────────────────────
echo "Pulling ${#MISSING[@]} images ..."
echo "(Trying GHCR mirror first for Docker Hub images to avoid rate limits)"
echo ""

PULLED=0
TOTAL=${#MISSING[@]}
FAILED_FINAL=()
IDX=0

for img in "${MISSING[@]}"; do
    IDX=$((IDX + 1))
    echo "[$IDX/$TOTAL] Pulling $img ..."

    if try_pull_with_ghcr_fallback "$img"; then
        PULLED=$((PULLED + 1))
    else
        echo "  FAILED: $img"
        FAILED_FINAL+=("$img")
    fi
done

echo ""
echo "═══════════════════════════════════════════════════════════════"
echo "  Pre-pull Summary"
echo "═══════════════════════════════════════════════════════════════"
echo "  Total images:   ${#ALL_IMAGES[@]}"
echo "  Already cached: ${#CACHED[@]}"
echo "  Pulled now:     $PULLED"
echo "  Failed:         ${#FAILED_FINAL[@]}"
echo "═══════════════════════════════════════════════════════════════"

if [[ ${#FAILED_FINAL[@]} -gt 0 ]]; then
    echo ""
    echo "Failed images:"
    for img in "${FAILED_FINAL[@]}"; do
        echo "  $img"
    done
    echo ""
    echo "To fix:"
    echo "  1. Authenticate with Docker Hub: docker login"
    echo "  2. Re-run this script: bash $0"
    echo "  3. Or wait for rate limit reset (~6h) and re-run"
    exit 1
fi

echo ""
echo "All images cached. Ready to run Terminal Bench."
