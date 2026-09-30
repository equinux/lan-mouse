//! macOS clipboard sharing inspired by upstream PR #438's opt-in, per-pair
//! permissions and sensitive-source suppression. Transport and state handling
//! are independent: pinned mutual TLS 1.3, bounded frames, no fan-out/history.
#[cfg(target_os = "macos")]
mod macos;
mod tls;
mod wire;

use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, interval, sleep, timeout},
};
use tokio_rustls::{TlsAcceptor, TlsConnector};
use webrtc_dtls::crypto::Certificate;
use wire::{MAX_TEXT, Message};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ClipboardConfig {
    pub enabled: bool,
    pub send: bool,
    pub receive: bool,
    /// Exact certificate fingerprint, obtained through a trusted channel.
    pub peer_fingerprint: String,
    /// Set on exactly one endpoint. The other listens on `listen_address`.
    pub peer_address: Option<SocketAddr>,
    pub listen_address: SocketAddr,
    pub suppress_apps: Vec<String>,
}
impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            send: false,
            receive: false,
            peer_fingerprint: String::new(),
            peer_address: None,
            listen_address: "127.0.0.1:4243".parse().expect("literal address"),
            suppress_apps: vec![
                "com.1password.1password",
                "com.agilebits.onepassword7",
                "com.bitwarden.desktop",
                "org.keepassxc.keepassxc",
                "com.apple.keychainaccess",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}

pub(crate) struct Clipboard {
    task: JoinHandle<()>,
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Clipboard {
    pub(crate) async fn stop(mut self) {
        self.task.abort();
        let _ = (&mut self.task).await;
    }
    pub(crate) async fn start(
        config: ClipboardConfig,
        cert: Certificate,
        authorized: Arc<RwLock<HashMap<String, String>>>,
    ) -> io::Result<Option<Self>> {
        if !config.enabled {
            return Ok(None);
        }
        if !config.send && !config.receive {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clipboard needs explicit send and/or receive permission",
            ));
        }
        let pin = tls::parse_pin(&config.peer_fingerprint)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let own_pin = tls::fingerprint(cert.certificate.first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing clipboard certificate")
        })?);
        if pin == own_pin {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "clipboard peer must be another identity",
            ));
        }
        let trusted_pin = config.peer_fingerprint.to_ascii_lowercase();
        if !authorized
            .read()
            .map(|keys| keys.contains_key(&trusted_pin))
            .unwrap_or(false)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "clipboard peer must also be in authorized_fingerprints",
            ));
        }
        #[cfg(not(target_os = "macos"))]
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "clipboard sharing currently requires macOS",
        ));
        #[cfg(target_os = "macos")]
        {
            let (client, server) = tls::configs(&cert, pin).map_err(io::Error::other)?;
            // Fail configuration/startup promptly rather than silently running
            // an enabled clipboard feature with no listening socket.
            let listener = if config.peer_address.is_none() {
                Some(TcpListener::bind(config.listen_address).await?)
            } else {
                None
            };
            let task = tokio::task::spawn_local(async move {
                let connector = TlsConnector::from(client);
                let acceptor = TlsAcceptor::from(server);
                let allowed = || {
                    authorized
                        .read()
                        .map(|keys| keys.contains_key(&trusted_pin))
                        .unwrap_or(false)
                };
                log::info!(
                    "Clipboard enabled for one pinned peer; text only, maximum {MAX_TEXT} bytes"
                );
                loop {
                    if !allowed() {
                        log::warn!("Clipboard stopped: peer authorization revoked");
                        break;
                    }
                    let result = if let Some(addr) = config.peer_address {
                        match timeout(wire::DEADLINE, async {
                            let tcp = TcpStream::connect(addr).await?;
                            tcp.set_nodelay(true)?;
                            connector
                                .connect(
                                    rustls::pki_types::ServerName::try_from("ignored")
                                        .expect("literal name"),
                                    tcp,
                                )
                                .await
                        })
                        .await
                        {
                            Ok(Ok(stream)) => {
                                if stream.get_ref().1.alpn_protocol()
                                    != Some(b"lan-mouse-clipboard/1")
                                {
                                    Err(io::Error::other("clipboard protocol negotiation failed"))
                                } else {
                                    session(
                                        stream,
                                        &config,
                                        own_pin,
                                        pin,
                                        &macos::MacBoard,
                                        &allowed,
                                    )
                                    .await
                                }
                            }
                            Ok(Err(e)) => Err(e),
                            Err(_) => Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                "clipboard handshake timeout",
                            )),
                        }
                    } else if let Some(listener) = &listener {
                        match listener.accept().await {
                            Ok((tcp, _)) => {
                                match timeout(wire::DEADLINE, acceptor.accept(tcp)).await {
                                    Ok(Ok(stream)) => {
                                        if stream.get_ref().1.alpn_protocol()
                                            != Some(b"lan-mouse-clipboard/1")
                                        {
                                            Err(io::Error::other(
                                                "clipboard protocol negotiation failed",
                                            ))
                                        } else {
                                            session(
                                                stream,
                                                &config,
                                                own_pin,
                                                pin,
                                                &macos::MacBoard,
                                                &allowed,
                                            )
                                            .await
                                        }
                                    }
                                    Ok(Err(e)) => Err(e),
                                    Err(_) => Err(io::Error::new(
                                        io::ErrorKind::TimedOut,
                                        "clipboard handshake timeout",
                                    )),
                                }
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        break;
                    };
                    // Errors never include frames or clipboard contents.
                    if let Err(e) = result {
                        log::warn!("Clipboard connection ended: {e}");
                    }
                    sleep(Duration::from_secs(2)).await;
                }
            });
            Ok(Some(Self { task }))
        }
    }
}

trait Board {
    fn ready(&self) -> bool;
    fn count(&self) -> isize;
    fn read(&self, expected: isize, apps: &[String]) -> Option<String>;
    fn write(&self, expected: isize, text: &str, apps: &[String]) -> bool;
}

/// Per-session Lamport revision + authenticated identity breaks simultaneous
/// copy ties deterministically. No wall-clock trust or sender-supplied origin.
struct Changes {
    count: isize,
    clock: u64,
    remote_clock: u64,
    winner: (u64, [u8; 32]),
    local_pin: [u8; 32],
    peer_pin: [u8; 32],
    ready: bool,
    peer: Option<(bool, bool, bool)>,
}
impl Changes {
    fn new(board: &impl Board, local_pin: [u8; 32], peer_pin: [u8; 32]) -> Self {
        log::debug!("Clipboard local session ready={}", board.ready());
        Self {
            count: board.count(),
            clock: 0,
            remote_clock: 0,
            winner: (0, [0; 32]),
            local_pin,
            peer_pin,
            ready: board.ready(),
            peer: None,
        }
    }
    fn poll(
        &mut self,
        board: &impl Board,
        config: &ClipboardConfig,
    ) -> io::Result<Option<Message>> {
        let ready = board.ready();
        let count = board.count();
        // Ignore copies made while paused; unlock must not export them later.
        if !ready || ready != self.ready {
            self.count = count;
            self.ready = ready;
            return Ok(None);
        }
        if self.count == count {
            return Ok(None);
        }
        self.count = count; // Advance even for concealed/oversized/denied reads.
        self.clock = self
            .clock
            .checked_add(1)
            .ok_or_else(|| io::Error::other("clipboard revision overflow"))?;
        self.winner = (self.clock, self.local_pin);
        if !config.send || !matches!(self.peer, Some((_, true, true))) {
            return Ok(None);
        }
        let text = board.read(count, &config.suppress_apps);
        if text.is_none() {
            log::debug!("Clipboard change skipped: sensitive, unavailable, or unsupported data");
        }
        Ok(text.map(|text| Message::Text {
            revision: self.clock,
            text,
        }))
    }
    fn state(
        &mut self,
        board: &impl Board,
        send: bool,
        receive: bool,
        ready: bool,
    ) -> io::Result<()> {
        if let Some((old_send, old_receive, _)) = self.peer {
            if (send, receive) != (old_send, old_receive) {
                return Err(io::Error::other(
                    "clipboard capabilities changed within a session",
                ));
            }
        }
        if self.peer.is_none_or(|(_, _, previous)| previous != ready) {
            self.count = board.count();
            log::debug!(
                "Clipboard peer permissions: send={send}, receive={receive}, ready={ready}"
            );
        }
        self.peer = Some((send, receive, ready));
        Ok(())
    }
    fn accept(
        &mut self,
        board: &impl Board,
        config: &ClipboardConfig,
        revision: u64,
        text: &str,
    ) -> io::Result<()> {
        if revision <= self.remote_clock {
            return Err(io::Error::other("non-increasing clipboard revision"));
        }
        self.remote_clock = revision;
        self.clock = self.clock.max(revision);
        let version = (revision, self.peer_pin);
        if config.receive
            && board.ready()
            && matches!(self.peer, Some((true, _, true)))
            && version > self.winner
            && board.write(self.count, text, &config.suppress_apps)
        {
            self.count = board.count(); // Own write must never echo back.
            self.winner = version;
        }
        Ok(())
    }
}

async fn session<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(
    stream: S,
    config: &ClipboardConfig,
    own_pin: [u8; 32],
    peer_pin: [u8; 32],
    board: &impl Board,
    allowed: impl Fn() -> bool,
) -> io::Result<()> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    // Read in a dedicated task so select cancellation cannot lose a partial
    // header. Bounded queue and frame limit bound even an authenticated peer.
    let (tx, mut rx) = mpsc::channel(2);
    let task = tokio::spawn(async move {
        loop {
            let message = wire::read(&mut reader).await;
            let failed = message.is_err();
            if tx.send(message).await.is_err() || failed {
                break;
            }
        }
    });
    let _reader = Clipboard { task }; // Abort reader on every exit, including config reload.
    let mut changes = Changes::new(board, own_pin, peer_pin);
    let mut tick = interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = Instant::now() - Duration::from_secs(1);
    let mut budget = (Instant::now(), 0u16);
    log::info!("Clipboard peer authenticated with mutual TLS; waiting for clipboard changes");
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if !allowed() { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "clipboard peer authorization revoked")); }
                if let Some(message) = changes.poll(board, config)? { wire::write(&mut writer, &message).await?; }
                if heartbeat.elapsed() >= Duration::from_secs(1) {
                    wire::write(&mut writer, &Message::State { send: config.send, receive: config.receive, ready: board.ready() }).await?;
                    heartbeat = Instant::now();
                }
            }
            message = rx.recv() => {
                if !allowed() { return Err(io::Error::new(io::ErrorKind::PermissionDenied, "clipboard peer authorization revoked")); }
                if budget.0.elapsed() >= Duration::from_secs(1) { budget = (Instant::now(), 0); }
                budget.1 += 1;
                if budget.1 > 100 { return Err(io::Error::other("clipboard peer exceeded message rate limit")); }
                let message = message.ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "clipboard reader closed"))??;
                match message {
                    Message::State { send, receive, ready } => changes.state(board,send,receive,ready)?,
                    Message::Text { revision, text } => {
                        // Observe a racing local copy before applying an incoming
                        // one, so both endpoints choose the same revision winner.
                        if let Some(message) = changes.poll(board,config)? { wire::write(&mut writer,&message).await?; }
                        changes.accept(board,config,revision,&text)?;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[derive(Default)]
    struct MemoryBoard(Mutex<(isize, String, bool, bool)>);
    impl MemoryBoard {
        fn copy(&self, value: &str, sensitive: bool) {
            let mut b = self.0.lock().unwrap();
            b.0 += 1;
            b.1 = value.into();
            b.3 = sensitive;
        }
        fn unlock(&self, value: bool) {
            self.0.lock().unwrap().2 = value;
        }
        fn text(&self) -> String {
            self.0.lock().unwrap().1.clone()
        }
    }
    impl Board for MemoryBoard {
        fn ready(&self) -> bool {
            self.0.lock().unwrap().2
        }
        fn count(&self) -> isize {
            self.0.lock().unwrap().0
        }
        fn read(&self, expected: isize, _: &[String]) -> Option<String> {
            let b = self.0.lock().unwrap();
            (b.0 == expected && b.2 && !b.3 && b.1.len() <= MAX_TEXT).then(|| b.1.clone())
        }
        fn write(&self, expected: isize, text: &str, _: &[String]) -> bool {
            let mut b = self.0.lock().unwrap();
            if b.0 != expected || !b.2 || b.3 {
                return false;
            }
            b.0 += 1;
            b.1 = text.into();
            true
        }
    }
    fn config() -> ClipboardConfig {
        ClipboardConfig {
            enabled: true,
            send: true,
            receive: true,
            ..Default::default()
        }
    }
    fn changes(board: &MemoryBoard, own: u8, peer: u8) -> Changes {
        let mut c = Changes::new(board, [own; 32], [peer; 32]);
        c.state(board, true, true, true).unwrap();
        c
    }
    #[test]
    fn defaults_require_explicit_pair_and_permissions() {
        let c: ClipboardConfig = toml::from_str("").unwrap();
        assert!(!c.enabled && !c.send && !c.receive);
        assert!(tls::parse_pin(&c.peer_fingerprint).is_err());
        assert!(toml::from_str::<ClipboardConfig>("recieve = true").is_err());
    }
    #[test]
    fn suppression_advances_state_and_never_leaks_on_focus_change() {
        let b = MemoryBoard::default();
        b.unlock(true);
        let mut c = changes(&b, 1, 2);
        b.copy("secret", true);
        assert!(c.poll(&b, &config()).unwrap().is_none());
        b.0.lock().unwrap().3 = false;
        assert!(c.poll(&b, &config()).unwrap().is_none());
        b.copy(&"x".repeat(MAX_TEXT + 1), false);
        assert!(c.poll(&b, &config()).unwrap().is_none());
        b.copy("normal", false);
        assert!(matches!(
            c.poll(&b, &config()).unwrap(),
            Some(Message::Text { .. })
        ));
    }
    #[test]
    fn startup_reconnect_unlock_and_peer_resume_do_not_export_old_text() {
        let b = MemoryBoard::default();
        b.unlock(true);
        b.copy("old", false);
        let mut c = changes(&b, 1, 2);
        assert!(c.poll(&b, &config()).unwrap().is_none());
        b.unlock(false);
        b.copy("locked secret", false);
        c.poll(&b, &config()).unwrap();
        b.unlock(true);
        assert!(c.poll(&b, &config()).unwrap().is_none());
        c.state(&b, true, true, false).unwrap();
        b.copy("while peer locked", false);
        c.poll(&b, &config()).unwrap();
        c.state(&b, true, true, true).unwrap();
        assert!(c.poll(&b, &config()).unwrap().is_none());
        let mut reconnected = changes(&b, 1, 2);
        assert!(reconnected.poll(&b, &config()).unwrap().is_none());
    }
    #[test]
    fn simultaneous_copies_converge_and_remote_writes_do_not_echo() {
        let a = MemoryBoard::default();
        let b = MemoryBoard::default();
        a.unlock(true);
        b.unlock(true);
        let mut ca = changes(&a, 1, 2);
        let mut cb = changes(&b, 2, 1);
        a.copy("A", false);
        b.copy("B", false);
        let Some(Message::Text {
            revision: ra,
            text: ta,
        }) = ca.poll(&a, &config()).unwrap()
        else {
            panic!()
        };
        let Some(Message::Text {
            revision: rb,
            text: tb,
        }) = cb.poll(&b, &config()).unwrap()
        else {
            panic!()
        };
        ca.accept(&a, &config(), rb, &tb).unwrap();
        cb.accept(&b, &config(), ra, &ta).unwrap();
        assert_eq!(a.text(), "B");
        assert_eq!(a.text(), b.text());
        assert!(ca.poll(&a, &config()).unwrap().is_none());
        assert!(ca.accept(&a, &config(), rb, &tb).is_err());
    }
    #[test]
    fn receive_permission_lock_and_racing_copy_are_enforced() {
        let b = MemoryBoard::default();
        b.unlock(true);
        let mut c = changes(&b, 1, 2);
        let mut disabled = config();
        disabled.receive = false;
        c.accept(&b, &disabled, 1, "remote").unwrap();
        assert_eq!(b.text(), "");
        b.unlock(false);
        c.accept(&b, &config(), 2, "remote").unwrap();
        assert_eq!(b.text(), "");
        b.unlock(true);
        b.copy("just copied locally", false);
        // Compare-and-check before setting avoids overwriting an unseen copy.
        c.accept(&b, &config(), 3, "remote").unwrap();
        assert_eq!(b.text(), "just copied locally");
        c.state(&b, false, true, true).unwrap_err();
    }
    #[tokio::test]
    async fn live_session_syncs_both_directions_without_echo() {
        let a = Arc::new(MemoryBoard::default());
        let b = Arc::new(MemoryBoard::default());
        a.unlock(true);
        b.unlock(true);
        a.copy("do not send at startup", false);
        let (left, right) = tokio::io::duplex(4096);
        let aa = a.clone();
        let bb = b.clone();
        let ta = tokio::spawn(async move {
            session(left, &config(), [1; 32], [2; 32], aa.as_ref(), || true).await
        });
        let tb = tokio::spawn(async move {
            session(right, &config(), [2; 32], [1; 32], bb.as_ref(), || true).await
        });
        sleep(Duration::from_millis(250)).await;
        assert_eq!(b.text(), "");
        a.copy("A 🌍", false);
        timeout(Duration::from_secs(2), async {
            while b.text() != "A 🌍" {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        b.copy("B\nsecond line", false);
        timeout(Duration::from_secs(2), async {
            while a.text() != "B\nsecond line" {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let ac = a.count();
        let bc = b.count();
        sleep(Duration::from_millis(300)).await;
        assert_eq!(a.count(), ac);
        assert_eq!(b.count(), bc);
        ta.abort();
        tb.abort();
    }
    #[tokio::test]
    async fn revocation_closes_an_existing_session() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let board = Arc::new(MemoryBoard::default());
        board.unlock(true);
        let permission = Arc::new(AtomicBool::new(true));
        let gate = permission.clone();
        let (left, _right) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            session(left, &config(), [1; 32], [2; 32], board.as_ref(), || {
                gate.load(Ordering::SeqCst)
            })
            .await
        });
        sleep(Duration::from_millis(30)).await;
        permission.store(false, Ordering::SeqCst);
        let result = timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
    }
}
