FROM node:22-bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        jq \
        less \
        ncurses-term \
        procps \
        python-is-python3 \
        python3 \
        python3-openpyxl \
        ripgrep \
        tini \
        tmux \
        vim \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code@latest @openai/codex \
    && npm cache clean --force

RUN curl -fsSL https://cursor.com/install | HOME=/opt/cursor-agent bash \
    && chmod -R a+rX /opt/cursor-agent/.local \
    && ln -sf /opt/cursor-agent/.local/bin/cursor-agent /usr/local/bin/cursor-agent \
    && printf '%s\n' \
        '#!/usr/bin/env bash' \
        'set -euo pipefail' \
        'if [ "${1:-}" = "agent" ]; then' \
        '  shift' \
        '  exec cursor-agent "$@"' \
        'fi' \
        'echo "Only Cursor Agent is available in this container; use: cursor agent ..." >&2' \
        'exit 64' \
        > /usr/local/bin/cursor \
    && chmod +x /usr/local/bin/cursor

# Writable home for an arbitrary UID passed via `docker run --user`.
# Pre-create /home/agent/.claude so that bind-mounts of its children (e.g.
# .credentials.json, project history, CLAUDE.md) don't leave the parent
# owned by root and blocking Claude Code from creating sibling dirs like
# session-env at runtime.
RUN mkdir -p /home/agent/.claude /home/agent/.codex /home/agent/.cursor \
    && chmod -R 1777 /home/agent

ENV HOME=/home/agent
ENV NODE_OPTIONS=--max-old-space-size=4096
# Claude Code opens $EDITOR for Ctrl+G prompt editing; pick vim by default
# so pressing the shortcut actually lands in a working editor.
ENV EDITOR=vim
ENV VISUAL=vim
# Default to C.UTF-8 so vim and other tools handle multi-byte input
# (Japanese, emoji, …) correctly. Without a UTF-8 locale libc falls back
# to POSIX/ASCII and every non-ASCII character gets garbled on display.
ENV LANG=C.UTF-8
ENV LC_ALL=C.UTF-8

WORKDIR /home/agent

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["claude", "--permission-mode", "bypassPermissions"]
