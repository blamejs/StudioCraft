//! Every call into the NVIDIA driver: the transcribed encode API ([`abi`]), the device a session is
//! opened on ([`device`]) and the session itself ([`session`]). The items re-exported here are safe.

#![cfg_attr(not(any(target_os = "windows", all(target_os = "linux", target_pointer_width = "64"))), allow(dead_code))]

mod abi;
#[cfg(test)]
mod abi_tests;
mod device;
mod session;

pub(crate) use session::{Codec, Locked, OpenError, Params, Rate, Session, Submitted, driver};
