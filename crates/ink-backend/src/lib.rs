//! Ink Mixer audio backend.
//!
//! Provides the audio I/O abstraction and its implementations.
//!
//! OS-specific and `cpal`-specific types must stay inside this crate and
//! must not leak into `ink-core` or the application layer.
