//! Shared library for the Mnemosyne session memory system.
//!
//! Provides the SQLite database layer, JSONL transcript parser, file anatomy
//! extraction, data models, and schema definitions used by all Mnemosyne binaries.

pub mod anatomy;
pub mod db;
pub mod jsonl;
pub mod models;
pub mod schema;
