//! newcut: know which albums you have, which you don't, and what's new
//! from the artists in your music libraries.

#![allow(clippy::missing_errors_doc, clippy::missing_panics_doc)]

pub mod app;
pub mod catalog;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod env_file;
pub mod events;
pub mod hex;
pub mod http;
pub mod import;
pub mod listenbrainz;
pub mod lock;
pub mod matching;
pub mod musicbrainz;
pub mod notify;
pub mod paths;
pub mod process;
pub mod scan;
pub mod shutdown;
pub mod slskd;
pub mod state;
pub mod web;
pub mod work;
