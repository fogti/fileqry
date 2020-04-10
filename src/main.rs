mod vfsx;

use crate::vfsx::VfsDb;
use std::{path::PathBuf, thread::sleep, time::Duration};

fn main() {
    tracing_subscriber::fmt::init();

    let mut db = vfsx::MyDatabase::new();
    loop {
        sleep(Duration::from_secs(1));
        db.process_events();
        db.read(PathBuf::from("test.txt"));
    }
}
