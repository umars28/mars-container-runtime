pub mod events;
pub mod features;
pub mod lifecycle;
pub mod list;
pub mod ps;
pub mod spec;

use crate::cli::{Cli, Command, Rootless};
use crate::error::Result;
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

        Command::Features => {
            features::run()?;
            Ok(0)
        }

        Command::List(args) => {
            list::run(&layout, args)?;
            Ok(0)
        }

        Command::Create(args) => {
            validate_id(&args.id)?;
            lifecycle::create(&layout, args, rootless, cli.otlp_endpoint.as_deref())?;
            Ok(0)
        }

        Command::Start(args) => {
            validate_id(&args.id)?;
            crate::container::start(&layout, &args.id)?;
            Ok(0)
        }

        Command::Run(args) => {
            validate_id(&args.id)?;
            lifecycle::run(&layout, args, rootless, cli.otlp_endpoint.as_deref())
        }

        Command::Exec(args) => {
            validate_id(&args.id)?;
            lifecycle::exec(&layout, args)
        }

        Command::Kill(args) => {
            validate_id(&args.id)?;
            lifecycle::kill(&layout, args)?;
            Ok(0)
        }

        Command::Delete(args) => {
            validate_id(&args.id)?;
            crate::container::delete(&layout, &args.id, args.force)?;
            Ok(0)
        }

        Command::State(args) => {
            validate_id(&args.id)?;
            lifecycle::state(&layout, &args.id)?;
            Ok(0)
        }

        Command::Ps(args) => {
            validate_id(&args.id)?;
            ps::run(&layout, args)?;
            Ok(0)
        }

        Command::Pause(args) => {
            validate_id(&args.id)?;
            crate::container::pause(&layout, &args.id, true)?;
            Ok(0)
        }

        Command::Resume(args) => {
            validate_id(&args.id)?;
            crate::container::pause(&layout, &args.id, false)?;
            Ok(0)
        }

        Command::Events(args) => {
            validate_id(&args.id)?;
            events::run(&layout, args)?;
            Ok(0)
        }

        Command::Update(args) => {
            validate_id(&args.id)?;
            lifecycle::update(&layout, args)?;
            Ok(0)
        }
    }
}
