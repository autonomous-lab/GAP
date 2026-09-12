FROM alpine:3.23
RUN apk add --no-cache python3 && adduser -D -u 10001 control
WORKDIR /opt/control
COPY runtime/control/*.py ./
USER 10001:10001
ENTRYPOINT ["python3", "service.py"]
CMD ["--config", "/config/control.json", "--bind", "0.0.0.0", "--port", "8096"]
