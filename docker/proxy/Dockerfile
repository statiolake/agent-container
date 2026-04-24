FROM alpine:3.20

RUN apk add --no-cache tinyproxy \
    && mkdir -p /var/run/tinyproxy \
    && chown -R nobody:nobody /var/run/tinyproxy

COPY tinyproxy.conf /etc/tinyproxy/tinyproxy.conf
# /etc/tinyproxy/allowlist.txt is bind-mounted at runtime by the CLI
# (see proxy_allowlist::generate) — nothing is baked into the image.

EXPOSE 8888

USER nobody
ENTRYPOINT ["tinyproxy"]
CMD ["-d", "-c", "/etc/tinyproxy/tinyproxy.conf"]
