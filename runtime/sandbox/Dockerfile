FROM node:22.14.0-alpine

WORKDIR /opt/sandbox
COPY server.mjs worker.mjs ./

USER node
EXPOSE 8090
HEALTHCHECK --interval=10s --timeout=2s --retries=6 \
    CMD wget -qO- http://127.0.0.1:8090/health || exit 1

ENTRYPOINT ["node", "/opt/sandbox/server.mjs"]
