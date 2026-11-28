use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sqlx::postgres::{PgListener, PgNotification, PgPoolOptions};
use tokio::sync::{watch, Mutex};

use crate::{registry, DbPool, DriverError, Result};

/// A subscription on a dedicated connection tied to a named pool.
pub struct NotificationListener {
    listener: Arc<Mutex<Option<PgListener>>>,
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
    failure: OnceLock<String>,
}

impl NotificationListener {
    /// Subscribe to the channels using the named pool's connection settings.
    pub async fn connect(name: &str, channels: &[String]) -> Result<Self> {
        validate_channels(channels)?;
        let DbPool::Postgres(application_pool) = registry().get(name).await?.clone_pool();

        // A private pool lets SQLx reconnect without taking an application pool slot.
        let acquire_timeout = application_pool.options().get_acquire_timeout();
        let listener_pool = PgPoolOptions::new()
            .max_connections(1)
            .min_connections(0)
            .acquire_timeout(acquire_timeout)
            .max_lifetime(None)
            .idle_timeout(None)
            .connect_lazy_with((*application_pool.connect_options()).clone());
        let listener = application_pool
            .close_event()
            .do_until(tokio::time::timeout(acquire_timeout, async {
                let mut listener = PgListener::connect_with(&listener_pool)
                    .await
                    .map_err(|e| DriverError::db("acquiring notification listener", e))?;
                listener.discard_connection_on_drop(true);
                listener
                    .listen_all(channels.iter().map(String::as_str))
                    .await
                    .map_err(|e| DriverError::db("subscribing notification listener", e))?;
                Ok::<_, DriverError>(listener)
            }))
            .await
            .map_err(|e| DriverError::db("subscribing notification listener", e))?
            .map_err(|_| {
                DriverError::db(
                    "subscribing notification listener",
                    sqlx::Error::PoolTimedOut,
                )
            })??;

        let listener = Arc::new(Mutex::new(Some(listener)));
        let (stop, mut stopped) = watch::channel(false);
        let (finished, done) = watch::channel(false);
        let owned = Arc::clone(&listener);
        let shutdown = stop.clone();
        tokio::spawn(async move {
            tokio::select! {
                biased;
                () = application_pool.close_event() => {},
                _ = stopped.wait_for(|value| *value) => {},
            }
            shutdown.send_replace(true);
            // Dropping PgListener closes the dedicated socket immediately.
            drop(owned.lock().await.take());
            // Keep a backstop for pool shutdown.
            let _ = tokio::time::timeout(Duration::from_secs(1), listener_pool.close()).await;
            finished.send_replace(true);
        });
        Ok(Self {
            listener,
            stop,
            done,
            failure: OnceLock::new(),
        })
    }

    /// Wait for a notification, returning `None` on explicit or pool closure.
    pub async fn recv(&self) -> Result<Option<PgNotification>> {
        let mut stopped = self.stop.subscribe();
        if *stopped.borrow() {
            return self.closed_result();
        }
        let mut guard = tokio::select! {
            biased;
            _ = stopped.wait_for(|value| *value) => return self.closed_result(),
            guard = self.listener.lock() => guard,
        };
        let Some(listener) = guard.as_mut() else {
            return self.closed_result();
        };
        tokio::select! {
            biased;
            _ = stopped.wait_for(|value| *value) => self.closed_result(),
            result = listener.recv() => match result {
                Ok(notification) => Ok(Some(notification)),
                Err(error) => {
                    let transient = is_transient_receive_error(&error);
                    let error = DriverError::db("receiving notification", error);
                    if !transient {
                        let _ = self.failure.set(error.to_string());
                        self.request_close();
                    }
                    Err(error)
                }
            },
        }
    }

    fn closed_result(&self) -> Result<Option<PgNotification>> {
        match self.failure.get() {
            Some(message) => Err(DriverError::ListenerFailed(message.clone())),
            None => Ok(None),
        }
    }

    /// Request shutdown even if the caller cannot await it.
    pub fn request_close(&self) {
        self.stop.send_replace(true);
    }

    /// Request shutdown and wait for the shutdown task.
    pub async fn close(&self) {
        self.request_close();
        let mut done = self.done.clone();
        let _ = done.wait_for(|value| *value).await;
    }
}

impl Drop for NotificationListener {
    fn drop(&mut self) {
        self.request_close();
    }
}

fn is_transient_receive_error(error: &sqlx::Error) -> bool {
    match error {
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolTimedOut => true,
        sqlx::Error::Database(error) => error.code().as_deref().is_some_and(is_transient_sqlstate),
        _ => false,
    }
}

fn is_transient_sqlstate(code: &str) -> bool {
    code.starts_with("08") || matches!(code, "57P01" | "57P02" | "57P03" | "53300" | "25006")
}

fn validate_channels(channels: &[String]) -> Result<()> {
    if channels.is_empty() {
        return Err(DriverError::InvalidListenerChannels(
            "at least one channel is required".into(),
        ));
    }
    let mut seen = HashSet::new();
    for channel in channels {
        if channel.is_empty() || channel.contains('\0') || channel.len() > 63 {
            return Err(DriverError::InvalidListenerChannels(
                "channel names must contain 1–63 UTF-8 bytes and no NUL".into(),
            ));
        }
        if !seen.insert(channel) {
            return Err(DriverError::InvalidListenerChannels(
                "duplicate channel names are not allowed".into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{is_transient_receive_error, is_transient_sqlstate, validate_channels};

    #[test]
    fn transient_receive_errors() {
        for error in [
            sqlx::Error::Io(std::io::ErrorKind::ConnectionReset.into()),
            sqlx::Error::Tls("TLS error".into()),
            sqlx::Error::PoolTimedOut,
        ] {
            assert!(is_transient_receive_error(&error));
        }
        for error in [
            sqlx::Error::PoolClosed,
            sqlx::Error::Protocol("protocol error".into()),
        ] {
            assert!(!is_transient_receive_error(&error));
        }
        for code in [
            "08000", "08006", "57P01", "57P02", "57P03", "53300", "25006",
        ] {
            assert!(is_transient_sqlstate(code));
        }
        for code in ["28000", "42501", "57P04", "57014"] {
            assert!(!is_transient_sqlstate(code));
        }
    }

    #[test]
    fn channel_validation() {
        for channels in [
            vec![],
            vec![String::new()],
            vec!["a\0b".into()],
            vec!["a".repeat(64)],
            vec!["é".repeat(32)],
            vec!["a".into(), "a".into()],
        ] {
            assert!(validate_channels(&channels).is_err());
        }
        assert!(validate_channels(&["a".repeat(63), "é".repeat(31), "a\"b".into()]).is_ok());
    }
}
