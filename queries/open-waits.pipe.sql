-- Question: Which runs are currently waiting, on what target/reason, and for how long?
-- Columns:
--   agent: Identifier of the agent (e.g. worker-3).
--   run: Run sequence number for this agent.
--   target: Target of the wait (e.g. subagent task slug or resource).
--   reason: Category of the wait (e.g. WAIT_SUBAGENT, WAIT_INPUT, WAIT_MESSAGE).
--   dur: Duration of the wait so far in nanoseconds.
PERFETTO PRAGMA pipelines = 1;

CREATE PERFETTO TABLE _trace_end AS
SELECT max(ts) AS end_ts FROM (
  SELECT ts FROM slice
  UNION ALL
  SELECT ts FROM state
);

FROM (
  SELECT
    ag.string_value AS agent,
    CAST(SUBSTR(p.name, 5) AS INT) AS run,
    COALESCE(wt.string_value, '') AS target,
    COALESCE(wo.string_value, '') AS reason,
    (SELECT end_ts FROM _trace_end) - s.ts AS dur
  FROM slice s
  JOIN track t ON s.track_id = t.id
  JOIN track p ON t.parent_id = p.id
  LEFT JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
  LEFT JOIN args wt ON s.arg_set_id = wt.arg_set_id AND wt.key = 'wait.target'
  LEFT JOIN args wo ON s.arg_set_id = wo.arg_set_id AND wo.key = 'wait.on'
  WHERE s.name = 'Wait' AND s.dur < 0
  ORDER BY s.ts ASC
)
|> SELECT agent, run, target, reason, dur;
