use crate::{ConnectionError, FrontendEvent, FrontendRequest, IpcError};
use std::{
    cmp::min,
    io::{self, BufReader, LineWriter, Lines, prelude::*},
    thread,
    time::{Duration, Instant},
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(windows)]
use std::net::TcpStream;

pub struct FrontendEventReader {
    #[cfg(unix)]
    lines: Lines<BufReader<UnixStream>>,
    #[cfg(windows)]
    lines: Lines<BufReader<TcpStream>>,
}

pub struct FrontendRequestWriter {
    #[cfg(unix)]
    line_writer: LineWriter<UnixStream>,
    #[cfg(windows)]
    line_writer: LineWriter<TcpStream>,
}

impl FrontendEventReader {
    pub fn next_event(&mut self) -> Option<Result<FrontendEvent, IpcError>> {
        match self.lines.next()? {
            Err(e) => Some(Err(e.into())),
            Ok(l) => Some(serde_json::from_str(l.as_str()).map_err(|e| e.into())),
        }
    }
}

impl FrontendRequestWriter {
    pub fn request(&mut self, request: FrontendRequest) -> Result<(), io::Error> {
        let mut json = serde_json::to_string(&request).unwrap();
        log::debug!("requesting: {json}");
        json.push('\n');
        self.line_writer.write_all(json.as_bytes())?;
        Ok(())
    }
}

/// Try to connect to the service once without retrying.
pub fn try_connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    #[cfg(unix)]
    let rx = {
        let socket_path = crate::default_socket_path()?;
        UnixStream::connect(&socket_path)?
    };
    #[cfg(windows)]
    let rx = TcpStream::connect("127.0.0.1:5252")?;
    make_connection(rx)
}

pub fn connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let rx = wait_for_service(None)?;
    make_connection(rx)
}

/// Wait at most `timeout` for the daemon to become available.
pub fn connect_timeout(
    timeout: Duration,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let rx = wait_for_service(Some(timeout))?;
    make_connection(rx)
}

#[cfg(unix)]
fn make_connection(
    rx: UnixStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

#[cfg(windows)]
fn make_connection(
    rx: TcpStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

/// wait for the lan-mouse socket to come online
#[cfg(unix)]
fn wait_for_service(timeout: Option<Duration>) -> Result<UnixStream, ConnectionError> {
    let socket_path = crate::default_socket_path()?;
    retry_connect(|| UnixStream::connect(&socket_path), timeout)
}

#[cfg(windows)]
fn wait_for_service(timeout: Option<Duration>) -> Result<TcpStream, ConnectionError> {
    retry_connect(|| TcpStream::connect("127.0.0.1:5252"), timeout)
}

fn retry_connect<T>(
    mut attempt: impl FnMut() -> io::Result<T>,
    timeout: Option<Duration>,
) -> Result<T, ConnectionError> {
    let started = Instant::now();
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = attempt() {
            return Ok(stream);
        }
        let mut delay = exponential_back_off(&mut duration);
        if let Some(timeout) = timeout {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(ConnectionError::Timeout);
            }
            delay = min(delay, remaining);
        }
        thread::sleep(delay);
    }
}

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_daemon_startup_times_out() {
        let started = Instant::now();
        let result = retry_connect::<()>(
            || Err(io::ErrorKind::ConnectionRefused.into()),
            Some(Duration::from_millis(30)),
        );
        assert!(matches!(result, Err(ConnectionError::Timeout)));
        assert!(started.elapsed() >= Duration::from_millis(30));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn daemon_can_become_available_during_startup() {
        let mut attempts = 0;
        let result = retry_connect(
            || {
                attempts += 1;
                if attempts == 2 {
                    Ok(42)
                } else {
                    Err(io::ErrorKind::NotFound.into())
                }
            },
            Some(Duration::from_secs(1)),
        );
        assert_eq!(result.unwrap(), 42);
        assert_eq!(attempts, 2);
    }
}
