//! Relay sends: only the verification half (delivery checks, expiry, stray accounting) lives here.

mod verify;

pub use verify::{
    HEADERLESS_DETAIL, REFLOWED_DETAIL, build_message, check_delivery, delivery_checker,
    delivery_match, expire_sends, normalize, parse_js_date, sha256_hex,
};
