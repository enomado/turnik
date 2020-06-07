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

// use tokio::task::LocalSet;

async fn web_request(
    thread_planner: ThreadPlanner,
    sql: SqlThread,
    shoutdown: ShutdownCall,
    val: Value,
) -> Result<impl warp::Reply, Infallible> {
    let yoba = val.to_string();

    let mut thread_planner = thread_planner;
    let _tx_db = sql;

    let command = FromWeb::parse_command(val);

    let resp = match command {
        FromWeb::AddThread(raw) => {
            let url = Url::parse(&raw.url);
            match url {
                Ok(url) => {
                    let mut url = url;
                    url.set_fragment(None);
                    thread_planner.add_thread(&url).await.unwrap();

                    let _d_url = DvachThreadUrl::parse(&url).unwrap();

                    Response::builder().body(format!("added"))
                }
                Err(err) => Response::builder().body(format!("not valid url: {}", err)),
            }
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

use crate::thread_planner::ThreadPlanner;

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
