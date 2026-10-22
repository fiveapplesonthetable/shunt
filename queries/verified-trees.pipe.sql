-- Question: Which git tree hashes passed BUILD, TEST, and LINT commands, and by which agent?
-- Columns:
--   git_tree: The git tree SHA hash.
--   agent: Identifier of the agent.
--   class: Command class (BUILD, TEST, LINT).
--   argv: The command executed.
--   dur: Duration of the passing check in nanoseconds.
PERFETTO PRAGMA pipelines = 1;

FROM (
  SELECT
    tr.string_value AS git_tree,
    ag.string_value AS agent,
    COALESCE(cl.string_value, s.name) AS class,
    COALESCE(ar.string_value, '') AS argv,
    s.dur
  FROM slice s
  JOIN args tr ON s.arg_set_id = tr.arg_set_id AND tr.key = 'command.git_tree'
  JOIN args ex ON s.arg_set_id = ex.arg_set_id AND ex.key = 'command_ended.exit'
  LEFT JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
  LEFT JOIN args cl ON s.arg_set_id = cl.arg_set_id AND cl.key = 'command.class'
  LEFT JOIN args ar ON s.arg_set_id = ar.arg_set_id AND ar.key = 'command.argv'
  WHERE ex.int_value = 0
    AND UPPER(COALESCE(cl.string_value, s.name)) IN ('BUILD', 'TEST', 'LINT')
  ORDER BY s.ts ASC
)
|> SELECT git_tree, agent, class, argv, dur;
