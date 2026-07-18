#!/usr/bin/env bash
set -euo pipefail
# Opt-in only. The central AWS SDK adapter must be enabled before this live gate.
: "${SYNAPS_AWS_LIVE_TEST:?set SYNAPS_AWS_LIVE_TEST=1 to acknowledge a live AWS call}"
[[ "$SYNAPS_AWS_LIVE_TEST" == 1 ]] || { echo 'live AWS smoke disabled'; exit 2; }
for v in SYNAPS_AWS_SSO_START_URL SYNAPS_AWS_SSO_REGION SYNAPS_AWS_ACCOUNT_ID SYNAPS_AWS_ROLE_NAME SYNAPS_AWS_BEDROCK_REGION SYNAPS_AWS_MODEL_ID; do
  [[ -n "${!v:-}" ]] || { echo "missing $v"; exit 2; }
done
# Never echo environment values: they include account context and may be sensitive.
echo 'AWS Bedrock live smoke requested; secret-safe evidence only.'
CARGO_BUILD_JOBS=8 cargo test -p synaps-core --test aws_bedrock_live -- --ignored --test-threads=1
