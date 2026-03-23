FROM postgres:16
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 python3-psycopg2 && \
    rm -rf /var/lib/apt/lists/*
RUN mkdir -p /bench
COPY bench.py /bench/bench.py
