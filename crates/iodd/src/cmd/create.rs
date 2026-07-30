use crate::cli::CreateArgs;
use crate::error::{Error, Result};

pub fn run(_args: &CreateArgs) -> Result<u8> {
    Err(Error::NotImplemented("create"))
}
