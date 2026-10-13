//! Fuzzes the controller's input path: Kubernetes manifests (Gateways,
//! routes, policies, Services, EndpointSlices) parsed into a `World` and
//! translated into per-Gateway configs. Route authors control most of this
//! input, so it must never panic, and every config the translator marks
//! valid must also pass the proxy's validation.

#![no_main]

use ferry_config::SnapshotVersion;
use ferry_controller::translate::{Settings, translate};
use ferry_controller::world::World;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(world) = World::from_manifests(text) else {
        return;
    };
    let settings = Settings {
        controller_name: "gateway.ferry.dev/controller".into(),
        proxy_protocol: false,
    };
    let t = translate(&world, &settings);
    for (key, g) in &t.gateways {
        if g.valid {
            assert!(
                g.config.clone().into_snapshot(SnapshotVersion(1)).is_ok(),
                "translator marked {key} valid but the proxy would reject it"
            );
        }
    }
});
