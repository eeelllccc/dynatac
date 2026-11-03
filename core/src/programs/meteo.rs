//! Shell entry point for the graphical `meteo` program.

use super::{ExecContext, ProgramLaunch, ProgramResult};
use crate::meteo::{default_location, ForecastRequest};

pub const USAGE: &str = "meteo [help | setup]";
const HELP: &str = "usage: meteo [help | setup]\n\ncontrols:\n  y          earlier forecast (scroll left)\n  h          later forecast (scroll right)\n  Backspace  exit";
const KEY_CONTEXT: &str = "api-key";

pub fn run(args: &[&str], ctx: &mut ExecContext) -> ProgramResult {
    match args {
        [] => {
            if ctx.credentials.meteoblue_api_key().is_none() {
                ProgramResult::err("no Meteoblue key configured — run: meteo setup".into())
            } else {
                ProgramResult::launch(ProgramLaunch::Meteo)
            }
        }
        ["setup"] => ProgramResult::ok(format!(
            "__START_TEXT_PROMPT__\nmask\n{KEY_CONTEXT}\nMeteoblue API key:"
        )),
        ["help"] => ProgramResult::ok(HELP.into()),
        _ => ProgramResult::err(format!("usage: {USAGE}")),
    }
}

pub fn on_text_submit(context: &str, text: &str, ctx: &mut ExecContext) -> ProgramResult {
    if context != KEY_CONTEXT {
        return ProgramResult::err("meteo: unexpected prompt context".into());
    }
    let key = text.trim();
    if key.is_empty() {
        return if ctx.credentials.meteoblue_api_key().is_some() {
            ProgramResult::ok("cancelled — kept existing Meteoblue key".into())
        } else {
            ProgramResult::err("API key cannot be empty".into())
        };
    }
    if ForecastRequest::new(default_location()).url(key).is_err() {
        return ProgramResult::err("invalid Meteoblue API key".into());
    }
    match ctx.credentials.set_meteoblue_api_key(key) {
        Ok(()) => ProgramResult::ok("saved Meteoblue API key".into()),
        Err(error) => ProgramResult::err(format!("save failed: {error}")),
    }
}
