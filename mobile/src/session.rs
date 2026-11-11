//! Mobile adapter for Blip's shared encrypted session store.

pub mod store {
    include!(concat!(env!("OUT_DIR"), "/session_store.rs"));
}
