//! The web chat's logic without any UI framework or browser API, so it is tested natively.

pub mod chat;
pub mod connection;

pub use chat::{Chat, Item, PermissionStatus, RequestStatus, Row, ToolStatus};
pub use connection::{Action, Connection, State};
