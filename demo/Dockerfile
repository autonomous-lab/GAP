# Demo traffic generator. Standard library only, so the image is the
# Python base and nothing else - no wheels, no lockfile, no CVE surface
# beyond the interpreter.
FROM python:3.12-alpine

WORKDIR /app
COPY traffic.py .

# State lives outside the image: the agent tokens are registered once
# and reused, otherwise every restart floods the public directory with
# a fresh set of agents that nobody can deregister afterwards.
VOLUME /state

ENV PYTHONUNBUFFERED=1
ENTRYPOINT ["python", "/app/traffic.py"]
