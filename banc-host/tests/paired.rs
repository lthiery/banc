//! Compile-level exercise of `paired_suite!`: expands both the explicit
//! `device_test:` form and the derived-name form. Off-rig these report
//! ignored like every banc suite; the macro wiring itself is what's under
//! test here.

use banc_host::DeviceSuite;

banc_host::paired_suite! {
    device_suite: DeviceSuite::new(std::env::temp_dir(), "join"),
    scenario explicit_device_name, device_test: "tests::something_else", |cx, device| {
        cx.evidence.record("paired", format!("runs {}", device.test_path()));
        device.start();
        device.verdict().await?;
    }
    scenario derived_device_name, |cx, device| {
        assert_eq!(device.test_path(), "tests::derived_device_name");
        cx.evidence.record("paired", "derived name checked");
        device.start();
    }
    scenario retried_scenario, attempts: 2, |cx, device| {
        cx.evidence.record("paired", format!("attempt {}", cx.attempt));
        device.start();
        device.verdict().await?;
    }
}
