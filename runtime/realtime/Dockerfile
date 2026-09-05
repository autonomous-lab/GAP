FROM node:22-alpine
WORKDIR /opt/realtime
COPY package.json package-lock.json ./
RUN apk add --no-cache su-exec && npm ci --omit=dev
COPY server.mjs ./
COPY entrypoint.sh ./
RUN mkdir -p /data && chmod 755 entrypoint.sh
EXPOSE 8091
HEALTHCHECK --interval=10s --timeout=3s --retries=3 \
  CMD wget -qO- http://127.0.0.1:8091/health || exit 1
ENTRYPOINT ["/opt/realtime/entrypoint.sh"]
