#![deny(unsafe_code)]

mod vfsx;

use crate::vfsx::VfsDb;
use std::path::PathBuf;
//use std::time::Duration;

fn main() {
    tracing_subscriber::fmt::init();

    let mut db = vfsx::MyDatabase::new();
    loop {
        db.read(PathBuf::from("test.txt"));
        //db.process_events_until::<()>(Duration::from_millis(200), None);
        if !db.process_events_blocking() {
            return;
        }
    }
}
