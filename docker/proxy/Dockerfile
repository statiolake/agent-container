FROM alpine:3.20

RUN apk add --no-cache tinyproxy \
    && mkdir -p /var/run/tinyproxy \
    && chown -R nobody:nobody /var/run/tinyproxy

COPY tinyproxy.conf /etc/tinyproxy/tinyproxy.conf
COPY allowlist.txt /etc/tinyproxy/allowlist.txt

EXPOSE 8888

USER nobody
ENTRYPOINT ["tinyproxy"]
CMD ["-d", "-c", "/etc/tinyproxy/tinyproxy.conf"]
