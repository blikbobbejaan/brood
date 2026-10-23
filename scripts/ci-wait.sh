#!/usr/bin/env bash
# ci-wait.sh [REF] — wait for CI's verdict on REF (default HEAD) and say what failed.
#
# `make green` reads the verdicts CI has already reached; this waits for one. It exists
# because the hand-written watch loop it replaces went wrong three ways in one day
# (2026-10-23), each costing a round of "is it green yet?":
#
#   - `gh run list -L 1` right after a push returns the PREVIOUS run (the new one is not
#     registered yet), so the loop watched the wrong commit — or, with no id, nothing at all.
#   - every push to main cancels the run before it (the workflow's concurrency group), so a
#     busy morning yields a chain of `cancelled`, none of them a verdict on anything.
#   - a run on a LATER commit is a verdict on REF too, when REF is its ancestor; waiting for
#     REF's own run to be cancelled and stopping there threw that verdict away.
#
# So: follow the newest CI run on main whose commit CONTAINS REF, through cancellations, until
# one completes; then print its conclusion, the jobs that failed and the tests that failed in
# them. Exit 0 on success, 1 on failure, 2 when no run containing REF appears.
set -uo pipefail
cd "$(git rev-parse --show-toplevel)" || exit 2

WORKFLOW=CI
BRANCH=main
ref=${1:-HEAD}
sha=$(git rev-parse "$ref") || exit 2
short=${sha:0:8}

contains_ref() { # $1 = a run's head sha — does that commit contain REF?
    [ "$1" = "$sha" ] && return 0
    git cat-file -e "$1^{commit}" 2>/dev/null || git fetch -q origin "$BRANCH" 2>/dev/null
    git merge-base --is-ancestor "$sha" "$1" 2>/dev/null
}

newest_containing() { # prints "id headsha status conclusion" of the newest run containing REF
    gh run list --workflow "$WORKFLOW" --branch "$BRANCH" --limit 20 \
        --json databaseId,headSha,status,conclusion \
        -q '.[] | "\(.databaseId) \(.headSha) \(.status) \(.conclusion)"' 2>/dev/null |
        while read -r id head status conclusion; do
            if contains_ref "$head"; then
                echo "$id $head $status $conclusion"
                return
            fi
        done
}

echo "ci-wait: waiting for a $WORKFLOW verdict on $short ($(git log -1 --format=%s "$sha" | cut -c1-60))" >&2
deadline=$(( $(date +%s) + 3 * 3600 ))
seen=""
while :; do
    [ "$(date +%s)" -gt "$deadline" ] && { echo "ci-wait: no verdict within 3 h" >&2; exit 2; }
    row=$(newest_containing)
    if [ -z "$row" ]; then
        sleep 20 # just pushed: the run is not registered yet
        continue
    fi
    read -r id head status conclusion <<<"$row"
    if [ "$id" != "$seen" ]; then
        echo "ci-wait: following run $id on ${head:0:8}" >&2
        seen=$id
    fi
    if [ "$status" != "completed" ]; then
        gh run watch "$id" --interval 60 >/dev/null 2>&1
        continue
    fi
    if [ "$conclusion" = "cancelled" ]; then
        sleep 20 # superseded: the run that cancelled it contains REF too
        continue
    fi
    break
done

echo "$WORKFLOW on ${head:0:8} (contains $short): $conclusion"
[ "$conclusion" = "success" ] && exit 0
gh run view "$id" --json jobs -q '.jobs[] | select(.conclusion != "success" and .conclusion != "skipped") | "  job: \(.conclusion) \(.name)"'
# The failing tests, as nextest and the Brood runner name them. The log is ANSI-coloured
# (KI-39): strip that first, or no pattern matches.
gh run view "$id" --log-failed 2>/dev/null |
    sed 's/\x1b\[[0-9;]*m//g' |
    grep -oE '(FAIL|TIMEOUT) \[[^]]*\] \([^)]*\) [^ ]+ [^ ]+|[^ ]+\.blsp:[0-9:]+: test failed: .*' |
    sed -E 's/^(FAIL|TIMEOUT) \[[^]]*\] \([^)]*\) /  \1 /; s/^([^ ]+\.blsp)/  \1/' | sort -u | head -30
echo "  full log: gh run view $id --log-failed"
exit 1
