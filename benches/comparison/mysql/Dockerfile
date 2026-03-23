FROM mysql:8.0
RUN apt-get update && apt-get install -y --no-install-recommends \
    python3 python3-pip && \
    pip3 install pymysql --break-system-packages 2>/dev/null || \
    pip3 install pymysql && \
    rm -rf /var/lib/apt/lists/*
COPY bench.py /bench/bench.py
