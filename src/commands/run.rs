use crate::bundle::Bundle;
use crate::cli::RunArgs;
use crate::container::{self, RunOptions};
use crate::error::{Error, Result};

pub fn run(args: &RunArgs) -> Result<u8> {
    if args.detach {
        return Err(Error::Unimplemented("run --detach"));
    }
    if args.console_socket.is_some() {
        return Err(Error::Unimplemented("--console-socket"));
    }
    if args.preserve_fds != 0 {
        return Err(Error::Unimplemented("--preserve-fds"));
    }
    if args.no_pivot {
        return Err(Error::Unimplemented("--no-pivot"));
    }

    let bundle = Bundle::load(&args.bundle)?;

    container::run(
        &bundle,
        &RunOptions {
            id: args.id.clone(),
            pid_file: args.pid_file.clone(),
        },
    )
}
