use crossbeam_channel as chan;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use tracing::{debug, error, info, span, trace, warn, Level};

pub trait FileWatcher {
    fn watch(&self, path: &Path);
}

#[salsa_inline_query::salsa_inline_query]
#[salsa::query_group(VfsDbStorage)]
pub trait VfsDb: salsa::Database + FileWatcher {
    fn read(&self, path: PathBuf) -> Option<String> {
        let span = span!(Level::DEBUG, "read", "{}", path.display());
        let _enter = span.enter();
        self.salsa_runtime()
            .report_synthetic_read(salsa::Durability::LOW);
        self.watch(&path);
        let data = std::fs::read_to_string(&path).ok();
        info!("{:?}", data);
        data
    }
}

#[derive(Debug)]
struct ModifyInfo {
    paths: Vec<PathBuf>,
    // if !is_direct, invalidate all childs, too
    is_direct: bool,
}

struct ManglerX {
    watcher: notify::RecommendedWatcher,
    trm: HashMap<PathBuf, HashSet<PathBuf>>,
    wset: HashSet<PathBuf>,
}

#[salsa::database(VfsDbStorage)]
pub struct MyDatabase {
    runtime: salsa::Runtime<MyDatabase>,
    modevs_r: chan::Receiver<ModifyInfo>,
    mangler: std::cell::RefCell<ManglerX>,
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

        let span = span!(Level::DEBUG, "watch");
        let _enter = span.enter();

        let orig_path = path;
        let path = match absolute_path(path) {
            Err(e) => {
                error!("absolute_path failed: {:?}", e);
                return;
            }
            Ok(x) => x,
        };

        let mut mangler = self.mangler.borrow_mut();

        debug!("{}", path.display());
        for i in [path.parent(), Some(&path)].iter().filter_map(|i| *i) {
            if mangler.wset.insert(i.to_path_buf()) {
                use notify::Watcher;
                if let Err(e) = mangler
                    .watcher
                    .watch(i, notify::RecursiveMode::NonRecursive)
                {
                    error!("{:?}", e);
                    if let notify::ErrorKind::PathNotFound = e.kind {
                        mangler.wset.remove(i);
                        break;
                    }
                }
            }
        }
        mangler
            .trm
            .entry(path)
            .or_default()
            .insert(orig_path.to_path_buf());
    }
}

impl MyDatabase {
    pub fn new() -> Self {
        let (modevs_s, modevs_r) = chan::bounded(2);
        let mut watcher =
            notify::immediate_watcher(move |ex: Result<notify::Event, notify::Error>| {
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

        let mangler = std::cell::RefCell::new(ManglerX {
            watcher,
            trm: Default::default(),
            wset: Default::default(),
        });

        Self {
            runtime: Default::default(),
            modevs_r,
            mangler,
        }
    }

    fn process_events_intern(
        &mut self,
        evs: impl std::iter::IntoIterator<Item = ModifyInfo> + std::fmt::Debug,
    ) {
        let span = span!(Level::DEBUG, "process_events_intern");
        let _enter = span.enter();

        let mangler = self.mangler.get_mut();
        let mut selected_paths = HashSet::new();
        let mut real_paths = Vec::new();

        trace!("wset = {:?}, evs = {:?}", mangler.wset, evs);

        for modev in evs {
            selected_paths.extend(if modev.is_direct {
                modev.paths
            } else {
                mangler
                    .wset
                    .iter()
                    .filter(|i| modev.paths.iter().any(|j| i.starts_with(j)))
                    .map(PathBuf::from)
                    .collect::<Vec<_>>()
            });
        }

        debug!("selected_paths = {:?}", selected_paths);

        for i in selected_paths {
            use notify::Watcher;
            if !mangler.wset.remove(&i) {
                continue;
            }
            info!("{}: file changed", i.display());
            if let Err(e) = mangler.watcher.unwatch(&i) {
                warn!("unwatch failed: {:?}", e);
            }
            if let Some(x) = mangler.trm.remove(&i) {
                real_paths.extend(x.into_iter());
            }
        }

        real_paths.sort();
        real_paths.dedup();
        for i in real_paths {
            salsa::Database::query_mut(self, ReadQuery).invalidate(&i);
        }
    }

    /// this method should be called after the UI waited for some time
    /// or before the next query
    #[allow(dead_code)]
    pub fn process_events(&mut self) {
        let span = span!(Level::DEBUG, "process_events");
        let _enter = span.enter();
        self.process_events_intern(self.modevs_r.try_iter().collect::<Vec<_>>());
    }

    /// this method should be called while waiting (this method waits internally)
    /// the parameter `oth` can be optionally used to specify another
    /// crossbeam channel receiver, on which misc events for the application can be submitted.
    /// (the received message on that channel is returned)
    #[allow(dead_code)]
    pub fn process_events_until<T>(
        &mut self,
        dur: std::time::Duration,
        oth: Option<chan::Receiver<T>>,
    ) -> Option<T> {
        let span = span!(Level::DEBUG, "process_events_until", "dur={:?}", dur);
        let _enter = span.enter();
        let timeout = chan::after(dur);
        let oth = oth.unwrap_or_else(chan::never);

        loop {
            chan::select! {
                recv(self.modevs_r) -> maybe_path => match maybe_path {
                    Err(_) => break,
                    Ok(path) => self.process_events_intern(vec![path]),
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

    /// in contrast to `process_events` and `process_events_until`, this method blocks
    /// when called and waits until at least one event comes available.
    /// if the result is `false`, the `db watcher thread` died and no more events
    /// will be emitted and processed in the future.
    #[allow(dead_code)]
    pub fn process_events_blocking(&mut self) -> bool {
        let span = span!(Level::DEBUG, "process_events_blocking");
        let _enter = span.enter();

        // 1. wait for one new event
        let first_ev = if let Ok(ev) = self.modevs_r.recv() {
            ev
        } else {
            return false;
        };

        // 2. wait for other events
        std::thread::sleep(std::time::Duration::from_millis(5));

        // 3. process events
        self.process_events_intern(
            self.modevs_r
                .try_iter()
                .chain(std::iter::once(first_ev))
                .collect::<Vec<_>>(),
        );

        true
    }
}
