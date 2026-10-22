-- Question: During what periods did more than one agent's Build or Test slices overlap?
-- Columns:
--   ts: Start timestamp of the contention period in nanoseconds.
--   dur: Duration of the contention period in nanoseconds.
--   active_builds: Number of concurrent build/test operations running in this interval.
--   max_load1_milli: Maximum 1-minute load average in millicpus (load * 1000) during this contention interval.
PERFETTO PRAGMA pipelines = 1;

CREATE PERFETTO TABLE build_slices AS
SELECT s.ts, s.dur
FROM slice s
WHERE (s.name IN ('Build', 'Test') OR s.name LIKE 'Build:%' OR s.name LIKE 'Test:%') AND s.dur > 0;

CREATE PERFETTO TABLE flattened AS
FROM build_slices
|> INTERVAL FLATTEN AGGREGATE COUNT(*) AS active_builds;

CREATE PERFETTO TABLE contention_intervals AS
SELECT
  row_number() OVER () AS id,
  ts,
  dur,
  active_builds
FROM flattened
WHERE active_builds >= 2 AND dur > 0;

CREATE PERFETTO TABLE load_samples AS
SELECT
  c.ts,
  CAST(c.value AS INT) AS load1_milli
FROM counter c
JOIN counter_track ct ON c.track_id = ct.id
WHERE ct.name = 'machine/load1' OR ct.name = 'load1';

CREATE PERFETTO TABLE load_intervals AS
SELECT
  row_number() OVER () AS id,
  ts,
  MAX(1, COALESCE(LEAD(ts) OVER (ORDER BY ts) - ts, (SELECT end_ts FROM trace_bounds) - ts, 1000000000)) AS dur,
  load1_milli
FROM load_samples;

CREATE PERFETTO TABLE raw_contention_load AS
INTERVAL INTERSECTION OF (contention_intervals AS c, load_intervals AS l)
|> SELECT c.id AS contention_id, l.load1_milli;

SELECT
  c.ts,
  c.dur,
  c.active_builds,
  COALESCE(MAX(cli.load1_milli), 0) AS max_load1_milli
FROM contention_intervals c
LEFT JOIN raw_contention_load cli ON c.id = cli.contention_id
GROUP BY c.id, c.ts, c.dur, c.active_builds
ORDER BY c.ts;
