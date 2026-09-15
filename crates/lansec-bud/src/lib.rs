//! BUD: Better User Datagrams for LAN interactive video.
//!
//! UDP + X25519/HKDF/AES-256-GCM, selective retransmission, encoder bitrate coupling.

mod congestion;
mod crypto;
mod endpoint;
mod packet;

pub use congestion::{CongestionController, CongestionStats};
pub use crypto::SessionKeys;
pub use endpoint::{BudConfig, BudEndpoint, Incoming};
pub use packet::{PacketHeader, MAX_PAYLOAD, MTU};

pub const DEFAULT_PORT: u16 = 44700;
