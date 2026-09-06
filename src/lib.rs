//! jlocal library root: the loopback API and its feature modules.
//!
//! A lib (not just a binary) so the capability surface the web UI reads
//! (`audio`, `capture`, `publish` types and constants) is genuinely exported
pub mod api;
pub mod audio;
pub mod audio_engine;
pub mod capture;
pub mod permissions;
pub mod publish;
pub mod status;
pub mod torrent;
pub mod update;
