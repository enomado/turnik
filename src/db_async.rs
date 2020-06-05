#![allow(dead_code)]

use std::{
    cell::{Cell, RefCell, UnsafeCell},
    convert::Infallible,
    env::current_dir,
    fmt::Display,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, LockResult, Mutex, MutexGuard},
    thread::sleep,
    time::Duration,
};

use anyhow::{bail, Context, Result};
use rusqlite::{self, params, Connection, Statement, NO_PARAMS};
use serde::{Deserialize, Serialize};
use tokio::{
    runtime,
    sync::{
        mpsc,
        mpsc::{Receiver, Sender},
        oneshot,
    },
    task,
    task::{spawn_local, LocalSet},
    time::delay_for,
};
use url::Url;

use crate::state::{DvachThreadUrl, MediaEntry, ThreadEntry};

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ThreadsFromStart {
    pub threads: Vec<DvachThreadUrl>,
}

#[derive(Debug)]
pub enum ThreadFilter {
    NotFinished,
    All,
}

pub struct SqlThread {
    pub tx: Sender<DbRequest>,
}

impl Clone for SqlThread {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
}

impl SqlThread {
    pub async fn get_threads(&mut self, filter: ThreadFilter) -> ThreadsFromStart {
        let (from_db_tx, from_db_rx) = oneshot::channel::<ThreadsFromStart>();
        self.tx.send(DbRequest::GetThreads(from_db_tx, filter)).await.context("error").unwrap();
        let media: ThreadsFromStart = from_db_rx.await.unwrap();

        media
    }

    pub async fn new_media(&mut self, media: &MediaEntry) -> Result<()> {
        self.tx.send(DbRequest::NewMedia(Clone::clone(&media))).await?;
        Ok(())
    }

    pub async fn new_thread(&mut self, url: &DvachThreadUrl) -> Result<()> {
        self.tx.send(DbRequest::NewThread(url.clone())).await.context("err").unwrap();
        Ok(())
    }

    pub async fn finish_thread(&mut self, thread_url: &DvachThreadUrl) -> Result<()> {
        self.tx.send(DbRequest::ThreadFinished(thread_url.clone())).await.context("err").unwrap();
        Ok(())
    }
}

#[derive(Debug)]
pub enum DbRequest {
    GetThreads(oneshot::Sender<ThreadsFromStart>, ThreadFilter),
    //
    NewThread(DvachThreadUrl),
    ThreadFinished(DvachThreadUrl),
    //
    NewMedia(MediaEntry),
    MediaFinished(MediaEntry),
}
