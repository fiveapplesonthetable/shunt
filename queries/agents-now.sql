-- Question: What is each agent's current state and duration in that state?
-- Columns:
--   agent: Identifier of the agent (e.g. worker-1).
--   state: Current state name (e.g. Idle, Thinking, Tool, Waiting, Stalled).
--   dur: Duration in nanoseconds spent in the current state (up to trace end).
WITH trace_end AS (
  SELECT max(ts) AS end_ts FROM (
    SELECT ts FROM slice
    UNION ALL
    SELECT ts FROM state
    UNION ALL
    SELECT ts FROM counter
  )
),
latest_states AS (
  SELECT
    CASE
      WHEN p.name LIKE 'agent:%' THEN SUBSTR(p.name, 7)
      ELSE p.name
    END AS agent,
    s.value AS state,
    CASE
      WHEN s.dur >= 0 THEN s.dur
      ELSE (SELECT end_ts FROM trace_end) - s.ts
    END AS dur,
    ROW_NUMBER() OVER (PARTITION BY p.id ORDER BY s.ts DESC) AS rn
  FROM state s
  JOIN track t ON s.track_id = t.id
  JOIN track p ON t.parent_id = p.id
  LEFT JOIN track gp ON p.parent_id = gp.id
  WHERE t.name = 'state' AND (p.name LIKE 'agent:%' OR gp.name = 'agents')
)
SELECT
  agent,
  state,
  dur
FROM latest_states
WHERE rn = 1
ORDER BY agent;
