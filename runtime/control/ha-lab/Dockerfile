FROM postgres:17.6-bookworm
RUN apt-get update && apt-get install -y --no-install-recommends python3 python3-pip && rm -rf /var/lib/apt/lists/*
RUN pip3 install --break-system-packages --no-cache-dir 'patroni[etcd3]==4.1.5' 'psycopg[binary]==3.2.10'
USER postgres
ENTRYPOINT ["patroni"]
