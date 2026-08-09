mod seekable;
mod simple;

pub use seekable::{Reader, write_nar};
pub use simple::write_nar as write_nar_simple;
