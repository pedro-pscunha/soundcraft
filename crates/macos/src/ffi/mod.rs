//! The only module with `unsafe`: the Objective-C side of the crate. Every `unsafe` block carries a
//! `// SAFETY:` comment; what leaves this module is a safe API.

pub mod open_documents;
