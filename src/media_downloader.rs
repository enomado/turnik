extern crate lazy_static;

use std::{
    cell::{Cell, Ref, RefCell, UnsafeCell},
    collections::{HashMap, HashSet},
};
// use std::borrow::Borrow;
use std::{
    io::Read,
    mem::replace,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, LockResult, Mutex, MutexGuard},
    thread, time,
    time::Duration,
};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use futures::stream::TryStreamExt;
use futures_util::StreamExt;
use regex::Regex;
use reqwest::Response;
use select::{document::Document, predicate::Name};
use tokio::{
    fs::{metadata, rename, File},
    io::AsyncWriteExt,
    sync::{
        broadcast,
        mpsc::{channel, channel as tokio_channel, Receiver, Sender},
        watch,
    },
    task,
    task::yield_now,
    time::delay_for,
};
use url::{Host, ParseError, Position, Url};

use crate::{
    state::{DlEntryStatus, MediaEntry, ThreadState, UrlFile},
    ShutdownCall,
};

fn update_media(state: &RefCell<ThreadState>, media: &MediaEntry) {
    let new_media = MediaEntry { state: DlEntryStatus::Finished, ..media.clone() };

    // state.db.media_finished_add(&new_media.url);

    let mut state = state.borrow_mut();
    state.medias.replace(Arc::new(new_media));
}

async fn copy_file(response: &mut Response, dest: &mut File) {
    // https://github.com/seanmonstar/reqwest/issues/482
    // let u = response.bytes_stream()
    // may be use use tokio_util::compat::FuturesAsyncReadCompatExt; ?

    while let Some(chunk) = response.chunk().await.unwrap() {
        dest.write(&chunk).await.unwrap();
    }
}

async fn file_exists(path: &PathBuf) -> bool {
    metadata(path).await.is_ok()
}

#[tokio::test]
async fn metadata_test() {
    let _data = metadata("~/fsdfsd").await.is_ok();
}

async fn process_media(sstate: &RefCell<ThreadState>, media: &MediaEntry) -> Result<()> {
    // TODO: continue download range http
    // https://rust-lang-nursery.github.io/rust-cookbook/web/clients/download.html#make-a-partial-download-with-http-range-headers

    let root_folder = {
        let state = sstate.borrow();
        state.download_directory.clone()
    };

    let filename = media.url_filename.clone();

    let folder = media.thread.local_fs_folder(&root_folder);

    // TODO:
    // filename.tmp
    // move than

    // skip if file exists

    if filename.contains('/') {
        panic!("wrong filename {filename}")
    }

    let abs_filename = folder.join(&filename);

    if file_exists(&abs_filename).await {
        println!("skipping existing {}", media.url);
        update_media(sstate, media);
        return Ok(());
    }

    let abs_filename_tmp = folder.join(filename.clone() + ".tmp");

    dbg!(&abs_filename);

    let mut response = reqwest::get(&media.url).await?;

    // let mut dest =
    // File::create(&abs_filename.to_str().unwrap().to_string()).unwrap();
    let mut dest: File = File::create(&abs_filename_tmp).await?;

    copy_file(&mut response, &mut dest).await;
    dest.flush().await?;

    rename(&abs_filename_tmp, &abs_filename).await?;
    update_media(sstate, media);

    // panic!("implement plz")
    Ok(())
}

#[tokio::test]
async fn select_test() {
    use futures::{future, select};
    let mut a = future::ready(4);
    let mut b = future::pending::<()>();

    let res = select! {
        a_res = a => a_res + 1,
        _ = b => 0,
    };
    assert_eq!(res, 5);
}

// use crate::media_downloader::MediaThread;

pub struct MediaThread {
    state: Rc<RefCell<ThreadState>>,
    sender: RefCell<Sender<Arc<MediaEntry>>>,

    receiver: Option<RefCell<Receiver<Arc<MediaEntry>>>>,
}

type MediaThreadPanic = ShutdownCall;

impl MediaThread {
    // todo use Notify

    pub fn init(sstate: &Rc<RefCell<ThreadState>>) -> MediaThread {
        let (sender, receiver) = tokio_channel::<Arc<MediaEntry>>(1);

        MediaThread {
            state: sstate.clone(),
            receiver: Some(RefCell::new(receiver)),
            sender: RefCell::new(sender),
        }
    }

    pub fn spawn(
        &mut self,
        local: &task::LocalSet,
        shotdown_seceiver: MediaThreadPanic,
    ) -> &MediaThread {
        let state = self.state.clone();

        let receiver = self.receiver.take().unwrap();

        local
            .spawn_local(async { MediaThreadInt::start(state, receiver, shotdown_seceiver).await });
        self
    }

    pub async fn notify_new_media(&self) -> Result<()> {
        use crate::state::DvachThreadUrl;
        use core::result::Result as CoreResult;
        use tokio::sync::mpsc::error::TrySendError;

        let mut sender = self.sender.borrow_mut();

        let res = sender.try_send(Arc::new(MediaEntry {
            url: "()".to_owned(),
            state: DlEntryStatus::JustAdded,
            thread: DvachThreadUrl::default(),
            url_filename: "()".to_owned(),
        }));

        // if let CoreResult::Err(r) = res{
        // }

        let res =
            res.err().and_then(|x| if let TrySendError::Closed(_) = x { Some(x) } else { None });

        if let Some(_o) = res {
            // return Err("o.into()");
            bail!("channel closed");
        }

        Ok(())
    }
}

struct MediaThreadInt {}

impl MediaThreadInt {
    fn get_next_media(state: &RefCell<ThreadState>) -> Option<Arc<MediaEntry>> {
        let media = {
            let state = state.borrow();
            let med = state.medias.iter().find(|x| x.is_ready_for_download());
            med.map(|df| df.clone())
        };
        media
    }

    async fn wait_notify_or_shoutwown(
        receiver: &mut Receiver<Arc<MediaEntry>>,
        shotdown: &mut MediaThreadPanic,
    ) -> bool {
        // delay_for(Duration::new(10, 0)).await;
        // let mut shotdown = shotdown.borrow_mut();

        let res = tokio::select! {
            _y = receiver.recv() => {
                println!("operation timed out");
                false
            }
            _x = shotdown.recv() => {
                println!("operation completed");
                true
            }
        };
        res
    }

    pub async fn start(
        state: Rc<RefCell<ThreadState>>,
        receiver: RefCell<Receiver<Arc<MediaEntry>>>,
        shotdown: MediaThreadPanic,
    ) {
        // https://github.com/seanmonstar/reqwest/issues/734

        // let receiver = receiver;
        // let mut state = state.borrow_mut();
        // let client = reqwest::Client::new();

        let mut receiver = receiver.borrow_mut();

        let mut shotdown = shotdown;

        loop {
            if shotdown.check_shotdown() {
                break;
            }

            let media = Self::get_next_media(&state);

            if let Some(med) = media {
                process_media(&state, &med).await.unwrap();
            } else {
                let res = Self::wait_notify_or_shoutwown(&mut receiver, &mut shotdown).await;
                if res {
                    break;
                }
            }

            println!("media tick");
        }
    }
}
