//! Reusable tool families for Gemini Live hosts.
//!
//! This crate owns tool definitions whose execution model is stable across
//! hosts. It intentionally does not own host-specific composition, UI, device
//! state, or persistence. Concrete applications choose which tool families to
//! expose and how to combine them with host-local capabilities.

pub mod timer;
pub mod workspace;
