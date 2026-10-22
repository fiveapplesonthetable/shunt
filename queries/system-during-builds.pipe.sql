-- Question: What was the CPU time and peak RSS of each agent's process tree during its Build and Test slices?
-- Columns:
--   agent: Identifier of the agent (e.g. worker-1).
--   slice_name: Category of the slice (Build or Test).
--   cpu_time_ns: Total CPU scheduling time in nanoseconds attributed to the agent's process tree during these slices.
--   peak_rss_bytes: Peak resident set size (RSS) in bytes observed during these slices.
PERFETTO PRAGMA pipelines = 1;

CREATE PERFETTO TABLE agent_pids AS
SELECT
  ag.string_value AS agent,
  rp.int_value AS pid
FROM slice s
JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
JOIN args rp ON s.arg_set_id = rp.arg_set_id AND rp.key = 'run.pid'
WHERE s.name = 'Run';

CREATE PERFETTO TABLE agent_builds AS
SELECT
  s.id,
  s.ts,
  s.dur,
  ag.string_value AS agent,
  CASE
    WHEN s.name LIKE 'Build%' THEN 'Build'
    WHEN s.name LIKE 'Test%' THEN 'Test'
    ELSE s.name
  END AS slice_name
FROM slice s
JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
WHERE (s.name IN ('Build', 'Test') OR s.name LIKE 'Build:%' OR s.name LIKE 'Test:%') AND s.dur > 0;

CREATE PERFETTO TABLE agent_sched AS
SELECT
  sc.ts,
  sc.dur,
  ap.agent
FROM sched sc
JOIN thread th ON sc.utid = th.utid
JOIN process pr ON th.upid = pr.upid
JOIN agent_pids ap ON pr.pid = ap.pid OR pr.parent_upid IN (SELECT upid FROM process WHERE pid = ap.pid)
WHERE sc.dur > 0;

CREATE PERFETTO TABLE raw_sched_intersections AS
INTERVAL INTERSECTION OF (agent_builds AS b, agent_sched AS s)
|> SELECT ts, dur, b.agent AS b_agent, b.slice_name, s.agent AS s_agent;

CREATE PERFETTO TABLE sched_intersections AS
FROM (
  SELECT ts, dur, b_agent AS agent, slice_name
  FROM raw_sched_intersections
  WHERE b_agent = s_agent
)
|> SELECT ts, dur, agent, slice_name;

CREATE PERFETTO TABLE sched_totals AS
SELECT
  agent,
  slice_name,
  SUM(dur) AS sched_cpu_ns
FROM sched_intersections
GROUP BY agent, slice_name;

CREATE PERFETTO TABLE raw_agent_cpu_counters AS
SELECT
  ap.agent,
  c.track_id,
  c.ts,
  CAST(CASE 
    WHEN ct.unit = 'us' OR ct.name = 'cpu' THEN c.value * 1000
    ELSE c.value 
  END AS INT) AS cpu_ns
FROM counter c
LEFT JOIN process_counter_track pct ON c.track_id = pct.id
LEFT JOIN process pr ON pct.upid = pr.upid
LEFT JOIN counter_track ct ON c.track_id = ct.id
LEFT JOIN track p ON ct.parent_id = p.id
LEFT JOIN track gp ON p.parent_id = gp.id
JOIN agent_pids ap ON pr.pid = ap.pid 
  OR pr.parent_upid IN (SELECT upid FROM process WHERE pid = ap.pid)
  OR ct.name LIKE 'agent:' || ap.agent || '%cpu%'
  OR (ct.name = 'cpu' AND (p.name = 'agent:' || ap.agent OR gp.name = 'agent:' || ap.agent))
WHERE (
  pct.name IN ('cpu.user_ns', 'cpu.kernel_ns', 'runtime')
  OR pct.name LIKE '%cpu%'
  OR pct.name LIKE '%runtime%'
  OR ct.name LIKE '%cpu%'
  OR (ct.name = 'cpu' AND (p.name = 'agent:' || ap.agent OR gp.name = 'agent:' || ap.agent))
);

CREATE PERFETTO TABLE counter_cpu_totals AS
SELECT
  agent,
  slice_name,
  SUM(track_delta) AS counter_cpu_delta_ns
FROM (
  SELECT
    b.agent,
    b.slice_name,
    c.track_id,
    MAX(c.cpu_ns) - MIN(c.cpu_ns) AS track_delta
  FROM agent_builds b
  JOIN raw_agent_cpu_counters c ON b.agent = c.agent 
    AND c.ts >= b.ts 
    AND c.ts <= (b.ts + b.dur)
  GROUP BY b.agent, b.slice_name, b.id, c.track_id
)
GROUP BY agent, slice_name;

CREATE PERFETTO TABLE raw_agent_rss AS
SELECT
  ap.agent,
  c.track_id,
  c.ts,
  CAST(c.value AS INT) AS rss
FROM counter c
LEFT JOIN process_counter_track pct ON c.track_id = pct.id
LEFT JOIN process pr ON pct.upid = pr.upid
LEFT JOIN counter_track ct ON c.track_id = ct.id
LEFT JOIN track p ON ct.parent_id = p.id
LEFT JOIN track gp ON p.parent_id = gp.id
JOIN agent_pids ap ON pr.pid = ap.pid 
  OR pr.parent_upid IN (SELECT upid FROM process WHERE pid = ap.pid)
  OR ct.name LIKE 'agent:' || ap.agent || '%rss%'
  OR (ct.name = 'rss' AND (p.name = 'agent:' || ap.agent OR gp.name = 'agent:' || ap.agent))
WHERE (
  pct.name LIKE '%rss%'
  OR ct.name LIKE '%rss%'
  OR (ct.name = 'rss' AND (p.name = 'agent:' || ap.agent OR gp.name = 'agent:' || ap.agent))
);

CREATE PERFETTO TABLE rss_intervals AS
SELECT
  row_number() OVER () AS id,
  track_id,
  agent,
  ts,
  MAX(1, COALESCE(LEAD(ts) OVER (PARTITION BY track_id ORDER BY ts) - ts, (SELECT end_ts FROM trace_bounds) - ts, 1000000000)) AS dur,
  rss
FROM raw_agent_rss;

CREATE PERFETTO TABLE raw_rss_intersections AS
INTERVAL INTERSECTION OF (agent_builds AS b, rss_intervals AS r)
|> SELECT ts, dur, b.agent AS b_agent, b.slice_name, r.agent AS r_agent, r.rss;

CREATE PERFETTO TABLE rss_intersections AS
FROM (
  SELECT b_agent AS agent, slice_name, rss
  FROM raw_rss_intersections
  WHERE b_agent = r_agent
)
|> SELECT agent, slice_name, rss;

SELECT
  b.agent,
  b.slice_name,
  COALESCE(st.sched_cpu_ns, ct.counter_cpu_delta_ns, 0) AS cpu_time_ns,
  COALESCE(MAX(ri.rss), 0) AS peak_rss_bytes
FROM agent_builds b
LEFT JOIN sched_totals st ON b.agent = st.agent AND b.slice_name = st.slice_name
LEFT JOIN counter_cpu_totals ct ON b.agent = ct.agent AND b.slice_name = ct.slice_name
LEFT JOIN rss_intersections ri ON b.agent = ri.agent AND b.slice_name = ri.slice_name
GROUP BY b.agent, b.slice_name
ORDER BY b.agent, b.slice_name;
