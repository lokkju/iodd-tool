use crate::cli::VerifyArgs;
use crate::error::{Error, Result};

pub fn run(_args: &VerifyArgs) -> Result<u8> {
    Err(Error::NotImplemented("verify"))
}
