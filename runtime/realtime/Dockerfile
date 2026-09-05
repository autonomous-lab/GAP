FROM node:22-alpine
WORKDIR /opt/realtime
COPY package.json package-lock.json ./
RUN npm ci --omit=dev
COPY server.mjs ./
RUN mkdir -p /data && chown node:node /data
USER node
EXPOSE 8091
HEALTHCHECK --interval=10s --timeout=3s --retries=3 \
  CMD wget -qO- http://127.0.0.1:8091/health || exit 1
CMD ["node", "server.mjs"]
