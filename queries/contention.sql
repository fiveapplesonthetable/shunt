-- Question: During what periods did more than one agent's Build or Test slices overlap?
-- Columns:
--   ts: Start timestamp of the contention period in nanoseconds.
--   dur: Duration of the contention period in nanoseconds.
--   active_builds: Number of concurrent build/test operations running in this interval.
--   max_load1_milli: Maximum 1-minute load average in millicpus (load * 1000) during this contention interval.
INCLUDE PERFETTO MODULE intervals.intersect;

CREATE PERFETTO TABLE build_slices AS
SELECT s.id, s.ts, s.dur
FROM slice s
WHERE (s.name IN ('Build', 'Test') OR s.name LIKE 'Build:%' OR s.name LIKE 'Test:%') AND s.dur > 0;

CREATE PERFETTO TABLE build_endpoints AS
SELECT ts, 1 AS delta FROM build_slices
UNION ALL
SELECT ts + dur AS ts, -1 AS delta FROM build_slices;

CREATE PERFETTO TABLE boundary_deltas AS
SELECT ts, SUM(delta) AS delta
FROM build_endpoints
GROUP BY ts;

CREATE PERFETTO TABLE boundary_sweep AS
SELECT
  ts,
  LEAD(ts) OVER (ORDER BY ts) - ts AS dur,
  SUM(delta) OVER (ORDER BY ts ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS active_builds
FROM boundary_deltas;

CREATE PERFETTO TABLE contention_intervals AS
SELECT
  row_number() OVER () AS id,
  ts,
  dur,
  active_builds
FROM boundary_sweep
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

CREATE PERFETTO TABLE contention_load_intersections AS
SELECT
  c.id AS contention_id,
  l.load1_milli
FROM _interval_intersect!((contention_intervals, load_intervals), ()) i
JOIN contention_intervals c ON i.id_0 = c.id
JOIN load_intervals l ON i.id_1 = l.id;

SELECT
  c.ts,
  c.dur,
  c.active_builds,
  COALESCE(MAX(cli.load1_milli), 0) AS max_load1_milli
FROM contention_intervals c
LEFT JOIN contention_load_intersections cli ON c.id = cli.contention_id
GROUP BY c.id, c.ts, c.dur, c.active_builds
ORDER BY c.ts;
