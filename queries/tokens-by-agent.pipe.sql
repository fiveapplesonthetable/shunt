-- Question: What are the token totals (input, output, cached, total) for each agent?
-- Columns:
--   agent: Identifier of the agent (e.g. worker-1).
--   input_tokens: Total prompt/input tokens consumed.
--   output_tokens: Total completion/output tokens generated.
--   cached_tokens: Total cached tokens read.
--   total_tokens: Sum of input, output, and cached tokens.
PERFETTO PRAGMA pipelines = 1;

FROM (
  SELECT
    ag.string_value AS agent,
    SUM(COALESCE(inp.int_value, 0)) AS input_tokens,
    SUM(COALESCE(outp.int_value, 0)) AS output_tokens,
    SUM(COALESCE(cac.int_value, 0)) AS cached_tokens,
    SUM(COALESCE(inp.int_value, 0) + COALESCE(outp.int_value, 0) + COALESCE(cac.int_value, 0)) AS total_tokens
  FROM slice s
  LEFT JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
  LEFT JOIN args inp ON s.arg_set_id = inp.arg_set_id AND inp.key = 'turn_ended.input_tokens'
  LEFT JOIN args outp ON s.arg_set_id = outp.arg_set_id AND outp.key = 'turn_ended.output_tokens'
  LEFT JOIN args cac ON s.arg_set_id = cac.arg_set_id AND cac.key = 'turn_ended.cached_tokens'
  WHERE inp.int_value IS NOT NULL OR outp.int_value IS NOT NULL OR cac.int_value IS NOT NULL
  GROUP BY agent
  ORDER BY agent
)
|> SELECT agent, input_tokens, output_tokens, cached_tokens, total_tokens;
