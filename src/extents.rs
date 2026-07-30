//! `FS_IOC_FIEMAP` and the extent-run merge.
//!
//! The ioctl and the merge stay separate: the merge is a pure function over
//! records, which is what makes it testable without a filesystem. Ported from
//! `spike/fiemap.py`, which is known-good against the physical device.
//!
//! Lands in phase 2.
