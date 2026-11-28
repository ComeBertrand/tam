//! Where every command prints. std's `println!` panics when stdout is a
//! pipe whose reader went away (`tam ps | head -1`, #38): the crate root
//! redefines `println!` and `eprintln!` to go through here instead, for all
//! the modules.
//!
//! Restoring the default `SIGPIPE` disposition would also stop the panic,
//! but the same signal would then kill a client whose daemon closed the
//! socket, silently and without the daemon's exit status (and in the middle
//! of an attach, with the terminal left raw).

use std::fmt::Arguments;
use std::io::{ErrorKind, Write};

/// Print a line on stdout. When the reader closed it, exit quietly (status
/// 0: what it read is all it wanted); on any other error, exit 1 saying so.
pub fn stdout_line(args: Arguments) {
    let mut out = std::io::stdout().lock();
    let written = out
        .write_fmt(args)
        .and_then(|()| out.write_all(b"\n"))
        .and_then(|()| out.flush());
    if let Err(e) = written {
        if e.kind() == ErrorKind::BrokenPipe {
            crate::activity::exit(0);
        }
        stderr_line(format_args!("tam: cannot write to stdout: {e}"));
        crate::activity::exit(1);
    }
}

/// Print a line on stderr, if it can be written at all: there is nowhere
/// else to report a failure to.
pub fn stderr_line(args: Arguments) {
    let mut err = std::io::stderr().lock();
    let _ = err.write_fmt(args).and_then(|()| err.write_all(b"\n"));
}
