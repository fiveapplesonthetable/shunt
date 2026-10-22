-- Question: Which commands completed with non-zero exit codes, by agent and command class?
-- Columns:
--   agent: Identifier of the agent (e.g. worker-1).
--   class: Command category (e.g. TEST, BUILD, LINT).
--   argv: The command line string executed.
--   exit_code: Non-zero integer exit status.
--   dur: Execution duration in nanoseconds.
SELECT
  ag.string_value AS agent,
  COALESCE(cl.string_value, s.name) AS class,
  COALESCE(ar.string_value, '') AS argv,
  ex.int_value AS exit_code,
  s.dur
FROM slice s
JOIN args ex ON s.arg_set_id = ex.arg_set_id AND ex.key = 'command_ended.exit'
LEFT JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
LEFT JOIN args cl ON s.arg_set_id = cl.arg_set_id AND cl.key = 'command.class'
LEFT JOIN args ar ON s.arg_set_id = ar.arg_set_id AND ar.key = 'command.argv'
WHERE ex.int_value != 0
ORDER BY s.ts ASC;
