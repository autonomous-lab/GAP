FROM nginx:1.27-alpine
COPY nginx.conf /etc/nginx/nginx.conf
USER nginx
EXPOSE 8080
HEALTHCHECK --interval=10s --timeout=3s --retries=6 \
  CMD wget -qO- http://127.0.0.1:8080/edge-health || exit 1
ENTRYPOINT ["nginx", "-g", "daemon off;"]
