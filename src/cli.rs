use std::ffi::{OsStr, OsString};

use crate::error::{BridgeError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeCommand {
    Launch(Vec<OsString>),
    Install,
    Repair,
    Uninstall,
    Doctor { live: bool },
    Login,
    Usage,
    Mcp,
}

pub fn parse_from<I, S>(args: I) -> Result<BridgeCommand>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    let mut args = args.into_iter().map(Into::into);
    let _program = args.next();
    let rest = args.collect::<Vec<_>>();
    let Some(command) = rest.first().and_then(|value| value.to_str()) else {
        return Ok(BridgeCommand::Launch(rest));
    };

    match command {
        "install" => no_args(BridgeCommand::Install, &rest),
        "repair" => no_args(BridgeCommand::Repair, &rest),
        "uninstall" => no_args(BridgeCommand::Uninstall, &rest),
        "login" => no_args(BridgeCommand::Login, &rest),
        "usage" => no_args(BridgeCommand::Usage, &rest),
        "mcp" => no_args(BridgeCommand::Mcp, &rest),
        "doctor" => match rest.get(1).map(OsString::as_os_str) {
            None => Ok(BridgeCommand::Doctor { live: false }),
            Some(value) if value == OsStr::new("--live") && rest.len() == 2 => {
                Ok(BridgeCommand::Doctor { live: true })
            }
            _ => Err(BridgeError::InvalidCommand(
                "usage: claude-gpt doctor [--live]".to_owned(),
            )),
        },
        _ => Ok(BridgeCommand::Launch(rest)),
    }
}

fn no_args(command: BridgeCommand, args: &[OsString]) -> Result<BridgeCommand> {
    if args.len() == 1 {
        Ok(command)
    } else {
        Err(BridgeError::InvalidCommand(format!(
            "{} does not accept arguments",
            args[0].to_string_lossy()
        )))
    }
}
