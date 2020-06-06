#![allow(unused_variables)]
#![allow(dead_code)]

use std::{
    assert,
    cell::RefCell,
    collections::{BTreeSet, HashSet},
    fmt,
    fmt::Display,
    hash::{Hash, Hasher},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, LockResult, Mutex, MutexGuard},
};

use anyhow::{bail, Context, Result};
use bincode;
use chrono::{offset::LocalResult, prelude::*, DateTime, Duration, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
// todo test appdb
use tokio::sync::mpsc::{channel as tokio_channel, Receiver, Sender};
use url::{Host, ParseError, Url};

use crate::thread_downloader::thread_downloader;

#[tokio::test]
async fn valid_arch_url() {
    let url = DvachThreadUrl::parse(
        &Url::parse(&"https://2ch.hk/b/arch/2020-06-01/res/221558211.html#221558211".to_owned())
            .unwrap(),
    );

    dbg!(url);
}

#[tokio::test]
async fn valid_origin() {
    let url =
        Url::parse(&"https://2ch.hk/b/arch/2020-06-01/res/221558211.html#221558211".to_owned())
            .unwrap();

    let origin = url.scheme();

    // let     scheme = match origin{
    // }

    dbg!();
}

#[tokio::test]
async fn test_something() {}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DvachThreadUrl {
    pub board: String,
    pub thread_number: String,
    pub is_archive: bool,
}

impl PartialEq for DvachThreadUrl {
    fn eq(&self, other: &Self) -> bool {
        self.board == other.board && self.thread_number == other.thread_number
    }
}

// Implement `Display` for `MinMax`.
impl fmt::Display for DvachThreadUrl {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Use `self.number` to refer to each positional data point.
        write!(f, "({}, {})", self.board, self.thread_number)
    }
}

impl DvachThreadUrl {
    pub fn parse(url: &Url) -> Result<DvachThreadUrl> {
        let mut path_segments = url.path_segments().context("no")?;

        let board = path_segments.next().context("no")?;

        let res_or_arch = path_segments.next().context("no")?;

        let (thread_num_raw, thread_date) = match res_or_arch {
            "res" => {
                let thread_num = path_segments.next().context("no")?;
                let thread_date = None;
                (thread_num, thread_date)
            }
            "arch" => {
                let thread_date = Some(path_segments.next().context("no")?);
                let mayby_res = path_segments.next().context("no")?;
                if mayby_res != "res" {
                    bail!("no res {}: {}", &url, mayby_res);
                }
                let thread_num = path_segments.next().context("no")?;
                (thread_num, thread_date)
            }
            something_new => panic!("something new in url {}", url),
        };

        let thread_number = thread_num_raw.split(".").next().context("no")?;

        Ok(DvachThreadUrl {
            board: board.to_owned(),
            thread_number: thread_number.to_owned(),
            is_archive: thread_date.is_some(),
        })
    }

    pub fn local_fs_folder(&self, root_path: &PathBuf) -> PathBuf {
        let root_path = root_path.clone();
        let folder = root_path.join(&self.board).join(&self.thread_number);
        folder
    }

    pub fn to_url_string(&self) -> String {
        format!("https://2ch.hk/{}/res/{}.html", self.board, self.thread_number)
    }

    pub fn json_form(&self) -> Url {
        let url = format!("https://2ch.hk/{}/res/{}.json", self.board, self.thread_number);

        let u = Url::parse(&url).unwrap();
        u
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq)]
pub enum DlEntryStatus {
    JustAdded,
    Downloading,
    Finished,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MediaEntry {
    pub url: String,
    pub state: DlEntryStatus,
    pub thread: DvachThreadUrl,
    pub url_filename: String,
}

impl MediaEntry {
    pub fn is_ready_for_download(&self) -> bool {
        self.state == DlEntryStatus::JustAdded
    }
}

impl PartialEq for MediaEntry {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

impl Hash for MediaEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.url.hash(state);
    }
}

impl Eq for MediaEntry {}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ThreadEntry {
    pub url: String,
    pub state: DlEntryStatus,

    pub last_crawled: DateTime<Utc>,
}

impl ThreadEntry {
    pub fn new<S: Into<String>>(url: S) -> ThreadEntry {
        let past_time = Utc.ymd(2014, 7, 8).and_hms(9, 10, 11);

        ThreadEntry { url: url.into(), state: DlEntryStatus::JustAdded, last_crawled: past_time }
    }
}

impl PartialEq for ThreadEntry {
    fn eq(&self, other: &Self) -> bool {
        self.url == other.url
    }
}

impl Hash for ThreadEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.url.hash(state);
    }
}

impl Eq for ThreadEntry {}

pub struct ThreadState {
    // single-thread for speed
    pub threads: HashSet<Arc<ThreadEntry>>,
    pub medias: HashSet<Arc<MediaEntry>>,
    pub is_running: bool,
    pub download_directory: PathBuf,
    /* pub counter: u64,
     * pub db: Arc<AppDb>,
     * thread_subscriber: Subscriber,
     * pub new_media_channel: Sender<Arc<MediaEntry>>, */
}

impl ThreadState {
    pub fn new(download_directory: &PathBuf) -> ThreadState {
        ThreadState {
            threads: Default::default(),
            medias: Default::default(),
            is_running: false,
            download_directory: download_directory.clone(),
        }
    }

    pub fn plan_thread<S>(&mut self, url: S) -> &mut Self
    where
        S: Into<String>,
    {
        self
    }

    pub fn plan_media(&mut self, media: &Arc<MediaEntry>) -> bool {
        self.medias.insert(media.clone())
    }
}

fn filter_url(url: &Url) -> Option<String> {
    let host = url.host_str();

    if !["https"].contains(&url.scheme()) {
        println!("not https {}", url.to_owned());
        return None;
    }

    if host.is_none() {
        println!("wooot {}", url.to_owned());
        return None;
    }

    let host = host.unwrap();

    if !["2ch.hk", "2ch.pm"].contains(&host) {
        // println!("filtered url by host: {}", url);
        return None;
    }

    lazy_static! {
        static ref FILE_ENDS: Regex = Regex::new(r".+?\..{1,10}$").unwrap();
    }

    // TODO: if re.match('/.+?/thumb/', url):

    let last_path_segment = url.path_segments().map(|c| c.last().unwrap());

    // dbg!(&maybe_something_with_dot);
    // return false;

    let is_file: bool = last_path_segment
        .map(|x| {
            FILE_ENDS.is_match(x)
                && !x.ends_with(".html")
                && !x.ends_with(".fcgi")
                && !x.ends_with(".cgi")
                && !x.ends_with(".php")
        })
        .unwrap_or(false);

    // dbg!(&y);

    return if is_file { Some(last_path_segment.unwrap().to_string()) } else { None };
}

#[derive(Debug, Clone)]
pub struct UrlFile {
    pub url: Url,
    pub url_filename: String,
}

impl UrlFile {
    pub fn from_url_or_none(url: Url) -> Option<UrlFile> {
        let path = filter_url(&url);

        path.map(|path_from_url| UrlFile { url, url_filename: path_from_url })
    }
}
