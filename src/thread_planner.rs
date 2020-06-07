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
    web::FromUi,
    ShutdownCall,
};

struct ThreadPlannerTransferState {
    rx: Receiver<FromUi>,
    sql_thread: SqlThread,
    state: Rc<RefCell<ThreadState>>,
    media_thread: Arc<MediaThread>,
}

pub struct ThreadPlannerBuilder {
    int_state: ThreadPlannerTransferState,
    tx: Sender<FromUi>,
}

impl ThreadPlannerBuilder {
    pub fn init(
        state: Rc<RefCell<ThreadState>>,
        sql_thread: SqlThread,
        media_thread: Arc<MediaThread>,
    ) -> Self {
        let (thread_planner_tx, thread_planner_rx) = mpsc::channel::<FromUi>(100);

        Self {
            tx: thread_planner_tx,
            int_state: ThreadPlannerTransferState {
                rx: thread_planner_rx,
                sql_thread,
                state,
                media_thread,
            },
        }
    }

    pub fn spawn(self, local_task_set: &LocalSet, shoutdown: ShutdownCall) -> ThreadPlanner {
        let state = self.int_state;

        local_task_set.spawn_local(async move {
            ThreadPlannerInt::new(state.sql_thread)
                .start(state.state, state.rx, state.media_thread, shoutdown)
                .await;
        });

        ThreadPlanner { tx: self.tx }
    }
}

// public api
pub struct ThreadPlanner {
    tx: Sender<FromUi>,
}

impl Clone for ThreadPlanner {
    fn clone(&self) -> Self {
        ThreadPlanner { tx: self.tx.clone() }
    }
}

impl ThreadPlanner {
    pub async fn add_thread(&mut self, url: &Url) -> Result<()> {
        self.tx.send(FromUi::AddThread(url.clone())).await?;
        Ok(())
    }
}

struct ThreadPlannerInt {
    sql: SqlThread,
    rx: Receiver<FromUi>,
}

impl ThreadPlannerInt {
    async fn spawn_thread_loader(
        &self,
        state: &Rc<RefCell<ThreadState>>,
        url: &DvachThreadUrl,
        media_thread: &Arc<MediaThread>,
    ) {
        let state = state.clone();

        let url = url.clone();

        println!("adding new thread from db");

        let db_tx = self.sql.clone();

        let media_thread = media_thread.clone();

        task::spawn_local(async move {
            thread_downloader(url, state, db_tx, media_thread).await;
        });
    }

    async fn add_new_threads_on_start(
        &mut self,
        state: &Rc<RefCell<ThreadState>>,
        media_thread: &Arc<MediaThread>,
    ) {
        let media = self.sql.get_threads(ThreadFilter::NotFinished).await;

        dbg!(&media);

        for url in media.threads.iter() {
            self.spawn_thread_loader(&state, &url, &media_thread).await;
        }
    }

    pub fn new(sql: SqlThread, rx: Receiver<FromUi>) -> Self {
        ThreadPlannerInt { sql, rx }
    }

    async fn add_thread(
        &mut self,
        state: &Rc<RefCell<ThreadState>>,
        url: &Url,
        media_thread: &Arc<MediaThread>,
    ) {
        let d_url = DvachThreadUrl::parse(&url).unwrap();
        let media = self.sql.get_threads(ThreadFilter::All).await;

        if media.threads.contains(&d_url) {
            println!("Already {}", &url);
        } else {
            println!("adding new thread {}", &d_url);
            self.sql.new_thread(&d_url).await.unwrap();
            self.spawn_thread_loader(&state, &d_url, &media_thread).await;
        }
    }
    pub async fn start(
        &mut self,
        state: Rc<RefCell<ThreadState>>,
        media_thread: Arc<MediaThread>,
        _shutdown: ShutdownCall,
    ) {
        let state = state.clone();

        // panic!("responce from db");
        // let mut threads: Vec<ThreadHandle> = vec![];

        self.add_new_threads_on_start(&state, &media_thread).await;

        loop {
            let media = self.rx.recv().await.unwrap();

            match media {
                FromUi::AddThread(url) => self.add_thread(&state, &url, &media_thread).await,
            }
        }

        // delay_for(Duration::new(5, 0)).await;
    }
}
