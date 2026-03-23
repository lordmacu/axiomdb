-- MySQL 8.0 benchmark script
-- Run with: mysql -h127.0.0.1 -P3307 -uroot -pbench bench < bench_mysql.sql

DROP TABLE IF EXISTS users;
CREATE TABLE users (
    id     INT NOT NULL,
    name   VARCHAR(255),
    age    INT,
    active TINYINT(1)
) ENGINE=InnoDB;

-- Measure: sequential INSERT (autocommit, 1K rows)
SET @start = NOW(6);
INSERT INTO users VALUES (1,'user1',25,1);
INSERT INTO users VALUES (2,'user2',26,1);
-- ... (script generates these programmatically via bench_runner.py)

SELECT CONCAT('INSERT 1K rows: ', TIMESTAMPDIFF(MICROSECOND, @start, NOW(6)), ' µs') AS result;

-- Measure: batch INSERT (single transaction, 1K rows)
SET @start = NOW(6);
START TRANSACTION;
-- ... (inserts)
COMMIT;
SELECT CONCAT('Batch INSERT 1K (1 txn): ', TIMESTAMPDIFF(MICROSECOND, @start, NOW(6)), ' µs') AS result;

-- Measure: SELECT * (full scan)
SET @start = NOW(6);
SELECT * FROM users;
SELECT CONCAT('SELECT * 1K rows: ', TIMESTAMPDIFF(MICROSECOND, @start, NOW(6)), ' µs') AS result;

-- Measure: SELECT with WHERE
SET @start = NOW(6);
SELECT * FROM users WHERE age = 30;
SELECT CONCAT('SELECT WHERE age=30: ', TIMESTAMPDIFF(MICROSECOND, @start, NOW(6)), ' µs') AS result;

-- Measure: COUNT(*)
SET @start = NOW(6);
SELECT COUNT(*) FROM users;
SELECT CONCAT('SELECT COUNT(*): ', TIMESTAMPDIFF(MICROSECOND, @start, NOW(6)), ' µs') AS result;
