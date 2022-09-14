//! Surf Disco: a client library for [Tide Disco](https://tide-disco.docs.espressosys.com/tide_disco/) applications.
//!
//! # Quick Start
//!
//!

pub mod client;
pub mod error;
pub mod request;

pub use client::Client;
pub use error::Error;
pub use request::Request;
pub use surf::{
    http::{self, Method, StatusCode},
    Url,
};
