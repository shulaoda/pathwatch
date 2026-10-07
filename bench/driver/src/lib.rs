//! The driver of the benchmark: it makes a tree of files, starts ONE adapter as a child
//! process, changes files, and measures the adapter from outside. Every library is measured
//! in the same way, because the driver knows nothing about the library behind an adapter.

pub mod adapter;
pub mod machine;
pub mod measure;
pub mod record;
pub mod scenarios;
pub mod session;
pub mod tree;
