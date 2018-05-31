use std::collections::{HashMap, HashSet};
use std::io;
use std::{cell::RefCell, net::SocketAddr, usize};

use crypto::{digest::Digest, sha2::Sha256};
use mio::{net::TcpListener, unix::UnixReady, Events, Poll, PollOpt, Ready, Token};
use pool::DcPool;
use pump::Pump;
use slab::Slab;

const MAX_PUMPS: usize = 1024 * 1024;
const ROOT_TOKEN: Token = Token(<usize>::max_value() - 1);

pub struct Server {
  sock: TcpListener,
  poll: Poll,
  secret: Vec<u8>,
  pool: DcPool,
  pumps: Slab<RefCell<Pump>>,
  detached: HashSet<Token>,
  links: HashMap<Token, Token>,
}

impl Server {
  pub fn new(addr: SocketAddr, seed: &str) -> Server {
    let mut sha = Sha256::new();
    let mut secret = vec![0u8; sha.output_bytes()];

    sha.input_str(seed);
    sha.result(&mut secret);
    secret.truncate(16);

    Server {
      secret,
      pool: DcPool::new(),
      detached: HashSet::new(),
      sock: TcpListener::bind(&addr).expect("Failed to bind"),
      poll: Poll::new().expect("Failed to create Poll"),
      pumps: Slab::with_capacity(MAX_PUMPS),
      links: HashMap::new(),
    }
  }

  pub fn init(&mut self) -> io::Result<()> {
    self.pool.start()
  }

  pub fn secret(&self) -> String {
    let secret: Vec<String> = self.secret.iter().map(|b| format!("{:02x}", b)).collect();
    secret.join("")
  }

  pub fn run(&mut self) -> io::Result<()> {
    info!("Starting proxy");
    self
      .poll
      .register(&self.sock, ROOT_TOKEN, Ready::readable(), PollOpt::edge())?;

    let mut events = Events::with_capacity(512);

    loop {
      self.poll.poll(&mut events, None)?;
      self.dispatch(&events)?;
      trace!(
        "pumps: {}, links: {}, detached: {}",
        self.pumps.len(),
        self.links.len(),
        self.detached.len()
      );
    }
  }

  fn dispatch(&mut self, events: &Events) -> io::Result<()> {
    let mut stale = HashSet::new();
    let mut new_peers = HashMap::new();

    for event in events {
      let token = event.token();

      if token == ROOT_TOKEN {
        trace!("accepting new connection");
        self.accept()?;
        continue;
      }

      let readiness = UnixReady::from(event.readiness());
      let mut pump = {
        let pump = &self.pumps.get(token.0);
        if pump.is_none() {
          warn!("slab inconsistency");
          continue;
        }
        pump.unwrap().borrow_mut()
      };

      if readiness.is_readable() {
        trace!("read event: {:?}", token);
        match pump.drain() {
          Ok(Some(mut dc_idx)) => match self.pool.get(dc_idx) {
            Some(mut peer) => {
              let buf = pump.pull();
              if buf.len() > 0 {
                peer.push(&buf);
              }
              new_peers.insert(token, peer);
            }
            None => {
              stale.insert(token);
            }
          },
          Ok(_) => {}
          Err(e) => {
            warn!("drain failed: {:?}: {}", token, e);
            stale.insert(token);
          }
        }
        if let Some(peer_token) = self.links.get(&token) {
          self.fan_out(&mut pump, peer_token)?;
        }
      }

      if readiness.is_writable() {
        trace!("write event: {:?}", token);
        if let Some(peer_token) = self.links.get(&token) {
          self.fan_in(&mut pump, peer_token)?;
        }
        match pump.flush() {
          Ok(_) => {}
          Err(e) => {
            warn!("flush failed: {:?}: {}", token, e);
            stale.insert(token);
            break;
          }
        }
      }

      if readiness.is_hup() {
        trace!("hup event: {:?}", event.token());
        stale.insert(token);
      } else if readiness.is_error() {
        trace!("error event {:?}", event.token());
        stale.insert(token);
      } else {
        self.poll.reregister(
          pump.sock(),
          token,
          pump.interest(),
          PollOpt::edge() | PollOpt::oneshot(),
        )?;
      }
    }

    for (token, peer_pump) in new_peers {
      let idx = self.pumps.insert(RefCell::new(peer_pump));
      let peer_pump = self.pumps.get(idx).unwrap().borrow();

      let peer_token = Token(idx);
      self.links.insert(peer_token, token);
      self.links.insert(token, peer_token);
      info!("linked to dc: {:?} -> {:?}", token, peer_token);

      self.poll.register(
        peer_pump.sock(),
        peer_token,
        peer_pump.interest(),
        PollOpt::edge() | PollOpt::oneshot(),
      )?;
    }

    for token in &self.detached {
      let pump = self.pumps.get(token.0).unwrap();
      let mut pump = pump.borrow_mut();
      if !pump.interest().is_writable() {
        stale.insert(*token);
      }
    }

    for token in stale {
      self.drop_pump(token)?;
    }

    Ok(())
  }

  fn accept(&mut self) -> io::Result<()> {
    if self.pumps.len() > MAX_PUMPS {
      warn!("max connection limit({}) exceeded", MAX_PUMPS / 2);
      return Ok(());
    }

    let sock = match self.sock.accept() {
      Ok((sock, _)) => sock,
      Err(err) => {
        warn!("accept failed: {}", err);
        return Ok(());
      }
    };

    let pump = Pump::downstream(&self.secret, sock);
    let idx = self.pumps.insert(RefCell::new(pump));
    let pump = self.pumps.get(idx).unwrap().borrow();

    let token = Token(idx);

    self.poll.register(
      pump.sock(),
      token,
      pump.interest(),
      PollOpt::edge() | PollOpt::oneshot(),
    )?;

    Ok(())
  }

  fn fan_out(&self, pump: &mut Pump, peer_token: &Token) -> io::Result<()> {
    trace!("fan out to {:?}", peer_token);
    let buf = pump.pull();
    if buf.is_empty() {
      return Ok(());
    }

    let peer = self.pumps.get(peer_token.0).unwrap();
    let mut peer = peer.borrow_mut();
    peer.push(&buf);

    self.poll.reregister(
      peer.sock(),
      *peer_token,
      peer.interest(),
      PollOpt::edge() | PollOpt::oneshot(),
    )?;

    Ok(())
  }

  fn fan_in(&self, pump: &mut Pump, peer_token: &Token) -> io::Result<()> {
    trace!("fan in from {:?}", peer_token);
    let peer = self.pumps.get(peer_token.0).unwrap();
    let mut peer = peer.borrow_mut();

    let buf = peer.pull();
    if buf.is_empty() {
      return Ok(());
    }
    pump.push(&buf);

    self.poll.reregister(
      peer.sock(),
      *peer_token,
      peer.interest(),
      PollOpt::edge() | PollOpt::oneshot(),
    )?;

    Ok(())
  }

  fn drop_pump(&mut self, token: Token) -> io::Result<()> {
    self.detached.remove(&token);

    let pump = self.pumps.remove(token.0);
    let pump = pump.borrow_mut();

    info!("dropping pump: {:?}", token);
    self.poll.deregister(pump.sock())?;
    match self.links.remove(&token) {
      Some(peer_token) => {
        info!("dropping link to peer: {:?} -> {:?}", token, peer_token);
        self.links.remove(&peer_token);
        self.detached.insert(peer_token);
      }
      _ => {}
    }
    Ok(())
  }
}
