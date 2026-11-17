//! Its own test binary: hardening changes the whole process, for good.

#[cfg(target_os = "linux")]
#[test]
fn serve_hardening_makes_the_process_non_dumpable() {
    use rustix::process::{DumpableBehavior, Resource, dumpable_behavior, getrlimit};
    assert!(
        std::env::var_os(fronde::hardening::ALLOW_CORE_DUMPS).is_none(),
        "unset {} to run this test",
        fronde::hardening::ALLOW_CORE_DUMPS
    );
    fronde::hardening::harden().unwrap();
    // prctl(PR_GET_DUMPABLE) returns 0.
    assert_eq!(dumpable_behavior().unwrap(), DumpableBehavior::NotDumpable);
    let core = getrlimit(Resource::Core);
    assert_eq!(core.current, Some(0));
    assert_eq!(core.maximum, Some(0));
}
