//! The Windows backend: `os` on a real Windows file system, `parts` the parts that make no
//! Windows call.

#![cfg(target_os = "windows")]

mod os;
mod parts;
