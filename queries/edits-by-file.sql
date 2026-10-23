-- Question: What files were edited, by how many agents, and how many lines were added/removed?
-- Columns:
--   path: File path that was edited.
--   agent_count: Number of distinct agents that modified this file.
--   lines_added: Total lines added across all edits.
--   lines_removed: Total lines removed across all edits.
--   edit_count: Total number of edit operations on this file.
SELECT
  fp.string_value AS path,
  COUNT(DISTINCT ag.string_value) AS agent_count,
  SUM(la.int_value) AS lines_added,
  SUM(lr.int_value) AS lines_removed,
  COUNT(*) AS edit_count
FROM slice s
JOIN args fp ON s.arg_set_id = fp.arg_set_id AND fp.key = 'file_edited.path'
LEFT JOIN args ag ON s.arg_set_id = ag.arg_set_id AND ag.key = 'agent'
LEFT JOIN args la ON s.arg_set_id = la.arg_set_id AND la.key = 'file_edited.lines_added'
LEFT JOIN args lr ON s.arg_set_id = lr.arg_set_id AND lr.key = 'file_edited.lines_removed'
WHERE s.name NOT LIKE 'FileEdited%'
GROUP BY path
ORDER BY path;
