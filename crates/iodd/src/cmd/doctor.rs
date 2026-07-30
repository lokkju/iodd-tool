use crate::cli::DoctorArgs;
use crate::error::{Error, Result};

pub fn run(_args: &DoctorArgs) -> Result<u8> {
    Err(Error::NotImplemented("doctor"))
}
