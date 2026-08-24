//! `/logout` -- remove auth credentials and return to the login screen.
//!
//! `/logout codex` removes only the OpenAI Codex (ChatGPT OAuth) credential;
//! xAI auth and the current session are untouched.

use crate::app::actions::Action;
use crate::slash::command::{AppCtx, ArgItem, CommandExecCtx, CommandResult, SlashCommand};

pub struct LogoutCommand;

impl SlashCommand for LogoutCommand {
    fn name(&self) -> &str {
        "logout"
    }

    fn description(&self) -> &str {
        "Log out and return to the login screen"
    }

    fn usage(&self) -> &str {
        "/logout [codex]"
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn takes_args_now(&self, _ctx: &AppCtx) -> bool {
        // Bare `/logout` is the common case — Enter should send, not chain
        // into args mode. `codex` stays reachable by typing it.
        false
    }

    fn suggest_args(&self, _ctx: &AppCtx, _args_query: &str) -> Option<Vec<ArgItem>> {
        Some(vec![ArgItem {
            display: "codex".into(),
            match_text: "codex".into(),
            insert_text: "codex".into(),
            description: "Disconnect OpenAI Codex only".into(),
        }])
    }

    fn run(&self, _ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        match args.trim() {
            "" => CommandResult::Action(Action::Logout),
            "codex" => CommandResult::Action(Action::CodexLogout),
            other => CommandResult::Error(format!(
                "Unknown argument: {other}. Use /logout or /logout codex"
            )),
        }
    }
}
