use crate::cli::ConvertArgs;
use crate::error::{Error, Result};

pub fn run(_args: &ConvertArgs) -> Result<u8> {
    Err(Error::NotImplemented("convert"))
}
