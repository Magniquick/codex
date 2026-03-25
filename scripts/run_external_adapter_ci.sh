#!/usr/bin/env bash
set -euo pipefail

REPO="${1:-Magniquick/codex}"
WORKFLOW="${2:-external-adapter-e2e.yml}"
REF="${3:-feature/acp-happy-adapter-rust-v0.116.0}"

if ! command -v gh >/dev/null 2>&1; then
  echo "gh CLI is required" >&2
  exit 1
fi

echo "Dispatching workflow ${WORKFLOW} on ${REPO}@${REF}"
gh workflow run "$WORKFLOW" --repo "$REPO" --ref "$REF"

echo "Waiting for the latest workflow run to complete..."
RUN_ID="$(gh run list --repo "$REPO" --workflow "$WORKFLOW" --limit 1 --json databaseId --jq '.[0].databaseId')"

if [[ -z "$RUN_ID" ]]; then
  echo "No run ID found after dispatch" >&2
  exit 1
fi

echo "Watching run ID: $RUN_ID"
gh run watch "$RUN_ID" --repo "$REPO" --exit-status

echo "Run completed. Logs:"
gh run view "$RUN_ID" --repo "$REPO" --log
