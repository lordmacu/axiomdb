FROM postgres:16
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 python3-pip && \
    pip3 install psycopg2-binary --break-system-packages 2>/dev/null || \
    pip3 install psycopg2-binary && \
    rm -rf /var/lib/apt/lists/*
COPY bench.py /bench/bench.py
