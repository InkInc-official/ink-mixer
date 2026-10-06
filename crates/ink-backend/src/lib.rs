//! Ink Mixer audio backend.
//!
//! Provides the audio I/O abstraction and its implementations.
//!
//! OS-specific and `cpal`-specific types must stay inside this crate and
//! must not leak into `ink-core` or the application layer.
//!
//! # Logging in the realtime path
//!
//! Code that runs in the audio callback or any other realtime path must not
//! call `tracing` macros (`info!`, `warn!`, `error!`, ...). Formatting and
//! writing a log record can allocate, take locks, and do I/O, which may
//! cause xruns.
//!
//! When the realtime path needs to report something (an underrun, a device
//! error), it should hand the information to a non-realtime control thread
//! through a realtime-safe channel (lock-free queue, atomics, preallocated
//! buffers), and that thread does the logging.
//!
//! See `AGENTS.md` §5 and `docs/ARCHITECTURE.md` §25.
