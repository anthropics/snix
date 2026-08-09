#![cfg_attr(docsrs, feature(doc_cfg))]

pub mod composition;
pub mod decompression;
pub mod fixtures;
pub mod import;
pub mod nar;
pub mod path_info;
pub mod pathinfoservice;
pub mod proto;
pub mod utils;

// Used as user agent in various HTTP Clients
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));
