# xai-grok-code-mode

An in-process, persistent V8 runtime for Grok Build's Code Mode. It evaluates
raw JavaScript as an async module, dispatches nested tool calls through a host
delegate, supports early yields and later `wait`/termination, and preserves
serializable session state between cells.

This crate is adapted from OpenAI Codex's `codex-code-mode` crate at commit
`2be648ba4a6c159a3d80b1c07e7323cbd5efef8f`. Grok Build intentionally carries
only the in-process runtime; Codex's optional remote host-process transport is
not included.
