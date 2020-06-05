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

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use futures::{future, stream::StreamExt, FutureExt};
use hyper::{header, Body, Method, Request, Response, Server};
//     pub fn get(&self) -> *mut T {
//         self.0.get()
//     }
// }
use hyper::service::{make_service_fn, service_fn};
use serde::{de::Deserializer, Deserialize, Serialize};
// use serde::Deserialize;
use serde_json::{json, value::Value};
use tokio::{
    runtime,
    sync::{
        mpsc,
        mpsc::{Receiver, Sender},
    },
    task,
    task::{spawn_local, LocalSet},
    time::delay_for,
};
use url::{ParseError, Url};
use warp::{Filter, Reply};

use crate::{
    db_async::{DbRequest, SqlThread, ThreadFilter, ThreadsFromStart},
    media_downloader::MediaThread,
    sql::sql_thread_runner,
    state::{DvachThreadUrl, ThreadState},
    thread_downloader::thread_downloader,
    ShutdownCall,
};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AddThread {
    url: String,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(untagged)]
pub enum FromWeb {
    AddThread(AddThread),
    Stop,
    Ping,
}

impl FromWeb {
    fn parse_command(val: Value) -> FromWeb {
        let command = val.get("command").unwrap().as_str().unwrap();

        let params = val.get("params").unwrap().clone();

        match command {
            "add_thread" => {
                let tcommand = FromWeb::AddThread(serde_json::from_value(params).unwrap());
                dbg!(&tcommand);
                tcommand
            }

            "stop" => {
                let tcommand = FromWeb::Stop;
                dbg!(&tcommand);
                tcommand
            }
            _ => panic!("bad command"),
        }
    }
}

#[derive(Debug)]
pub enum FromUi {
    AddThread(Url),
    // Ping,
}

async fn spawn_thread_loader(
    state: &Rc<RefCell<ThreadState>>,
    url: &DvachThreadUrl,
    db_tx: &SqlThread,
    media_thread: &Arc<MediaThread>,
) {
    let state = state.clone();

    let url = url.clone();

    println!("adding new thread from db");

    let db_tx = db_tx.clone();

    let media_thread = media_thread.clone();

    task::spawn_local(async move {
        thread_downloader(url, state, db_tx, media_thread).await;
    });
}

// use tokio::task::LocalSet;

struct ThreadPlannerTransferState {
    rx: Receiver<FromUi>,
    sql_thread: SqlThread,
    state: Rc<RefCell<ThreadState>>,
    media_thread: Arc<MediaThread>,
}

pub struct ThreadPlanner {
    tx: Sender<FromUi>,
    int_state: Option<ThreadPlannerTransferState>,
}

unsafe impl Send for ThreadPlanner {}
unsafe impl Sync for ThreadPlanner {}

impl Clone for ThreadPlanner {
    fn clone(&self) -> Self {
        ThreadPlanner { tx: self.tx.clone(), int_state: None }
    }
}

impl ThreadPlanner {
    pub fn init(
        state: Rc<RefCell<ThreadState>>,
        sql_thread: SqlThread,
        media_thread: Arc<MediaThread>,
    ) -> Self {
        let (thread_planner_tx, thread_planner_rx) = mpsc::channel::<FromUi>(100);

        Self {
            tx: thread_planner_tx,
            int_state: Some(ThreadPlannerTransferState {
                rx: thread_planner_rx,
                sql_thread,
                state,
                media_thread,
            }),
        }
    }

    pub fn spawn(&mut self, local_task_set: &LocalSet, shoutdown: ShutdownCall) {
        let state = self.int_state.take().unwrap();

        local_task_set.spawn_local(async move {
            thread_planner(state.state, state.rx, state.sql_thread, state.media_thread, shoutdown)
                .await;
        });
    }

    pub async fn add_thread(&mut self, url: &Url) {
        self.tx.send(FromUi::AddThread(url.clone())).await.unwrap();
    }
}

pub async fn thread_planner(
    state: Rc<RefCell<ThreadState>>,
    receiver: Receiver<FromUi>,
    sql: SqlThread,
    media_thread: Arc<MediaThread>,
    _shutdown: ShutdownCall,
) {
    let state = state.clone();

    let mut receiver = receiver;
    let mut sql = sql;

    {
        let media = sql.get_threads(ThreadFilter::NotFinished).await;

        dbg!(&media);

        for url in media.threads.iter() {
            spawn_thread_loader(&state, &url, &sql, &media_thread).await;
        }
    }

    // panic!("responce from db");
    // let mut threads: Vec<ThreadHandle> = vec![];

    loop {
        let media = receiver.recv().await.unwrap();

        match media {
            FromUi::AddThread(url) => {
                let d_url = DvachThreadUrl::parse(&url).unwrap();
                let media = sql.get_threads(ThreadFilter::All).await;

                if media.threads.contains(&d_url) {
                    println!("Already {}", &url);
                    break;
                }

                println!("adding new thread {}", &d_url);

                sql.new_thread(&d_url).await.unwrap();

                spawn_thread_loader(&state, &d_url, &sql, &media_thread).await;
            }
        }

        println!("new thread");

        delay_for(Duration::new(5, 0)).await;
    }
}

async fn web_request(
    tx: ThreadPlanner,
    sql: SqlThread,
    shoutdown: ShutdownCall,
    val: Value,
) -> Result<impl warp::Reply, Infallible> {
    let yoba = val.to_string();

    let mut thread_planner = tx;
    let _tx_db = sql;

    let command = FromWeb::parse_command(val);

    let resp = match command {
        FromWeb::AddThread(raw) => {
            let mut url = Url::parse(&raw.url).unwrap();
            url.set_fragment(None);
            thread_planner.add_thread(&url).await;

            let _d_url = DvachThreadUrl::parse(&url).unwrap();

            Response::builder().body(format!("added"))
        }
        FromWeb::Ping => Response::builder().body(format!("pong")),

        FromWeb::Stop => {
            shoutdown.stop();
            Response::builder().body(format!("stopping"))
        }
    };

    // val.tx.send(FromUi::AddThread(thread_url)).await.unwrap();

    println!("{}", yoba);
    Ok(resp)
}

// use crate::ShotdownCall;

// use hyper::{header, Body, Method, Request, Response, Server};

// async fn warp_handler(
//     tx: ThreadPlanner,
//     // sql: SqlThread,
//     // shoutdown: ShotdownCall,
//     val: Value,
// ) -> Result<impl warp::Reply, Infallible> {
//     Ok(Response::builder().body(format!("added")))
// }

pub async fn web_backend(thread_planner: ThreadPlanner, db: SqlThread, shoutdown: ShutdownCall) {
    let tp = thread_planner.clone();
    let db = db.clone();
    let shoutdown = shoutdown.clone();

    let hello = warp::path!("hello")
        .and(warp::any().map(move || tp.clone()))
        .and(warp::any().map(move || db.clone()))
        .and(warp::any().map(move || shoutdown.clone()))
        .and(warp::body::json())
        // .and_then(warp_handler);
        .and_then(web_request);
    println!("web server starts");
    warp::serve(hello).run(([127, 0, 0, 1], 3030)).await;
}
