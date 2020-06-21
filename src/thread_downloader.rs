#![allow(dead_code)]
// #![allow(unused_imports)]
// #![allow(warnings)]

extern crate lazy_static;

use std::{
    cell::{Cell, Ref, RefCell, UnsafeCell},
    collections::{HashMap, HashSet},
};
// use std::borrow::Borrow;
use std::{
    io::{prelude::*, Bytes, Read},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, LockResult, Mutex, MutexGuard},
    thread, time,
    time::Duration,
};

use anyhow::{bail, Context, Error, Result};
use chrono::{DateTime, Utc};
use futures::stream::TryStreamExt;
use futures_util::StreamExt;
use http::StatusCode;
use regex::Regex;
use reqwest::{
    header::{IF_MODIFIED_SINCE, LAST_MODIFIED},
    Client, Response,
};
use select::{document::Document, predicate::Name};
use serde_json::value::Value;
use tokio::{
    fs::{create_dir_all, File},
    io::AsyncWriteExt,
    sync::mpsc::{channel, channel as tokio_channel, Receiver, Sender},
    task::yield_now,
    time::delay_for,
};
use url::{Host, ParseError, Position, Url};

use crate::{
    db_async::{DbRequest, SqlThread},
    media_downloader::MediaThread,
    state::{DlEntryStatus, DvachThreadUrl, MediaEntry, ThreadState, UrlFile},
};

fn parse_thread_vec_cached(doc: &Document) -> Vec<UrlFile> {
    let root_url = Url::parse("https://2ch.hk").unwrap();

    let mut url_cache = HashSet::with_capacity(100);

    let u = doc
        .find(Name("a"))
        .filter_map(|n| n.attr("href"))
        .filter_map(|x| {
            if url_cache.contains(x) {
                None
            } else {
                url_cache.insert(x);
                Some(x)
            }
        })
        .map(|x| root_url.join(x).unwrap())
        .filter_map(|url| UrlFile::from_url_or_none(url))
        .collect::<Vec<_>>();
    // .collect();

    // let y = Url::parse("https://2ch.hk/b/res/218447265.html").unwrap();
    u
}

// struct ReadableResponse(Response);

// impl Read for ReadableResponse {
//     fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
//         use futures_util::io::AsyncReadExt;

//         let timeout = 20u32;
//         self.0.body_mut().read(buf)
//     }
// }

#[test]
fn regexp_test() {
    let file_ends = Regex::new(r"(.+?)/arch/(.+?)$").unwrap();

    assert!(file_ends.is_match("/b/ardch/2020-05-14/res/220208847.html"));
}

// fn thread_url_is_archived(url: &Url) -> bool {
//     assert!(url.host() == Some(Host::Domain("2ch.hk")));

//     let path = url.path();

//     lazy_static! {
//         static ref FILE_ENDS: Regex =
// Regex::new(r"(.+?)/arch/(.+?)$").unwrap();     }

//     FILE_ENDS.is_match(path)
// }

struct RespOk {
    page_body: String,
    // json_body: String,
    media_urls: Vec<UrlFile>,
}

enum RespType {
    Ok(RespOk),
    NotFound,
    NotModified,
    BadGateway,
}

struct ThreadResult {
    resp: RespType,
    is_archived: bool,
    last_modified: Option<String>,
    /* еще не успел редиректнуться в архив
     * NotFound */
}

// use anyhow::Result;

async fn download_thread_and_get_urls(client_wrap: &ClientWrap, url: &str) -> Result<ThreadResult> {
    let resp = {
        let client = client_wrap.client.get(url);

        let client = if let Some(modifed) = &client_wrap.last_modifed {
            client.header(IF_MODIFIED_SINCE, modifed)
        } else {
            client
        };

        client.send().await?
    };

    let status = resp.status();

    println!("resp code is {}. url: {}", status, &url);

    if status == StatusCode::NOT_MODIFIED {
        return Ok(ThreadResult {
            resp: RespType::NotModified,
            is_archived: false,
            last_modified: client_wrap.last_modifed.clone(),
        });
    };

    if status == StatusCode::NOT_FOUND {
        println!("thread not found {}", &url);
        return Ok(ThreadResult {
            resp: RespType::NotFound,
            is_archived: false,
            last_modified: client_wrap.last_modifed.clone(),
        });
    };

    if status == StatusCode::BAD_GATEWAY {
        println!("bad gateway {}", &url);
        return Ok(ThreadResult {
            resp: RespType::BadGateway,
            is_archived: false,
            last_modified: client_wrap.last_modifed.clone(),
        });
    };

    // TODO: hangle 502 with no attempt increment
    assert!(resp.status().is_success());

    let last_modified: Option<String> = resp
        //
        .headers()
        .get(LAST_MODIFIED)
        .and_then(|x| x.to_str().ok().map(|x| x.to_owned()));

    let final_url = DvachThreadUrl::parse(resp.url())?;
    let thread_is_arhived = final_url.is_archive;

    let page_text: String = resp.text().await?;

    let urls = {
        // let rresp = resp.bytes().await?;
        // let doc = Document::from_read(rresp.as_ref()).unwrap();

        let doc = Document::from(page_text.as_ref());
        parse_thread_vec_cached(&doc)
    };

    Ok(ThreadResult {
        resp: RespType::Ok(RespOk { page_body: page_text, media_urls: urls }),
        is_archived: thread_is_arhived,
        last_modified,
    })
}

async fn plan_media(
    state: &RefCell<ThreadState>,
    urls: &Vec<UrlFile>,
    thread_url: &DvachThreadUrl,
    media_thread: &Arc<MediaThread>,
    sql: &mut SqlThread,
) -> Result<()> {
    let values = urls.iter().map(|o| {
        let entry = MediaEntry {
            url: o.url.to_string(),
            state: DlEntryStatus::JustAdded,
            thread: thread_url.clone(),
            url_filename: o.url_filename.clone(),
        };
        let aentry = Arc::new(entry);
        aentry
    });

    {
        for val in values {
            let is_new = {
                let mut state = state.borrow_mut();
                let is_new = state.plan_media(&val);
                is_new
            };

            if is_new {
                sql.new_media(&val).await?;
            }
        }

        media_thread.notify_new_media().await?;

        Ok(())
    }

    // for val in values {
    //     media_channel.send(val).await.unwrap();
    // }
}

async fn download_thread_json(thread_url: &DvachThreadUrl) -> (Vec<UrlFile>, bool) {
    let resp: Value =
        reqwest::get(thread_url.json_form()).await.unwrap().json::<Value>().await.unwrap();

    let _max_num = resp.get("max_num").unwrap().as_u64().unwrap();

    let threads = get_data_json(&resp).unwrap();

    // dbg!("dfdfdf {}", resp);

    // assert!(resp.status().is_success());

    // let final_url = resp.url();
    // let thread_is_arhived = thread_url_is_archived(final_url);

    // let rresp = resp.bytes().await.unwrap();

    // let doc = Document::from_read(rresp.as_ref()).unwrap();

    // let last_max_num != get("max_num");

    // thread_first_image

    // ["threads"]["posts"].map(
    // post.path
    // )

    (threads, false)
}

fn get_data_json(obj: &Value) -> Result<Vec<UrlFile>> {
    let threads = obj.get("threads").context("no threads")?.as_array().context("not array")?;

    if threads.len() != 1 {
        bail!("threads more than one");
    }

    let thread: &Value = &threads[0];

    let posts = thread.get("posts").context("no")?.as_array().context("no")?;

    let thread_files = posts.iter().map(|post| {
        let files =
            post.get("files").context("no files").and_then(|s| s.as_array().context("not array"));
        files
    });

    let mut media_urls: Vec<Url> = Vec::with_capacity(posts.len());

    let root_url = Url::parse("https://2ch.hk")?;

    for thread_file in thread_files {
        let files = thread_file?;

        for file in files {
            let raw_url = file.get("path").context("path")?.as_str().context("bot string")?;

            let final_url = root_url.join(raw_url)?;
            media_urls.push(final_url);
        }
    }

    let urlfiles = media_urls
        .iter()
        .map(|url| {
            let urlfile = UrlFile::from_url_or_none(url.clone());
            if urlfile.is_none() {
                panic!("add url {}", &url);
            };
            urlfile.unwrap()
        })
        .collect::<Vec<_>>();

    // dbg!(&urlfiles);

    Ok(urlfiles)
}

// #[tokio::test]
// async fn parse_thread() {
//     let thread_url =
//         DvachThreadUrl::parse(&Url::parse("https://2ch.hk/soc/res/4219284.html#4219284").unwrap());

//     let (json_urls, _): (Vec<UrlFile>, bool) =
// download_thread_json(&thread_url).await;

//     let client = ClientWrap::new();

//     let result: ThreadResult =
//         download_thread_and_get_urls(&client,
// &thread_url.to_url_string()).await.unwrap();

//     let simple_url = result.resp.media_urls;

//     // dbg!(&simple_url);

//     let mut from_json = HashSet::new();

//     json_urls.iter().for_each(|u| {
//         from_json.insert(u.url.clone());
//         ()
//     });

//     let mut from_simple = HashSet::new();

//     simple_url.iter().for_each(|u| {
//         from_simple.insert(u.url.clone());
//         ()
//     });

//     let diff = from_json.symmetric_difference(&from_simple);

//     dbg!(diff);

//     let mut stderr = std::io::stderr();
//     stderr.flush();
// }

#[test]
fn test_url_filter() {
    let u = Url::parse("https://vk.com/ixxx.xxx.xxx.xxxi").unwrap();

    let tt = UrlFile::from_url_or_none(u);

    assert!(tt.is_none())
}

struct ClientWrap {
    pub client: Client,
    pub last_modifed: Option<String>,
}

impl ClientWrap {
    pub fn new() -> ClientWrap {
        ClientWrap { client: Client::new(), last_modifed: None }
    }
}

async fn save_index(thread_path: &Path, body: &String) -> Result<()> {
    let index_path = thread_path.join("index.html");

    let mut file = File::create(&index_path).await?;

    let file_str = index_path.to_str().unwrap();

    println!("saving index: {}", file_str);
    file.write_all(body.as_bytes()).await?;
    file.sync_all().await?;
    file.flush().await?;

    Ok(())
}

async fn save_json(
    thread_path: &Path,
    client: &ClientWrap,
    thread_url: &DvachThreadUrl,
) -> Result<()> {
    let raw_url = thread_url.json_form();

    println!("try {}", &raw_url);

    let resp = client.client.get(raw_url.as_ref()).send().await?;

    assert!(resp.status().is_success());

    let resp_text = resp.text().await?;

    let body = resp_text;

    let index_path = thread_path.join("index.json");

    let mut file = File::create(&index_path).await?;

    let file_str = index_path.to_str().unwrap();

    println!("saving json: {}", file_str);
    file.write_all(body.as_bytes()).await?;
    file.sync_all().await?;
    file.flush().await?;

    Ok(())
}

pub async fn thread_downloader(
    thread_url_raw: DvachThreadUrl,
    sstate: Rc<RefCell<ThreadState>>,
    db_tx: SqlThread,
    media_thread: Arc<MediaThread>,
) {
    // state.threads.insert(Rc::new(ThreadEntry::new(url.into())));
    // state.start_thread(thread_url);
    // let state = sstate.borrow();

    let mut sql = db_tx;

    let root_folder = { sstate.borrow().download_directory.clone() };

    let download_dir = thread_url_raw.local_fs_folder(&root_folder);

    create_dir_all(&download_dir).await.unwrap();

    let mut client = ClientWrap::new();

    let mut not_found_attempts: u32 = 0;

    loop {
        println!("attempts {}: {}", thread_url_raw.to_url_string(), not_found_attempts);
        // let res = download_thread_json(&thread_url_raw).await;
        let res: ThreadResult =
            download_thread_and_get_urls(&client, &thread_url_raw.to_url_string()).await.unwrap();

        client.last_modifed = res.last_modified;
        let is_archived = res.is_archived;

        dbg!("is_archived: {}", is_archived);

        match res.resp {
            RespType::Ok(resp) => {
                plan_media(&sstate, &resp.media_urls, &thread_url_raw, &media_thread, &mut sql)
                    .await
                    .unwrap();

                save_index(&download_dir, &resp.page_body).await.unwrap();

                if !is_archived {
                    save_json(&download_dir, &client, &thread_url_raw).await.unwrap();
                }

                not_found_attempts = 0;
            }
            RespType::NotFound => not_found_attempts += 1,
            RespType::BadGateway => not_found_attempts += 1,
            RespType::NotModified => not_found_attempts = 0,
        }

        if is_archived {
            break;
        }

        if not_found_attempts > 5 {
            break;
        }

        // state.plan_media(url)
        println!("thread {} tick", &thread_url_raw.to_url_string());

        // https://docs.rs/tokio/0.2.21/tokio/time/fn.timeout.html
        delay_for(Duration::new(50, 0)).await;
    }

    sql.finish_thread(&thread_url_raw).await.unwrap();

    println!("thread {} is free!", &thread_url_raw.to_url_string());
}
