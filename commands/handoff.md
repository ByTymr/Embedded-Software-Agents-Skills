---
description: Save a local work summary for a new chat without changing the Git or board state.
---

Write a new Markdown note under the project's ignored `sessions/handoffs/` directory.
Check that Git ignores the path before writing. If needed, use a local directory outside
the repository. Use a timestamped filename and keep previous notes accessible.

Record the following from the current task and available evidence.

- User goal, working branch, commit and relevant uncommitted files.
- Latest build and its AXF/HEX references, when relevant.
- Last verified firmware on the target and the time of that verification. Record build,
  upload and physical test results separately.
- Related measurement notes, pending checks and approaches already tried.
- Remote session name, if needed, and the first actionable next step.

Keep authentication material, device identifiers and raw chat history out of the note.
For a summary intended for sharing outside the workspace, also omit private names, paths,
configuration and measurements. Give the user the local note path and a short prompt for
the next chat: “Resume work from this handoff note.”
