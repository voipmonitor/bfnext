use crate::db::Db;
use bytes::{Bytes, BytesMut};
use log::error;
use simplelog::{LevelFilter, WriteLogger};
use std::{cell::RefCell, env, io, path::PathBuf, thread};
use tokio::{
    fs::File,
    io::AsyncWriteExt,
    runtime::Builder,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

struct LogHandle(UnboundedSender<Task>);

thread_local! {
    static LOGBUF: RefCell<BytesMut> = RefCell::new(BytesMut::new());
}

impl io::Write for LogHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        LOGBUF.with(|lbuf| {
            let mut lbuf = lbuf.borrow_mut();
            lbuf.extend_from_slice(buf);
            self.0
                .send(Task::WriteLog(lbuf.split().freeze()))
                .map_err(|_| io::Error::new(io::ErrorKind::Other, "backend dead"))?;
            Ok(buf.len())
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
pub enum Task {
    MizInit,
    SaveState(PathBuf, Db),
    WriteLog(Bytes),
}

async fn background_loop(write_dir: PathBuf, mut rx: UnboundedReceiver<Task>) {
    let log_path = write_dir.join("Logs").join("bfnext.txt");
    let mut log_file = File::options()
        .create(true)
        .write(true)
        .append(true)
        .open(log_path)
        .await
        .unwrap();
    while let Some(msg) = rx.recv().await {
        match msg {
            Task::MizInit => (),
            Task::SaveState(path, db) => match db.save(&path) {
                Ok(()) => (),
                Err(e) => error!("failed to save state to {:?}, {:?}", path, e),
            },
            Task::WriteLog(mut buf) => log_file.write_all_buf(&mut buf).await.unwrap(),
        }
    }
}

fn setup_logger(tx: UnboundedSender<Task>) {
    let level = match env::var("RUST_LOG").ok().map(|s| s.to_ascii_lowercase()) {
        None => LevelFilter::Warn,
        Some(s) if &s == "trace" => LevelFilter::Trace,
        Some(s) if &s == "debug" => LevelFilter::Debug,
        Some(s) if &s == "info" => LevelFilter::Info,
        Some(s) if &s == "error" => LevelFilter::Error,
        Some(s) if &s == "warn" => LevelFilter::Warn,
        Some(s) if &s == "off" => LevelFilter::Off,
        Some(_) => LevelFilter::Warn,
    };
    WriteLogger::init(dbg!(level), simplelog::Config::default(), LogHandle(tx)).unwrap()
}

pub fn init(write_dir: PathBuf) -> UnboundedSender<Task> {
    let (tx, rx) = mpsc::unbounded_channel();
    setup_logger(tx.clone());
    thread::spawn(move || {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(background_loop(write_dir, rx))
    });
    tx
}
