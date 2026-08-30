use cyclops_proto::{BUILD_ID, BUILD_REF, VERSION_WITH_BUILD};

#[test]
fn one_build_stamp_composes_with_the_cargo_workspace_version() {
    assert!(!BUILD_REF.is_empty());
    assert!(!BUILD_ID.is_empty());
    assert_eq!(
        VERSION_WITH_BUILD,
        format!("{} ({BUILD_REF})", env!("CARGO_PKG_VERSION"))
    );
}
