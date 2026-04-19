#!/bin/sh
# Fetch fresh AWS credentials from the agent-container host broker and write
# them into ~/.aws/credentials under the [bedrock] profile. Claude Code
# invokes this as awsAuthRefresh whenever its AWS calls hit auth errors.
set -eu

endpoint="${AGENT_CONTAINER_HOST_ENDPOINT:?AGENT_CONTAINER_HOST_ENDPOINT is not set}"
creds_file="${AWS_SHARED_CREDENTIALS_FILE:-$HOME/.aws/credentials}"

mkdir -p "$(dirname "$creds_file")"
tmp="${creds_file}.tmp.$$"

if ! curl -fsS --max-time 15 "$endpoint/aws/credentials" > "$tmp"; then
    rm -f "$tmp"
    echo "agent-container-aws-refresh: failed to fetch credentials from $endpoint" >&2
    exit 1
fi

mv "$tmp" "$creds_file"
chmod 600 "$creds_file"
