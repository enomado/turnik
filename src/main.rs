// #![allow(dead_code)]
#![allow(unused_imports)]
// #![allow(warnings)]

#[macro_use]
extern crate lazy_static;
#[allow(unused_imports)]
extern crate serde;
#[macro_use]
extern crate serde_json;

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    convert::Infallible,
    env::current_dir,
    fmt::Display,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, LockResult, Mutex, MutexGuard},
    time::Duration,
};

use anyhow::Result;
use chrono::{DateTime, Utc};
use futures::{future, stream::StreamExt, FutureExt};
use serde::{de::Deserializer, Deserialize, Serialize};
use serde_json::{json, value::Value};
use tokio::{
    runtime,
    sync::{
        broadcast, mpsc,
        mpsc::{Receiver, Sender},
    },
    task,
    task::{spawn_local, LocalSet},
    time::delay_for,
};
use url::{ParseError, Url};
use warp::{Filter, Reply};

use media_downloader::MediaThread;
use state::ThreadState;
use thread_downloader::thread_downloader;

use crate::{
    db_async::{DbRequest, SqlThread, ThreadsFromStart},
    sql::{sql_thread_runner, DbState},
    state::DvachThreadUrl,
    web::{web_backend, FromUi, ThreadPlanner, ThreadPlannerBuilder},
};

use toml;

mod db_async;
mod media_downloader;
mod sql;
mod state;
mod thread_downloader;
mod web;

pub struct Config {
    pub download_path: PathBuf,
    pub database_path: PathBuf,
}

#[derive(Deserialize)]
struct TomlConfig {
    download_path: String,
}

impl TomlConfig {
    pub fn parse(path: &PathBuf) -> Result<Self> {
        use std::fs::read_to_string;

        let file_contents = read_to_string(&path)?;

        let config: TomlConfig = toml::from_str(file_contents.as_str())?;

        Ok(config)
    }
}

fn get_config() -> Result<Config> {
    let current_dir = current_dir().unwrap();
    println!("current dir is {}", current_dir.to_str().unwrap());

    let config_path = current_dir.join("turnik.toml");

    println!("config path: {}", &config_path.to_str().unwrap());

    let toml_config = TomlConfig::parse(&config_path)?;

    let dl_folder = PathBuf::from(toml_config.download_path);

    let _ = dl_folder.exists() || panic!("folder does not exists");

    // download_folder.metadata()

    // let md = metadata(".").unwrap();
    let md = dl_folder.metadata().unwrap();
    println!("is dir: {}", md.is_dir());
    println!("is file: {}", md.is_file());

    let db_path = current_dir.join("db");

    Ok(Config { download_path: dl_folder, database_path: db_path })
}

fn spawn_sql_thread(config: &Config, shotdown: ShutdownCall) -> SqlThread {
    let (db_tx, db_rx) = mpsc::channel::<DbRequest>(10);

    let db_path = config.download_path.join("threads.db");

    let _db_thread = tokio::task::spawn_blocking(move || sql_thread(db_path, db_rx, shotdown));

    SqlThread { tx: db_tx }
}
fn sql_thread(
    db_filename: PathBuf,
    receiver: Receiver<DbRequest>,
    shotdown: ShutdownCall,
) -> Result<()> {
    let rt = tokio::runtime::Handle::current();
    let local = tokio::task::LocalSet::new();

    let db_state = DbState::sql_init(db_filename).unwrap();

    println!("blocked thread spawned");

    rt.block_on(async {
        local.run_until(async { sql_thread_runner(db_state, receiver, shotdown).await }).await;
    });

    println!("blocked finished");

    Ok(())
}

#[derive(Debug)]
pub struct ShutdownCall {
    tx: broadcast::Sender<bool>,
    rx: broadcast::Receiver<bool>,
}

impl Clone for ShutdownCall {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone(), rx: self.tx.subscribe() }
    }
}

impl ShutdownCall {
    pub fn new() -> Self {
        let (panic_tx, panic_rx) = broadcast::channel::<bool>(1);
        ShutdownCall { tx: panic_tx, rx: panic_rx }
    }

    fn check_shotdown(&mut self) -> bool {
        // use tokio::sync::mpsc::error::TryRecvError;
        use tokio::sync::broadcast::TryRecvError;
        // let mut shotdown = chan.borrow_mut();
        let res = self.rx.try_recv();

        let to_ret = match res {
            Err(TryRecvError::Closed) => {
                println!("channel closed, shuting down");
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Lagged(_)) => false,
            Ok(_x) => true,
        };
        to_ret
    }

    pub fn stop(&self) {
        self.tx.send(true).unwrap();
    }

    pub async fn recv(&mut self) -> bool {
        self.rx.recv().await.unwrap();
        true
    }

    // pub fn try_recv(&mut self) -> bool {
    //     self.rx.try_recv()
    // }
}

#[tokio::main]
async fn main() {
    let config = get_config().unwrap();

    let state = ThreadState::new(&config.download_path);
    let state = Rc::new(RefCell::new(state));

    println!("hi");

    let shoutdown = ShutdownCall::new();

    let sql_thread = spawn_sql_thread(&config, shoutdown.clone());

    let local_task_set = task::LocalSet::new();

    let mut media_thread_raw = MediaThread::init(&state);
    media_thread_raw.spawn(&local_task_set, shoutdown.clone());

    let media_thread = Arc::new(media_thread_raw);

    let thread_planner_builder =
        ThreadPlannerBuilder::init(state.clone(), sql_thread.clone(), media_thread.clone());
    let thread_planner = thread_planner_builder.spawn(&local_task_set, shoutdown.clone());

    let shoutdown = shoutdown.clone();
    local_task_set
        .spawn_local(async move { web_backend(thread_planner, sql_thread, shoutdown).await });

    println!("ho");

    let _state = state.clone();

    local_task_set.await;
}
