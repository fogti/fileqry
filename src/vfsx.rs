use crossbeam_channel as chan;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, span, trace, warn, Level};

pub trait FileWatcher {
    fn watch(&self, path: &Path);
}

#[salsa::query_group(VfsDbStorage)]
pub trait VfsDb: salsa::Database + FileWatcher {
    fn read(&self, path: PathBuf) -> Option<String>;
}

fn read(db: &impl VfsDb, path: PathBuf) -> Option<String> {
    let span = span!(Level::DEBUG, "read", "{}", path.display());
    let _enter = span.enter();
    db.salsa_runtime()
        .report_synthetic_read(salsa::Durability::LOW);
    db.watch(&path);
    let data = std::fs::read_to_string(&path).ok();
    info!("{:?}", data);
    data
}

#[salsa::database(VfsDbStorage)]
pub struct MyDatabase {
    runtime: salsa::Runtime<MyDatabase>,
    paths_s: chan::Sender<PathBuf>,
    inv_r: chan::Receiver<PathBuf>,
    trm: std::cell::RefCell<HashMap<PathBuf, HashSet<PathBuf>>>,
}

#[derive(Debug)]
struct ModifyInfo {
    paths: Vec<PathBuf>,
    // if !is_direct, invalidate all childs, too
    is_direct: bool,
}

/// - paths_r: receiver for paths which should be watched
/// - inv_s: sender for paths which are invalidated
fn mangle_and_watch(paths_r: chan::Receiver<PathBuf>, inv_s: chan::Sender<PathBuf>) {
    let (modevs_s, modevs_r) = chan::bounded(2);
    let mut watcher = notify::immediate_watcher(move |ex: Result<notify::Event, notify::Error>| {
        let span = span!(Level::DEBUG, "(watch runtime)");
        let _enter = span.enter();
        match ex {
            Err(e) => error!("{:?}", e),
            Ok(mut event) => {
                use notify::event::*;
                trace!("{:?}", event);
                let is_direct = match &event.kind {
                    EventKind::Any => {
                        warn!("got 'any' event on paths = {:?}", event.paths);
                        false
                    }
                    EventKind::Create(CreateKind::File)
                    | EventKind::Modify(ModifyKind::Data(_))
                    | EventKind::Remove(RemoveKind::File) => true,
                    EventKind::Create(CreateKind::Any)
                    | EventKind::Modify(_)
                    | EventKind::Remove(RemoveKind::Any)
                    | EventKind::Remove(RemoveKind::Folder) => false,
                    // irrelevant event
                    _ => return,
                };
                let _ = modevs_s.send(ModifyInfo {
                    paths: std::mem::take(&mut event.paths),
                    is_direct,
                });
            }
        }
    })
    .expect("mangler: unable to initialize watcher");
    use notify::Watcher;
    let _ = watcher.configure(notify::Config::PreciseEvents(true));
    let mut wset = HashSet::new();
    let drecurm = notify::RecursiveMode::NonRecursive;
    let span = span!(Level::DEBUG, "(mangler)");
    let _enter = span.enter();
    loop {
        trace!("wset = {:?}", wset);
        chan::select! {
            recv(paths_r) -> path => {
                if !path.is_ok() {
                    warn!("input data channel closed");
                    return;
                }
                let path = path.unwrap();
                debug!("subscribe path = {}", path.display());
                for i in [path.parent(), Some(&path)].iter().filter_map(|i| *i) {
                    if wset.insert(i.to_path_buf()) {
                        if let Err(e) = watcher.watch(i, drecurm) {
                            error!("{:?}", e);
                            if let notify::ErrorKind::PathNotFound = e.kind {
                                wset.remove(i);
                                break;
                            }
                        }
                    }
                }
            },
            recv(modevs_r) -> modev => {
                let modev = modev.expect("mangler: watcher disappeared!");
                trace!("modev = {:?}", modev);
                let selected_paths = if modev.is_direct {
                    modev.paths
                } else {
                    wset.iter().filter(|i| modev.paths.iter().any(|j| i.starts_with(j))).map(PathBuf::from).collect::<Vec<_>>()
                };
                debug!("selected_paths = {:?}", selected_paths);
                for i in selected_paths {
                    if !wset.remove(&i) {
                        continue;
                    }
                    if let Err(e) = watcher.unwatch(&i) {
                        warn!("unwatch failed: {:?}", e);
                    }
                    if inv_s.send(i).is_err() {
                        // the main data channel is closed
                        warn!("output data channel closed");
                        return;
                    }
                }
            },
        }
    }
}

impl salsa::Database for MyDatabase {
    fn salsa_runtime(&self) -> &salsa::Runtime<MyDatabase> {
        &self.runtime
    }
    fn salsa_runtime_mut(&mut self) -> &mut salsa::Runtime<MyDatabase> {
        &mut self.runtime
    }
}

impl FileWatcher for MyDatabase {
    fn watch(&self, path: &Path) {
        fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
            Ok(path_clean::PathClean::clean(&if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::current_dir()?.join(path)
            }))
        }

        let span = span!(Level::DEBUG, "watch", "{}", path.display());
        let _enter = span.enter();

        let absp = match absolute_path(path) {
            Err(e) => {
                error!("absolute_path failed: {:?}", e);
                return;
            }
            Ok(x) => x,
        };

        if self.paths_s.send(absp.clone()).is_err() {
            error!("channel send failed");
        } else {
            self.trm
                .borrow_mut()
                .entry(absp)
                .or_default()
                .insert(path.to_path_buf());
        }
    }
}

impl MyDatabase {
    pub fn new() -> Self {
        let (paths_s, paths_r) = chan::bounded(1);
        let (inv_s, inv_r) = chan::unbounded();

        std::thread::spawn(move || mangle_and_watch(paths_r, inv_s));

        Self {
            runtime: Default::default(),
            trm: Default::default(),
            paths_s,
            inv_r,
        }
    }

    /// this method should be called after the UI waited for some time
    /// or before the next query
    #[allow(dead_code)]
    pub fn process_events(&mut self) {
        let evs: Vec<_> = self.inv_r.try_iter().collect();
        let span = span!(Level::DEBUG, "process_events");
        let _enter = span.enter();
        for path in evs {
            info!("{}: file changed", path.display());
            if let Some(x) = self.trm.get_mut().remove(&path) {
                for i in x {
                    salsa::Database::query_mut(self, ReadQuery).invalidate(&i);
                }
            }
        }
    }

    /// this method should be called while waiting (this method waits internally)
    /// the parameter `oth` can be optionally used to specify another
    /// crossbeam channel receiver, on which misc events for the application can be submitted.
    /// (the received message on that channel is returned)
    #[allow(dead_code)]
    pub fn process_events_until<T>(&mut self, dur: std::time::Duration, oth: Option<chan::Receiver<T>>) -> Option<T> {
        let span = span!(Level::DEBUG, "process_events_until", "dur={:?}", dur);
        let _enter = span.enter();
        let timeout = chan::after(dur);
        let oth = oth.unwrap_or(chan::never());

        loop {
            chan::select! {
                recv(self.inv_r) -> maybe_path => match maybe_path {
                    Err(_) => break,
                    Ok(path) => {
                        info!("{}: file changed", path.display());
                        if let Some(x) = self.trm.get_mut().remove(&path) {
                            for i in x {
                                salsa::Database::query_mut(self, ReadQuery).invalidate(&i);
                            }
                        }
                    },
                },
                recv(oth) -> msg => match msg {
                    Err(_) => break,
                    Ok(ret) => return Some(ret),
                },
                recv(timeout) -> _ => break,
            }
        }
        None
    }
}
