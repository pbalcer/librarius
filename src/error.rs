use snafu::Snafu;
use std::io;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("unable to open storage file "))]
    SourceError {  },

    #[snafu(display("memory mapping failed: {}", errno))]
    MemoryAlloc { errno: errno::Errno },

    #[snafu(display("invalid page source was provided"))]
    InvalidSource {},

    #[snafu(display("attempted to access memory outside of source"))]
    InvalidMemory {},

    #[snafu(display("Source has pagesize: {}, expected: {}", is, expected))]
    WrongPagesize { is: usize, expected: usize },

    #[snafu(display("generic I/O error {}", err))]
    FileIO { err: io::Error },

    #[snafu(display("FIXME"))]
    PartialIO {},

    #[snafu(display("source not byte addressable"))]
    NotByteAddressable {},

    #[snafu(display("no memory available"))]
    NoAvailableMemory {},

    #[snafu(display("can't flush without persistent storage"))]
    NoPersistentStorage {},

    #[snafu(display("source has invalid logical address space mapping"))]
    InvalidLogicalAddress {},

    #[snafu(display("flush on block storage"))]
    InvalidFlush {},

    #[snafu(display("source with root already exists"))]
    RootExists {},

    #[snafu(display("tried to create a log entry larger than a pagesize"))]
    LogEntryTooLarge {},

    #[snafu(display("incorrect page context length"))]
    ContextTooLarge {},

    #[snafu(display("conflict during commit"))]
    TxAborted {},
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
