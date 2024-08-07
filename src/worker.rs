// Licensed to the Apache Software Foundation (ASF) under one or more
// contributor license agreements.  See the NOTICE file distributed with
// this work for additional information regarding copyright ownership.
// The ASF licenses this file to You under the Apache License, Version 2.0
// (the "License"); you may not use this file except in compliance with
// the License.  You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::{
    channel::{self, TxReporter},
    module::{
        HEARTBEAT_PERIOD, PROPERTIES_REPORT_PERIOD_FACTOR, SERVICE_INSTANCE, SERVICE_NAME,
        SOCKET_FILE_PATH, WORKER_THREADS,
    },
    reporter::run_reporter,
    util::change_permission,
};

use once_cell::sync::Lazy;

use skywalking::{
    management::{instance::Properties, manager::Manager},
    reporter::{CollectItem, CollectItemConsume},
};
use std::{
    cmp::Ordering, error::Error, fs, io, marker::PhantomData, num::NonZeroUsize, process::exit,
    thread::available_parallelism, time::Duration,
};

use fslock::LockFile;
use tokio::{
    net::UnixListener,
    runtime::{self, Runtime},
    select,
    signal::unix::{signal, SignalKind},
    sync::mpsc::{self, error::TrySendError},
};
use tonic::async_trait;
use tracing::{debug, error, info, warn};
use crate::module::AGENT_PID_FILE_PATH;

pub fn init_worker() {
    let worker_threads = worker_threads();

    unsafe {
        // TODO Shutdown previous worker before fork if there is a PHP-FPM reload
        // operation.
        // TODO Change the worker process name.

        let pid = libc::fork();
        match pid.cmp(&0) {
            Ordering::Less => {
                error!("fork failed");
            }

            Ordering::Equal => {
                // Ensure worker process exits when master process exists.
                #[cfg(target_os = "linux")]
                // libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);

                let mut pid_lock =
                    LockFile::open(&*AGENT_PID_FILE_PATH).unwrap();
                if !pid_lock.try_lock_with_pid().unwrap() {
                    println!("process has running...");
                    return;
                }


                match fs::metadata(&*SOCKET_FILE_PATH) {
                    Ok(_) => {
                        if let Err(err) = fs::remove_file(&*SOCKET_FILE_PATH) {
                            error!(?err, "Remove socket file failed");
                        }
                    }
                    Err(_) => {

                    }
                }


                // Run the worker in subprocess.
                let rt = new_tokio_runtime(worker_threads);
                match rt.block_on(start_worker()) {
                    Ok(_) => {
                        exit(0);
                    }
                    Err(err) => {
                        error!(?err, "worker exit unexpectedly");
                        exit(1);
                    }
                }
            }
            Ordering::Greater => {}
        }
    }
}

fn worker_threads() -> usize {
    let worker_threads = *WORKER_THREADS;
    if worker_threads <= 0 {
        available_parallelism().map(NonZeroUsize::get).unwrap_or(1)
    } else {
        worker_threads as usize
    }
}

fn new_tokio_runtime(worker_threads: usize) -> Runtime {
    runtime::Builder::new_multi_thread()
        .thread_name("sw: worker")
        .enable_all()
        .worker_threads(worker_threads)
        .build()
        .unwrap()
}

async fn start_worker() -> anyhow::Result<()> {
    debug!("Starting worker...");

    // Ensure to cleanup resources when worker exits.
    let _guard = WorkerExitGuard::default();

    // Graceful shutdown signal, put it on the top of program.
    let mut sig_term = signal(SignalKind::terminate())?;
    let mut sig_int = signal(SignalKind::interrupt())?;

    let socket_file = &*SOCKET_FILE_PATH;

    let fut = async move {
        debug!(?socket_file, "Bind unix stream");
        let listener = UnixListener::bind(socket_file)?;
        change_permission(socket_file, 0o777);

        let (tx, rx) = mpsc::channel::<CollectItem>(255);
        let tx_ = tx.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((mut stream, _addr)) => {
                        let tx = tx.clone();

                        tokio::spawn(async move {
                            debug!("Entering channel_receive loop");

                            loop {
                                let r = match channel::channel_receive(&mut stream).await {
                                    Err(err) => match err.downcast_ref::<io::Error>() {
                                        Some(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                                            debug!("Leaving channel_receive loop");
                                            return;
                                        }
                                        _ => {
                                            error!(?err, "channel_receive failed");
                                            continue;
                                        }
                                    },
                                    Ok(i) => i,
                                };

                                // Try send here, to prevent the ipc blocking caused by the channel
                                // bursting (too late to report),
                                // which affects the pool process of php-fpm.
                                if let Err(err) = tx.try_send(r) {
                                    error!(?err, "Send collect item failed");
                                    if !matches!(err, TrySendError::Full(_)) {
                                        return;
                                    }
                                }
                            }
                        });
                    }
                    Err(err) => {
                        error!(?err, "Accept failed");
                    }
                }
            }
        });

        report_properties_and_keep_alive(TxReporter(tx_));

        // Run reporter with blocking.
        run_reporter((), Consumer(rx)).await?;

        Ok::<_, anyhow::Error>(())
    };

    // TODO Do graceful shutdown, and wait 10s then force quit.
    select! {
        _ = sig_term.recv() => {}
        _ = sig_int.recv() => {}
        r = fut => {
            r?;
        }
    }

    info!("Start to shutdown skywalking grpc reporter");

    Ok(())
}

struct Consumer(mpsc::Receiver<CollectItem>);

#[async_trait]
impl CollectItemConsume for Consumer {
    async fn consume(&mut self) -> Result<Option<CollectItem>, Box<dyn Error + Send>> {
        Ok(self.0.recv().await)
    }

    async fn try_consume(&mut self) -> Result<Option<CollectItem>, Box<dyn Error + Send>> {
        Ok(self.0.try_recv().ok())
    }
}

#[derive(Default)]
struct WorkerExitGuard(PhantomData<()>);

impl Drop for WorkerExitGuard {
    fn drop(&mut self) {
        match Lazy::get(&SOCKET_FILE_PATH) {
            Some(socket_file) => {
                info!(?socket_file, "Remove socket file");
                if let Err(err) = fs::remove_file(socket_file) {
                    error!(?err, "Remove socket file failed");
                }
            }
            None => {
                warn!("Socket file not created");
            }
        }
    }
}

fn report_properties_and_keep_alive(reporter: TxReporter) {
    let manager = Manager::new(&*SERVICE_NAME, &*SERVICE_INSTANCE, reporter);

    manager.report_and_keep_alive(
        || {
            let mut props = Properties::new();
            props.insert_os_info();
            props.update(Properties::KEY_LANGUAGE, "php");
            props.update(Properties::KEY_PROCESS_NO, unsafe {
                libc::getppid().to_string()
            });
            debug!(?props, "Report instance properties");
            props
        },
        Duration::from_secs(*HEARTBEAT_PERIOD as u64),
        *PROPERTIES_REPORT_PERIOD_FACTOR as usize,
    );
}
