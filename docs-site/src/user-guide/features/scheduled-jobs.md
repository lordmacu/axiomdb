# Scheduled Jobs

AxiomDB includes a built-in cron scheduler, compatible with the `pg_cron`
SQL API. Jobs are persisted in the catalog and executed by a background task
inside the server process — no external process or daemon required.

## Quick start

```sql
-- Run DELETE every day at midnight
SELECT cron_schedule('nightly_cleanup', '@daily',
    'DELETE FROM logs WHERE ts < NOW() - INTERVAL 30 DAY');

-- Run a refresh every hour at :00
SELECT cron_schedule('hourly_refresh', '0 * * * *', 'CALL refresh_stats()');

-- See all scheduled jobs
SELECT * FROM information_schema.scheduled_jobs;

-- Pause a job without deleting it
SELECT cron_disable('hourly_refresh');

-- Resume it
SELECT cron_enable('hourly_refresh');

-- Remove permanently
SELECT cron_unschedule('nightly_cleanup');
```

## Functions

### `cron_schedule(name, schedule, command)`

Registers or updates a scheduled job. Returns the job name on success.

| Parameter  | Type | Description |
|------------|------|-------------|
| `name`     | TEXT | Unique job identifier |
| `schedule` | TEXT | Cron expression or alias (see below) |
| `command`  | TEXT | SQL statement to execute |

If a job with the same name already exists it is replaced (upsert semantics).
The job runs in the database context active when `cron_schedule` is called.

### `cron_unschedule(name)`

Removes the named job. Returns `1` if the job existed, `0` otherwise.

### `cron_enable(name)` / `cron_disable(name)`

Resumes or pauses a job without removing it. Returns `1` on success, or
raises an error if the job does not exist.

## Schedule expressions

AxiomDB accepts standard 5-field cron expressions and the common `@`-aliases.

### 5-field format

```
MIN HOUR DOM MONTH DOW
 │    │    │    │    └── day of week (0-7, 0 and 7 = Sunday)
 │    │    │    └─────── month (1-12)
 │    │    └──────────── day of month (1-31)
 │    └───────────────── hour (0-23)
 └────────────────────── minute (0-59)
```

Each field accepts:

| Syntax  | Example     | Meaning                      |
|---------|-------------|------------------------------|
| `*`     | `*`         | every value                  |
| `N`     | `5`         | exact value                  |
| `N-M`   | `9-17`      | inclusive range              |
| `*/N`   | `*/15`      | every N-th value             |
| `N,M`   | `1,15`      | comma-separated list         |

### `@` aliases

| Alias        | Equivalent      | When it fires              |
|--------------|-----------------|----------------------------|
| `@hourly`    | `0 * * * *`     | top of every hour          |
| `@daily`     | `0 0 * * *`     | midnight every day         |
| `@midnight`  | `0 0 * * *`     | same as `@daily`           |
| `@weekly`    | `0 0 * * 0`     | Sunday at midnight         |
| `@monthly`   | `0 0 1 * *`     | 1st of month at midnight   |
| `@yearly`    | `0 0 1 1 *`     | Jan 1st at midnight        |
| `@annually`  | `0 0 1 1 *`     | same as `@yearly`          |

## `information_schema.scheduled_jobs`

```sql
SELECT JOB_NAME, SCHEDULE, COMMAND, DATABASE_NAME,
       ENABLED, NEXT_RUN, LAST_RUN, LAST_STATUS
FROM information_schema.scheduled_jobs;
```

| Column          | Type | Description |
|-----------------|------|-------------|
| `JOB_NAME`      | TEXT | Unique identifier |
| `SCHEDULE`      | TEXT | Cron expression as stored |
| `COMMAND`       | TEXT | SQL command |
| `DATABASE_NAME` | TEXT | Database the command runs in |
| `ENABLED`       | TEXT | `YES` or `NO` |
| `NEXT_RUN`      | TEXT | Next scheduled fire time (UTC) |
| `LAST_RUN`      | TEXT | Last fire time (UTC), empty if never run |
| `LAST_STATUS`   | TEXT | `ok` or error message from last run |

## How it works

The scheduler runs as a background tokio task launched at server startup. Every
minute it:

1. Opens a catalog snapshot and lists all enabled jobs.
2. Fires any job whose `next_run_ms ≤ now` (or `next_run_ms = 0` for new jobs).
3. Executes the job's SQL in a fresh `SessionContext` scoped to the job's target
   database.
4. Records `last_run_ms`, `next_run_ms`, and `last_status` back to the catalog
   in an ACID mini-transaction.
5. Sleeps to the next minute boundary.

Jobs are stored in the `axiom_cron_jobs` system heap (meta page offset 160).
The heap is upgraded lazily on open, so existing databases gain job support
transparently.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">No external dependency</span>
The cron expression parser is implemented natively in AxiomDB — no external
cron library is used in the executor path. The scheduler uses only <code>tokio</code>
and <code>chrono</code>, which are already present in the server runtime.
</div>
</div>
