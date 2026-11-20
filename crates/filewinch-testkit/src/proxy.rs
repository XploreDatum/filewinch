//! A TCP forwarder that can cut connections and add latency, for testing resume and retries.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Default)]
struct State {
    /// Bytes (both directions, all connections) left before the next cut; None = disarmed.
    budget: Option<u64>,
    latency: Duration,
    conns: Vec<TcpStream>,
}

/// Forwards `127.0.0.1:port` to a target. Stops accepting on drop and cuts open connections.
pub struct FaultProxy {
    pub port: u16,
    pub addr: SocketAddr,
    state: Arc<Mutex<State>>,
    stop: Arc<AtomicBool>,
    cuts: Arc<AtomicU64>,
    accepted: Arc<AtomicU64>,
}

impl FaultProxy {
    pub fn start(target: SocketAddr) -> FaultProxy {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind proxy");
        let addr = listener.local_addr().expect("proxy addr");
        let state = Arc::new(Mutex::new(State::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let cuts = Arc::new(AtomicU64::new(0));
        let accepted = Arc::new(AtomicU64::new(0));
        let (st, sp, cu, ac) = (state.clone(), stop.clone(), cuts.clone(), accepted.clone());
        std::thread::spawn(move || {
            for client in listener.incoming() {
                if sp.load(Ordering::SeqCst) {
                    return;
                }
                let Ok(client) = client else { continue };
                let Ok(server) = TcpStream::connect(target) else {
                    let _ = client.shutdown(Shutdown::Both);
                    continue;
                };
                ac.fetch_add(1, Ordering::SeqCst);
                let _ = client.set_nodelay(true);
                let _ = server.set_nodelay(true);
                {
                    let mut s = st.lock().unwrap();
                    s.conns.retain(|c| c.peer_addr().is_ok());
                    s.conns.push(client.try_clone().expect("clone"));
                    s.conns.push(server.try_clone().expect("clone"));
                }
                for (from, to) in [(client.try_clone().expect("clone"), server.try_clone().expect("clone")), (server, client)] {
                    let (st, cu) = (st.clone(), cu.clone());
                    std::thread::spawn(move || pump(from, to, &st, &cu));
                }
            }
        });
        FaultProxy { port: addr.port(), addr, state, stop, cuts, accepted }
    }

    /// Cuts every open connection once `n` more bytes have passed (counting both directions
    /// over all connections), then disarms. Bytes up to the cut are delivered.
    pub fn cut_after(&self, n: u64) {
        self.state.lock().unwrap().budget = Some(n);
    }

    /// Disarms a pending `cut_after`.
    pub fn disarm(&self) {
        self.state.lock().unwrap().budget = None;
    }

    /// Delay added before forwarding each chunk.
    pub fn set_latency(&self, d: Duration) {
        self.state.lock().unwrap().latency = d;
    }

    /// Cuts every open connection now.
    pub fn cut_all(&self) {
        let mut s = self.state.lock().unwrap();
        cut(&mut s, &self.cuts);
    }

    /// How many times connections were cut.
    pub fn cuts(&self) -> u64 {
        self.cuts.load(Ordering::SeqCst)
    }

    /// How many connections were accepted.
    pub fn accepted(&self) -> u64 {
        self.accepted.load(Ordering::SeqCst)
    }
}

fn cut(s: &mut State, cuts: &AtomicU64) {
    for c in s.conns.drain(..) {
        let _ = c.shutdown(Shutdown::Both);
    }
    cuts.fetch_add(1, Ordering::SeqCst);
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr); // wake the accept loop
        let mut s = self.state.lock().unwrap();
        for c in s.conns.drain(..) {
            let _ = c.shutdown(Shutdown::Both);
        }
    }
}

fn pump(mut from: TcpStream, mut to: TcpStream, st: &Mutex<State>, cuts: &AtomicU64) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        let (latency, allowed, cut_now) = {
            let mut s = st.lock().unwrap();
            match s.budget {
                Some(b) if (n as u64) >= b => {
                    s.budget = None;
                    (s.latency, b as usize, true)
                }
                Some(b) => {
                    s.budget = Some(b - n as u64);
                    (s.latency, n, false)
                }
                None => (s.latency, n, false),
            }
        };
        if !latency.is_zero() {
            std::thread::sleep(latency);
        }
        if allowed > 0 && to.write_all(&buf[..allowed]).is_err() {
            break;
        }
        if cut_now {
            let mut s = st.lock().unwrap();
            cut(&mut s, cuts);
            break;
        }
    }
    let _ = to.shutdown(Shutdown::Write);
}
