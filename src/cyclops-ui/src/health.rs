//! Client and daemon build identity carried by the socket greeting.

/// Source build stamped into this UI client.
pub const CLIENT_BUILD: &str = env!("CYCLOPS_BUILD_REF");

/// Whether this client is connected to the daemon build it expects.
///
/// Both identities stay in the model. Rendering does not collapse a mismatch
/// into a generic warning that leaves an operator unable to identify the old
/// process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildHealth {
    Current { client: String, daemon: String },
    Mismatch { client: String, daemon: String },
    LegacyDaemon { client: String },
}

impl BuildHealth {
    /// Classify one authenticated daemon greeting against this client.
    pub fn from_hello(hello: &cyclops_proto::Hello) -> Self {
        match hello.build.as_deref() {
            Some(daemon) if daemon == CLIENT_BUILD => BuildHealth::Current {
                client: CLIENT_BUILD.into(),
                daemon: daemon.into(),
            },
            Some(daemon) => BuildHealth::Mismatch {
                client: CLIENT_BUILD.into(),
                daemon: daemon.into(),
            },
            None => BuildHealth::LegacyDaemon {
                client: CLIENT_BUILD.into(),
            },
        }
    }

    /// Persistent health copy, absent only when the exact builds match.
    pub fn notice(&self) -> Option<String> {
        match self {
            BuildHealth::Current { .. } => None,
            BuildHealth::Mismatch { client, daemon } => Some(format!(
                "build mismatch: client {client}, daemon {daemon}; restart cyclopsd"
            )),
            BuildHealth::LegacyDaemon { client } => Some(format!(
                "daemon build unavailable: client {client}; restart cyclopsd"
            )),
        }
    }
}
