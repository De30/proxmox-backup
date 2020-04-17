use anyhow::{Error};
use lazy_static::lazy_static;
use std::sync::Mutex;

use futures::*;

use tokio::signal::unix::{signal, SignalKind};

use crate::tools::{self, BroadcastData};

#[derive(PartialEq, Copy, Clone, Debug)]
pub enum ServerMode {
    Normal,
    Shutdown,
}

pub struct ServerState {
    pub mode: ServerMode,
    pub shutdown_listeners: BroadcastData<()>,
    pub last_worker_listeners: BroadcastData<()>,
    pub worker_count: usize,
    pub reload_request: bool,
}

lazy_static! {
    static ref SERVER_STATE: Mutex<ServerState> = Mutex::new(ServerState {
        mode: ServerMode::Normal,
        shutdown_listeners: BroadcastData::new(),
        last_worker_listeners: BroadcastData::new(),
        worker_count: 0,
        reload_request: false,
    });
}

pub fn server_state_init() -> Result<(), Error> {

    let mut stream = signal(SignalKind::interrupt())?;

    let future = async move {
        while stream.recv().await.is_some() {
            println!("got shutdown request (SIGINT)");
            SERVER_STATE.lock().unwrap().reload_request = false;
            tools::request_shutdown();
        }
    }.boxed();

    let abort_future = last_worker_future().map_err(|_| {});
    let task = futures::future::select(future, abort_future);

    tokio::spawn(task.map(|_| ()));

    let mut stream = signal(SignalKind::hangup())?;

    let future = async move {
        while stream.recv().await.is_some() {
            println!("got reload request (SIGHUP)");
            SERVER_STATE.lock().unwrap().reload_request = true;
            tools::request_shutdown();
        }
    }.boxed();

    let abort_future = last_worker_future().map_err(|_| {});
    let task = futures::future::select(future, abort_future);

    tokio::spawn(task.map(|_| ()));

    Ok(())
}

pub fn is_reload_request() -> bool {
    let data = SERVER_STATE.lock().unwrap();

    data.mode == ServerMode::Shutdown && data.reload_request
}

pub fn server_shutdown() {
    let mut data = SERVER_STATE.lock().unwrap();

    println!("SET SHUTDOWN MODE");

    data.mode = ServerMode::Shutdown;

    data.shutdown_listeners.notify_listeners(Ok(()));

    drop(data); // unlock

    check_last_worker();
}

pub fn shutdown_future() -> impl Future<Output = ()> {
    let mut data = SERVER_STATE.lock().unwrap();
    data
        .shutdown_listeners
        .listen()
        .map(|_| ())
}

pub fn last_worker_future() ->  impl Future<Output = Result<(), Error>> {
    let mut data = SERVER_STATE.lock().unwrap();
    data.last_worker_listeners.listen()
}

pub fn set_worker_count(count: usize) {
    let mut data = SERVER_STATE.lock().unwrap();
    data.worker_count = count;

    if !(data.mode == ServerMode::Shutdown && data.worker_count == 0) { return; }

    data.last_worker_listeners.notify_listeners(Ok(()));
}


pub fn check_last_worker() {

    let mut data = SERVER_STATE.lock().unwrap();

    if !(data.mode == ServerMode::Shutdown && data.worker_count == 0) { return; }

    data.last_worker_listeners.notify_listeners(Ok(()));
}
