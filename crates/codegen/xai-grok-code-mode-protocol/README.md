# xai-grok-code-mode-protocol

Protocol types, JavaScript tool descriptions, pragma parsing, and session traits
for Grok Build's embedded Code Mode runtime.

This crate is adapted from OpenAI Codex's `codex-code-mode-protocol` crate at
commit `2be648ba4a6c159a3d80b1c07e7323cbd5efef8f`. The out-of-process host wire
protocol is intentionally omitted; Grok Build uses the embedded runtime.
