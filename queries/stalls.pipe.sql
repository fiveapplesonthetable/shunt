-- Question: What stall detector firings occurred, for which agent, and what was the evidence?
-- Columns:
--   agent: Identifier of the agent (e.g. worker-2).
--   flag: Detector flag name (e.g. LOOP, FAILING, SILENT, SPIN, BURN, OFF_BRIEF).
--   evidence: Descriptive explanation of the detected anomaly.
--   repeats: Number of repeated occurrences triggering the detector.
PERFETTO PRAGMA pipelines = 1;

FROM (
  SELECT
    ag.string_value AS agent,
    sk.string_value AS flag,
    ev.string_value AS evidence,
    COALESCE(rp.int_value, 0) AS repeats
  FROM slice s
  JOIN args sk ON s.arg_set_id = sk.arg_set_id AND sk.key = 'stall.kind'
  LEFT JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
  LEFT JOIN args ev ON s.arg_set_id = ev.arg_set_id AND ev.key = 'stall.evidence'
  LEFT JOIN args rp ON s.arg_set_id = rp.arg_set_id AND rp.key = 'stall.repeats'
  ORDER BY s.ts ASC
)
|> SELECT agent, flag, evidence, repeats;
