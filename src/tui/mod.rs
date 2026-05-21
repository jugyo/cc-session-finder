//! TUI entry point: run the indexing phase to completion (showing only a
//! terminal-line spinner), then start the interactive app on the populated DB.

use std::process::ExitCode;

use anyhow::Result;

pub mod app;
pub mod indexing;
pub mod input;
pub mod view;

pub fn run(initial_query: Option<String>, reindex: bool, no_vector: bool) -> Result<ExitCode> {
    indexing::run(reindex, no_vector)?;
    app::run(initial_query, no_vector)
}
