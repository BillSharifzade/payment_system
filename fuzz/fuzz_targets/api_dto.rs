//! The public request DTOs the API deserialises from untrusted JSON (those reachable from
//! outside the crate) never panic, a response DTO round-trips byte for byte, and
//! `DEVICE_BINDING` parses only its three names.
#![no_main]

use api::biometric::{
    CreateCheckRequest, EnrollRequest, ListChecksParams, PayCheckRequest, PayCheckResponse,
    ProbeRequest,
};
use api::devices::RegisterDeviceRequest;
use api::DeviceBinding;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = serde_json::from_slice::<EnrollRequest>(data);
    let _ = serde_json::from_slice::<CreateCheckRequest>(data);
    let _ = serde_json::from_slice::<ListChecksParams>(data);
    let _ = serde_json::from_slice::<ProbeRequest>(data);
    let _ = serde_json::from_slice::<PayCheckRequest>(data);
    let _ = serde_json::from_slice::<RegisterDeviceRequest>(data);
    if let Ok(r) = serde_json::from_slice::<PayCheckResponse>(data) {
        let once = serde_json::to_vec(&r).unwrap();
        let again: PayCheckResponse = serde_json::from_slice(&once).unwrap();
        assert_eq!(serde_json::to_vec(&again).unwrap(), once);
    }
    if let Ok(s) = std::str::from_utf8(data) {
        match s.parse::<DeviceBinding>() {
            Ok(b) => assert_eq!(b.name(), s),
            Err(()) => assert!(!["required", "optional", "off"].contains(&s)),
        }
    }
});
