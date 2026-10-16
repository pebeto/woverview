//! `WOVERVIEW_DEBUG=1` logging. Every line carries the monotonic clock in
//! milliseconds, so a script can line it up with when it pressed the key.

use std::fmt;
use std::sync::OnceLock;

use rustix::time::{ClockId, clock_gettime};

pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("WOVERVIEW_DEBUG").is_some())
}

pub fn print(message: fmt::Arguments) {
    let now = clock_gettime(ClockId::Monotonic);
    let ms = now.tv_sec as f64 * 1e3 + now.tv_nsec as f64 / 1e6;
    eprintln!("woverview: [{ms:.1}] {message}");
}

macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::debug::enabled() {
            $crate::debug::print(format_args!($($arg)*));
        }
    };
}

pub(crate) use debug;
