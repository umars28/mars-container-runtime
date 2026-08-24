pub mod list;
pub mod spec;

use crate::cli::{Cli, Command, Rootless};
use crate::error::{Error, Result};
use crate::paths::{Layout, validate_id};

pub fn dispatch(cli: Cli) -> Result<u8> {
    let layout = Layout::new(cli.root.clone());
    let rootless = matches!(cli.rootless, Some(Rootless::True))
        || (matches!(cli.rootless, Some(Rootless::Auto)) && !nix::unistd::geteuid().is_root());

    match &cli.command {
        Command::Spec(args) => {
            spec::run(args, rootless)?;
            Ok(0)
        }

        Command::List(args) => {
            list::run(&layout, args)?;
            Ok(0)
        }

        Command::Create(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("create"))
        }

        Command::Start(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("start"))
        }

        Command::Run(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("run"))
        }

        Command::Exec(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("exec"))
        }

        Command::Kill(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("kill"))
        }

        Command::Delete(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("delete"))
        }

        Command::State(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("state"))
        }

        Command::Ps(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("ps"))
        }

        Command::Pause(args) | Command::Resume(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("pause/resume"))
        }

        Command::Events(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("events"))
        }

        Command::Update(args) => {
            validate_id(&args.id)?;
            Err(Error::Unimplemented("update"))
        }

        Command::Features => Err(Error::Unimplemented("features")),
    }
}
