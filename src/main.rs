mod vfsx;

use crate::vfsx::VfsDb;
use std::{path::PathBuf, time::Duration};

fn main() {
    tracing_subscriber::fmt::init();

    let mut db = vfsx::MyDatabase::new();
    loop {
        db.process_events_until::<()>(Duration::from_millis(200), None);
        db.read(PathBuf::from("test.txt"));
    }
}
