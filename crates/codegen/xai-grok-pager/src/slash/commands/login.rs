//! `/login` -- log in or re-authenticate with your account.
//!
//! `/login codex` connects the OpenAI Codex (ChatGPT OAuth) account instead;
//! it is independent of xAI auth and uses the shell's browser flow.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct LoginCommand;

impl SlashCommand for LoginCommand {
    fn name(&self) -> &str {
        "login"
    }

    fn description(&self) -> &str {
        "Log in or re-authenticate with your account"
    }

    fn usage(&self) -> &str {
        "/login [codex]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn takes_args_now(&self, _ctx: &AppCtx) -> bool {
        // Bare `/login` is the common case — Enter should send, not chain
        // into args mode. `codex` stays reachable by typing it.
        false
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(vec![ArgItem {
            display: "codex".into(),
            match_text: "codex".into(),
            insert_text: "codex".into(),
            description: "Connect OpenAI Codex (ChatGPT)".into(),
        }])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match args.trim() {
            "" => CommandResult::Action(Action::Login),
            "codex" => CommandResult::Action(Action::CodexLogin),
            other => CommandResult::Error(format!(
                "Unknown argument: {other}. Use /login or /login codex"
            )),
        }
    }
}
