mod vfsx;

use crate::vfsx::VfsDb;
use std::{path::PathBuf, thread::sleep, time::Duration};

fn main() {
    let mut db = vfsx::MyDatabase::new();
    loop {
        sleep(Duration::from_secs(1));
        db.process_events();
        db.read(PathBuf::from("test.txt"));
    }
}
