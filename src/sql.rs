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

use crate::{
    db_async::{DbRequest, ThreadFilter, ThreadsFromStart},
    state::DvachThreadUrl,
    ShutdownCall,
};

pub struct DbState {
    pub conn: Connection,
}

fn from_rusqlite_error(e: rusqlite::Error) -> anyhow::Error {
    anyhow::anyhow!("{}", e)
}

impl DbState {
    pub fn sql_init(db_filename: PathBuf) -> Result<DbState> {
        println!("db location: {}", &db_filename.to_str().unwrap());
        let conn = Connection::open(db_filename)?;

        conn.execute(
            "create table if not exists thread (
                     id integer primary key,
                     board text not null,
                     thread_num text not null,
                     is_finished boolean default false,
                     UNIQUE(board,thread_num)
                 )",
            NO_PARAMS,
        )?;

        Ok(DbState { conn })
    }

    fn read_threads(&self, filter: ThreadFilter) -> Result<Vec<DvachThreadUrl>> {
        let raw_sql = match filter {
            ThreadFilter::All => {
                //
                "SELECT id, board, thread_num, is_finished FROM thread"
            }

            ThreadFilter::NotFinished => {
                "SELECT id, board, thread_num, is_finished FROM thread where is_finished=false"
            }
        };

        let mut stmt = self.conn.prepare(raw_sql)?;

        let person_iter = stmt.query_map(params![], |row| {
            Ok(DvachThreadUrl { board: row.get(1)?, thread_number: row.get(2)?, is_archive: false })
        })?;

        let u = person_iter.collect::<Result<Vec<_>, _>>();
        u.map_err(from_rusqlite_error)
    }

    fn remove_thread(&self, url: &DvachThreadUrl) {
        self.conn
            .execute(
                "UPDATE thread set is_finished=true where board=?1 and thread_num=?2",
                params![url.board, url.thread_number],
            )
            .unwrap();
    }

    fn add_thread(&self, url: &DvachThreadUrl) {
        self.conn
            .execute(
                "INSERT INTO thread (board, thread_num) VALUES(?1, ?2);",
                params![url.board, url.thread_number],
            )
            .unwrap();
    }
}

pub async fn sql_thread_runner(
    db_state: DbState,
    receiver: Receiver<DbRequest>,
    _shotdown: ShutdownCall,
) {
    loop {
        let mut receiver = receiver;
        loop {
            let command: DbRequest = receiver.recv().await.unwrap();

            match command {
                DbRequest::GetThreads(sender, filter) => {
                    let threads = db_state.read_threads(filter).unwrap();

                    let data = ThreadsFromStart { threads };
                    sender.send(data).unwrap();
                }
                DbRequest::ThreadFinished(url) => {
                    //
                    db_state.remove_thread(&url)
                }
                DbRequest::NewThread(url) => {
                    //
                    db_state.add_thread(&url);
                }
                DbRequest::NewMedia(_) => {
                    // println!("new media");
                }
                DbRequest::MediaFinished(_) => panic!("implement"),
            };
        }
    }
}
