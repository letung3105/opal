//! Asynchronous server for the storage engine that communicates with RESP protocol.

use super::{Connection, Error};
use crate::{
    net::{cmd::Command, Shutdown},
    storage::StorageEngine,
};
use std::{convert::TryFrom, future::Future, sync::Arc, time::Duration};
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc, Semaphore},
    time,
};
use tracing::{debug, error, info};

/// Max number of concurrent connections that can be served by the server.
const MAX_CONNECTIONS: usize = 128;

/// Max number of seconds to wait for when retrying to accept a new connection.
/// The value is in second.
const MAX_BACKOFF: u64 = 64;

/// Provide methods and hold states for a Redis server. The server will exist when `shutdown`
/// finishes, or when there's an error.
pub struct Server<S: Future> {
    ctx: Context,
    shutdown: S,
}

impl<S: Future> Server<S> {
    /// Runs the server.
    pub fn new(listener: TcpListener, shutdown: S) -> Self {
        // Ignoring the broadcast received because one can be created by
        // calling `subscribe()` on the `Sender`
        let (notify_shutdown, _) = broadcast::channel(1);
        let (shutdown_complete_tx, shutdown_complete_rx) = mpsc::channel(1);

        let ctx = Context {
            storage: Default::default(),
            listener,
            limit_connections: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            notify_shutdown,
            shutdown_complete_rx,
            shutdown_complete_tx,
        };

        Self { ctx, shutdown }
    }

    /// Runs the server that exits when `shutdown` finishes, or when there's
    /// an error.
    pub async fn run(mut self) {
        // Concurrently run the tasks and blocks the current task until
        // one of the running tasks finishes. The block that is associated
        // with the task gets to run, when the task is the first to finish.
        // Under normal circumstances, this blocks until `shutdown` finishes.
        tokio::select! {
            result = self.ctx.listen() => {
                if let Err(err) = result {
                    // The server has been failing to accept inbound connections
                    // for multiple times, so it's giving up and shutting down.
                    // Error occured while handling individual connection don't
                    // propagate further.
                    error!(cause = %err, "failed to accept");
                }
            }
            _ = self.shutdown => {
                info!("shutting down");
            }
        }

        // Dropping this so tasks that have called `subscribe()` will be notified for
        // shutdown and can gracefully exit.
        drop(self.ctx.notify_shutdown);

        // Dropping this so there's no dangling `Sender`. Otherwise, awaiting on the
        // channel's received will block forever because we still holding the last
        // sender instance.
        drop(self.ctx.shutdown_complete_tx);

        // Awaiting for all active connections to finish processing.
        self.ctx.shutdown_complete_rx.recv().await;
    }
}

/// The server's runtime state that is shared across all connections.
/// This is also in charge of listening for new inbound connections.
struct Context {
    // Database handle
    storage: StorageEngine,

    // The TCP socket for listening for inbound connection
    listener: TcpListener,

    // Semaphore with `MAX_CONNECTIONS`.
    //
    // When a handler is dropped, the semaphore is decremented to grant a
    // permit. When waiting for connections to close, the listener will be
    // notified once a permit is granted.
    limit_connections: Arc<Semaphore>,

    // Broacast channeling to signal a shutdown to all active connections.
    //
    // The server is responsible for gracefully shutting down active connections.
    // When a connection is spawned, it is given a broadcast receiver handle.
    // When the server wants to gracefully shutdown its connections, a `()` value
    // is sent. Each active connection receives the value, reaches a safe terminal
    // state, and completes the task.
    notify_shutdown: broadcast::Sender<()>,

    // This channel ensures that the server will wait for all connections to
    // complete processing before shutting down.
    //
    // Tokio's channnels are closed when all the `Sender` handles are dropped.
    // When a connection handler is created, it is given clone of the of
    // `shutdown_complete_tx`, which is dropped when the listener shutdowns.
    // When all the listeners shut down, the channel is closed and
    // `shutdown_complete_rx.receive()` will return `None`. At this point, it
    // is safe for the server to quit.
    shutdown_complete_rx: mpsc::Receiver<()>,
    shutdown_complete_tx: mpsc::Sender<()>,
}

/// Reads client requests and applies those to the storage.
struct Handler {
    // Database handle.
    storage: StorageEngine,

    // Writes and reads frame.
    connection: Connection,

    // The semaphore that granted the permit for this handler.
    // The handler is in charge of releasing its permit.
    limit_connections: Arc<Semaphore>,

    // Receives shut down signal.
    shutdown: Shutdown,

    // Signals that the handler finishes executing.
    _shutdown_complete: mpsc::Sender<()>,
}

impl Context {
    async fn listen(&mut self) -> Result<(), Error> {
        info!("listening for new connections");

        loop {
            // Wait for a permit to become available.
            //
            // For convenient, the handle is bounded to the semaphore's lifetime
            // and when it gets dropped, it decrements the count. Because we're
            // releasing the permit in a different task from the one we acquired it
            // in, `forget()` is use to drop the semaphore handle without releasing
            // the permit at the end of this scope.
            self.limit_connections.acquire().await.unwrap().forget();

            // Accepts a new connection and retries on error. If this function
            // returns an error, it means that the server could not accept any
            // new connection and it is aborting.
            let socket = self.accept().await?;

            // Creating the handler's state for managing the new connection
            let mut handler = Handler {
                storage: self.storage.clone(),
                connection: Connection::new(socket),
                limit_connections: Arc::clone(&self.limit_connections),
                shutdown: Shutdown::new(self.notify_shutdown.subscribe()),
                _shutdown_complete: self.shutdown_complete_tx.clone(),
            };

            // Spawn separate task for handling the connection
            tokio::spawn(async move {
                if let Err(err) = handler.run().await {
                    error!(cause=?err, "connection error");
                }
            });
        }
    }

    /// Accepts a new connection.
    ///
    /// Returns the a [`TcpStream`] on success. Retries with an exponential
    /// backoff strategy when there's an error. If the backoff time passes
    /// to maximum allowed time, returns an error.
    ///
    /// [`TcpStream`]: tokio::net::TcpStream
    async fn accept(&mut self) -> Result<TcpStream, Error> {
        let mut backoff = 1;
        loop {
            match self.listener.accept().await {
                Ok((socket, _)) => return Ok(socket),
                Err(err) => {
                    if backoff > MAX_BACKOFF {
                        return Err(err.into());
                    }
                }
            }

            // Wait for `backoff` seconds
            time::sleep(Duration::from_secs(backoff)).await;

            // Doubling the backoff time
            backoff <<= 1;
        }
    }
}

impl Handler {
    /// Process a single connection.
    ///
    /// Currently, pipelining is not implemented. See for more details at:
    /// https://redis.io/topics/pipelining
    ///
    /// When the shutdown signal is received, the connection is processed until
    /// it reaches a safe state, at which point it is terminated.
    #[tracing::instrument(skip(self))]
    async fn run(&mut self) -> Result<(), Error> {
        // Keeps ingesting frames when not the server is still running
        while !self.shutdown.is_shutdown() {
            // Awaiting for a shutdown event or a new frame
            let maybe_frame = tokio::select! {
                res = self.connection.read_frame() => res?,
                _ = self.shutdown.recv() => {
                    return Ok(());
                }
            };

            // No frame left means the client closed the connection, so we can
            // return with no error
            let frame = match maybe_frame {
                Some(frame) => frame,
                None => return Ok(()),
            };

            // Try to parse a command out of the frame
            let cmd = Command::try_from(frame)?;
            debug!(?cmd);

            cmd.apply(&self.storage, &mut self.connection, &mut self.shutdown)
                .await?;
        }
        Ok(())
    }
}

impl Drop for Handler {
    fn drop(&mut self) {
        // Releases the permit that was granted for this handler. Performing this
        // in the `Drop` implementation ensures that the permit is always
        // automatically returned when the handler finishes
        self.limit_connections.add_permits(1);
    }
}