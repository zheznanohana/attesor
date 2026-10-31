# Local default-on release106 / owner request

The owner explicitly requested that the retained optimization profile be ON in
ordinary `attesor`, not hidden behind a separate entrypoint. DEFAULT_ON now
matches the previously measured full `attesor-fast` profile through recleaf101
and inlinewide105. This changes the feature selection, not the guest algorithms
or compiler safety guards. Negative/flat trials remain OFF. This is NOT a claim
of new V8 improvement, all-program acceptance or all-target >=60% achievement.

* `attesor program-arm64`: retained profile ON by default, normal AOT policy.
* `attesor --no-aot program-arm64`: same default flags, JIT only.
* `attesor-fast program-arm64`: retained backward-compatible JIT-only alias.
* `attesor-safe program-arm64`: explicitly forces the pre106 conservative list;
  normal AOT policy is unchanged, and JIT_OFF still has priority.

An explicitly set ATTESOR_JIT_ON (even empty) continues to override ordinary
attesor defaults. Clear stale JIT_ON/OFF overrides when verifying the new default.

Translator version106 intentionally isolates old cached code. First launches
may rebuild translations; no existing cache is deleted. The local install keeps
an executable backup and source checkpoint, uses atomic executable replacement,
and does not stop running guest programs or modify launch agents/GPU bridges.
New defaults apply to newly started processes; already-running processes keep
their mapped code. Compare V8 only in a quiet, same-engine/same-input window.
