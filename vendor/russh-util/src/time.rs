#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

#[cfg(target_arch = "wasm32")]
pub use wasm::Instant;

#[cfg(target_arch = "wasm32")]
mod wasm {
    #[derive(Debug, Clone, Copy)]
    pub struct Instant {
        inner_ms: f64,
    }

    impl Instant {
        pub fn now() -> Self {
            Instant {
                inner_ms: js_sys::Date::now(),
            }
        }

        pub fn duration_since(&self, earlier: Instant) -> std::time::Duration {
            std::time::Duration::from_secs_f64((self.inner_ms - earlier.inner_ms) / 1_000.0)
        }
    }
}
