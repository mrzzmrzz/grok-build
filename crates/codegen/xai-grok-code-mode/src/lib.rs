// Derived from OpenAI Codex code-mode at commit
// 2be648ba4a6c159a3d80b1c07e7323cbd5efef8f (Apache-2.0).

mod cell_actor;
mod runtime;
mod service;
mod session_runtime;
mod v8_init;

pub(crate) type TaskFailureHandler = std::sync::Arc<dyn Fn(String) + Send + Sync>;

pub use service::InProcessCodeModeSession;
pub use service::InProcessCodeModeSessionProvider;
pub use service::NoopCodeModeSessionDelegate;
pub use v8_init::V8JitMode;
pub use v8_init::initialize_v8;
pub use xai_grok_code_mode_protocol::*;
