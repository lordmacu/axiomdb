FROM mysql:8.0
# MySQL 8.0 runs on Oracle Linux 9 — uses microdnf
RUN microdnf install -y python3 python3-pip && \
    pip3 install pymysql && \
    microdnf clean all
RUN mkdir -p /bench
COPY bench.py /bench/bench.py
