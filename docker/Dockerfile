FROM node:22-bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        git \
        jq \
        less \
        procps \
        ripgrep \
        tini \
        vim \
    && rm -rf /var/lib/apt/lists/*

RUN npm install -g @anthropic-ai/claude-code@latest @openai/codex \
    && npm cache clean --force

COPY agent-container-aws-refresh.sh /usr/local/bin/agent-container-aws-refresh
RUN chmod 0755 /usr/local/bin/agent-container-aws-refresh

# Writable home for an arbitrary UID passed via `docker run --user`.
# Pre-create /home/agent/.claude so that bind-mounts of its children (e.g.
# .credentials.json, projects/-workspace, CLAUDE.md) don't leave the parent
# owned by root and blocking Claude Code from creating sibling dirs like
# session-env at runtime.
RUN mkdir -p /home/agent/.claude /home/agent/.codex \
    && chmod -R 1777 /home/agent \
    && mkdir -p /workspace \
    && chmod 1777 /workspace

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

WORKDIR /workspace

ENTRYPOINT ["/usr/bin/tini", "--"]
CMD ["claude", "--dangerously-skip-permissions"]
