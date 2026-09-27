#!/usr/bin/env bash
# Regenerates CHANGELOG.md from the commit log: one section per tag (newest first), plus
# "Unreleased" for commits after the latest tag. Same shape as openmw_config's changelog.
set -euo pipefail

OUTPUT="${1:-CHANGELOG.md}"

emit_commits() {
    git log "$1" --format="format:%h %s" | while read -r hash msg; do
        echo "- $hash - $msg"
    done
}

{
    echo "# Changelog"
    echo ""
    mapfile -t tags < <(git tag --sort=-version:refname 2>/dev/null)
    if [ "${#tags[@]}" -eq 0 ]; then
        echo "## Unreleased"
        echo ""
        emit_commits HEAD
        echo ""
    else
        if [ -n "$(git log "${tags[0]}..HEAD" --format="format:%h")" ]; then
            echo "## Unreleased"
            echo ""
            emit_commits "${tags[0]}..HEAD"
            echo ""
        fi
        for i in "${!tags[@]}"; do
            tag="${tags[$i]}"
            echo "## $tag"
            echo ""
            if [ $((i + 1)) -lt "${#tags[@]}" ]; then
                emit_commits "${tags[$((i + 1))]}..$tag"
            else
                emit_commits "$tag"
            fi
            echo ""
        done
    fi
} > "$OUTPUT"
echo "wrote $OUTPUT"
