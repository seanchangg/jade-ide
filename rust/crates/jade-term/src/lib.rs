//! `jade-term` — a headless terminal engine for Jade.
//!
//! A PTY plus a terminal state machine, with **no rendering**. A GPUI panel
//! consumes [`GridSnapshot`]s in a later phase; this crate is renderer-agnostic
//! and tokio-friendly.
//!
//! # Relationship to the C++/Electron IDE
//!
//! This is the Rust rewrite of `src/main/pty-manager.ts` (`PtyManager`). Spawn
//! and lifecycle semantics are ported line-for-line and cited in the
//! [`manager`] module's doc comments:
//!
//! - shell = `$SHELL` else `/bin/zsh`; terminfo `xterm-256color`;
//!   `TERM=xterm-256color`, `COLORTERM=truecolor`; cwd = workspace root;
//!   initial 80x24 (`pty-manager.ts:21-41`).
//! - multiple instances with numeric ids from 1 (`:19`); per-instance exit
//!   notification with code (`:82-90`); user input and programmatic writes
//!   share one PTY (inventory §5.2).
//! - PTY-unavailable degrades gracefully (`:22`, `:96`).
//!
//! # Implementation
//!
//! Built on [`alacritty_terminal`] `0.26` — the same grid/state-machine crate
//! Zed uses. Its `tty` module owns the Unix PTY (`rustix-openpty` + fork/exec)
//! and its `event_loop` runs a dedicated OS thread that reads the PTY, drives
//! the VTE parser into the `Term`, and reports child exit race-free via
//! `SIGCHLD`. The `Term` state lives behind a fair mutex;
//! [`TermManager::snapshot`] clones out a plain [`GridSnapshot`].
//!
//! # Example
//!
//! ```no_run
//! use jade_term::{TermManager, TermEvent};
//! use std::path::Path;
//!
//! # async fn run() {
//! let manager = TermManager::new();
//! let mut events = manager.take_events().unwrap();
//!
//! let id = manager.create(Path::new("/tmp")).expect("PTY available");
//! manager.write(id, b"echo hi\n");
//!
//! while let Some(event) = events.recv().await {
//!     match event {
//!         TermEvent::Damaged { id } => {
//!             let snap = manager.snapshot(id).unwrap();
//!             // hand `snap` to the renderer …
//!             let _ = snap;
//!         }
//!         TermEvent::Exited { id, code } => {
//!             println!("terminal {id} exited: {code:?}");
//!             break;
//!         }
//!     }
//! }
//! # }
//! ```

mod manager;
mod snapshot;

pub use manager::{TermError, TermEvent, TermId, TermManager, DEFAULT_SCROLLBACK};
pub use snapshot::{Cell, CellFlags, Color, Cursor, GridSnapshot};
