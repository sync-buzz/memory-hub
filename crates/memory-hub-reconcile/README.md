# memory-hub-reconcile

Hookless code-history reconciliation behind the `Reconciler` interface.

- a worktree-local cursor stores the full last processed code commit;
- MCP initialization, CLI operations, and every Memory mutation reconcile the
  actual Git graph rather than relying on filesystem events or hooks;
- missed commits are walked in topological order, path diffs stale matching
  plaintext records, and every code commit receives a Memory checkpoint even
  when the Memory tree did not change;
- checkpoint-before-cursor ordering and deterministic transaction ids make an
  interrupted reconcile safe to retry;
- rewritten history returns a structured divergence report. A caller must
  explicitly request `full_rebuild`, which invalidates freshness before moving
  the cursor; and
- linked worktrees keep independent cursors while sharing canonical Memory refs.

The implementation reads Git through `git2`. Hooks and file watchers may later
wake reconciliation sooner, but neither participates in correctness.
