#![allow(dead_code, private_interfaces)]

/*
CDXC:GhostexTui 2026-05-26-10:55:
The Ghostex TUI `src/bin` target needs Herdr's native terminal runtime instead
of a local vt100 wrapper so attached sessions inherit Herdr's scrollback,
cursor, color, keyboard, and mouse behavior. Expose the existing Herdr modules
through a library crate so the Ghostex binary can call that runtime without
duplicating module paths or reimplementing terminal semantics.
*/
pub const HERDR_ENV_VAR: &str = "HERDR_ENV";
pub const HERDR_ENV_VALUE: &str = "1";

pub mod agent_resume;
pub mod api;
pub mod app;
pub mod cli;
pub mod client;
pub mod config;
pub mod detect;
pub mod events;
pub mod ghostty;
pub mod input;
pub mod integration;
pub mod ipc;
pub mod kitty_graphics;
pub mod layout;
pub mod logging;
pub mod pane;
pub mod persist;
pub mod platform;
pub mod product_announcements;
pub mod protocol;
pub mod raw_input;
pub mod release_notes;
pub mod remote;
pub mod selection;
pub mod server;
pub mod session;
pub mod sound;
pub mod terminal;
pub mod terminal_notify;
pub mod terminal_theme;
pub mod ui;
pub mod update;
pub mod workspace;
pub mod worktree;
