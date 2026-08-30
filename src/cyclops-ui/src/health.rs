//! Runtime identity health carried by the socket greeting.

/// The stream keeps the shared Hello classification in its model so a
/// mismatch remains visible after transient notices clear.
pub type BuildHealth = cyclops_client::HelloCompatibility;

/// Stream-UI presentation for the shared Hello classification.
pub fn notice(health: &BuildHealth) -> Option<String> {
    use cyclops_client::HelloCompatibility;

    match health {
        HelloCompatibility::Current { .. } => None,
        HelloCompatibility::Mismatch { client, daemon } => Some(format!(
            "version/build mismatch: cyclops {}, cyclopsd {} · continuing · run cyclops daemon restart; if they still differ, update or reinstall the older side",
            client.description(),
            daemon.description()
        )),
        HelloCompatibility::UnverifiedDaemon { client, daemon } => Some(format!(
            "daemon identity unverified: cyclops {}, cyclopsd {} · continuing · run cyclops daemon restart; if it remains unverified, update or reinstall the daemon",
            client.description(),
            daemon.description()
        )),
    }
}
