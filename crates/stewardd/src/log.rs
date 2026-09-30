//! Daemon logging: `say!(0, …)` always prints, `say!(1, …)` with `-v`,
//! `say!(2, …)` with `-vv`. Lines go to stderr, prefixed `stewardd: `.

use std::sync::atomic::{AtomicU8, Ordering};

static LEVEL: AtomicU8 = AtomicU8::new(0);

pub fn set_verbosity(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn enabled(level: u8) -> bool {
    LEVEL.load(Ordering::Relaxed) >= level
}

#[macro_export]
macro_rules! say {
    ($level:expr, $($arg:tt)*) => {
        if $crate::log::enabled($level) {
            eprintln!("stewardd: {}", format_args!($($arg)*));
        }
    };
}
