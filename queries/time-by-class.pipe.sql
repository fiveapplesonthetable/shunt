-- Question: How much total wall time did each agent spend in each command class?
-- Columns:
--   agent: Identifier of the agent (e.g. worker-1).
--   class: Command category (e.g. BUILD, TEST).
--   total_dur: Total wall time spent in nanoseconds.
--   command_count: Number of commands executed in this class.
PERFETTO PRAGMA pipelines = 1;

FROM (
  SELECT
    ag.string_value AS agent,
    COALESCE(cl.string_value, s.name) AS class,
    SUM(s.dur) AS total_dur,
    COUNT(*) AS command_count
  FROM slice s
  JOIN args cl ON s.arg_set_id = cl.arg_set_id AND cl.key = 'command.class'
  LEFT JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
  GROUP BY agent, class
  ORDER BY agent, class
)
|> SELECT agent, class, total_dur, command_count;
