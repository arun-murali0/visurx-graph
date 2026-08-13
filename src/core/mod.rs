//! Only `parse` exists right now. Add the others (`semantic`, `resolve`,
//! `cfg`, `aggregate`, `pipeline`) as each is actually built — declaring
//! them ahead of the files existing breaks the build for a directory being
//! empty, not for a real code error.

pub mod analyse;
pub mod cfg;
pub mod parse;
pub mod references;
pub mod resolve;
pub mod semantic;
