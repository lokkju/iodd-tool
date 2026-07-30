//! The allocation sequence: fallocate, early extent check, mandatory zero
//! pass, UNWRITTEN recheck, footer in place, then temp-file rename.
//!
//! Lands in phase 4.
