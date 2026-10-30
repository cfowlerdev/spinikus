//! The agent protocol (`spinikus.agent.v1`): messages and the tonic client and server.
//!
//! Shared by the agent and the control plane, which depend on nothing else of each other.
//! Conversions to and from `spinikus-core` types live on either side, not here.

pub mod agent {
    // Generated code: prost puts `oneof` messages inline, so variant sizes differ.
    #[allow(clippy::large_enum_variant)]
    pub mod v1 {
        tonic::include_proto!("spinikus.agent.v1");

        /// The protocol version agents send in `Hello`. Bumped when the control plane needs
        /// to tell apart agents that understand newer, additive parts of v1.
        pub const PROTOCOL_VERSION: u32 = 1;
    }
}
