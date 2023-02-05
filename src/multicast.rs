use automerge::transaction::Transactable;
use automerge::{AutoCommit, ObjType, ReadDoc, ROOT};

use eyre::eyre;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Socket, Type};
use tokio::net::UdpSocket;
use tokio::runtime::Builder;

use std::net::{Ipv4Addr, SocketAddr};

use std::convert::TryInto;

use eyre::Result;
use std::collections::{BTreeMap, BTreeSet};

use tracing::*;

use tokio::sync::mpsc::{
    channel as tokio_channel, Receiver as TokioReceiver, Sender as TokioSender,
};

use tokio::sync::RwLock;

use glib::Sender as GlibSender;

use std::thread;

use std::cmp;

use std::sync::Arc;

const BUF_SIZE: usize = 1400;

#[derive(Debug)]
pub enum TextChange {
    Insert { offset: usize, text: String },
    Remove { offset: usize, len: usize },
    SendState,
    RequestState { site_id: u32 },
}

#[derive(Serialize, Deserialize, Debug)]
pub enum McastMessage {
    DeltaChange(Vec<u8>),
    State(Vec<u8>),
    RequestState(u32),
}

pub fn setup(tx: GlibSender<String>, site_id: u32) -> TokioSender<TextChange> {
    let (tokio_tx, tokio_rx) = tokio_channel(128);

    let tokio_tx_mcast = tokio_tx.clone();

    thread::spawn(move || {
        let runtime = Builder::new_current_thread()
            .thread_name("mpad-multicast")
            .enable_all()
            .build()
            .expect("Could not build tokio runtime!");

        if let Err(err) = runtime.block_on(async_inner(site_id, tx, tokio_rx, tokio_tx_mcast)) {
            error!("Error with async task:{err:?}");
        }
    });

    tokio_tx
}

#[instrument(skip(tx, rx, tokio_tx))]
pub async fn async_inner(
    site_id: u32,
    tx: GlibSender<String>,
    rx: TokioReceiver<TextChange>,
    tokio_tx: TokioSender<TextChange>,
) -> Result<()> {
    let mut autocommit = AutoCommit::new();
    let text_id = autocommit.put_object(ROOT, "text", ObjType::Text)?;
    // fill it in
    autocommit.splice_text(&text_id, 0, 0, "")?;

    let site = Arc::new(RwLock::new(autocommit));

    let read = read_from_multicast(site_id, site.clone(), tx, tokio_tx);
    let write = write_to_multicast(site_id, site, rx);

    tokio::select! {
        val = read => {
            val?;
        },
        val = write => {
            val?;
        }
    };

    Ok(())
}

type Site = Arc<RwLock<AutoCommit>>;

// Reads packets from the multicast group and updates local state if necessary

pub async fn read_from_multicast(
    our_id: u32,
    site: Site,
    tx: GlibSender<String>,
    tokio_tx: TokioSender<TextChange>,
) -> Result<()> {
    let mut buf = [0u8; BUF_SIZE + 16];

    // A map of site partials
    let mut map = BTreeMap::new();

    // a set of sites we have received their full state from
    let mut state_set = BTreeSet::new();

    let recv_socket = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    recv_socket.set_reuse_address(true)?;
    recv_socket.set_nonblocking(true)?;
    recv_socket.join_multicast_v4(&Ipv4Addr::new(239, 1, 1, 1), &Ipv4Addr::new(0, 0, 0, 0))?;
    recv_socket.bind(&"0.0.0.0:1111".parse::<SocketAddr>()?.into())?;

    let socket = UdpSocket::from_std(recv_socket.into())?;

    loop {
        let (amt, src) = socket.recv_from(&mut buf).await?;

        trace!("received {} bytes from {:?}", amt, src);

        if amt < 16 {
            warn!("Amount on the wire is below the partial header size!");
            continue;
        }

        let incoming_site_id = u32::from_be_bytes(buf[0..4].try_into().unwrap());

        if incoming_site_id == our_id {
            trace!("Our packet, skipping!");
            continue;
        }

        let mut partials: SitePartials = map.remove(&incoming_site_id).unwrap_or_default();

        partials.from_buffer(amt, &buf);

        if partials.can_reconstruct() {
            let total_buffer = partials.get_buffer();
            debug!("Reconstructed message len:{}", total_buffer.len());

            let mcast_message: McastMessage = bincode::deserialize(&total_buffer)?;

            match mcast_message {
                McastMessage::DeltaChange(val) => {
                    debug!("Site:{} DeltaChange:{}", incoming_site_id, val.len());
                    // If we've seen this site before
                    if state_set.contains(&incoming_site_id) {
                        let mut wrt = site.write().await;
                        wrt.load_incremental(&val)?;

                        let text_id = wrt
                            .get(ROOT, "text")?
                            .ok_or_else(|| eyre!("Text Object not initialised!"))?
                            .1;

                        let new_value = wrt.text(&text_id).unwrap();

                        tx.send(new_value)?;
                    } else {
                        // request the full state
                        tokio_tx
                            .send(TextChange::RequestState {
                                site_id: incoming_site_id,
                            })
                            .await?;
                    }
                }
                McastMessage::State(val) => {
                    debug!("Site:{} State:{}", incoming_site_id, val.len());
                    let mut other = AutoCommit::load(&val)?;

                    let mut wrt = site.write().await;

                    wrt.merge(&mut other)?;

                    let text_id = wrt
                        .get(ROOT, "text")?
                        .ok_or_else(|| eyre!("Text Object not initialised!"))?
                        .1;

                    debug!("State: {:#?}", wrt);
                    let new_value = wrt.text(&text_id)?;

                    tx.send(new_value)?;

                    state_set.insert(incoming_site_id);
                }
                McastMessage::RequestState(site_id) => {
                    debug!("Site:{} RequestState:{}", incoming_site_id, site_id);
                    if site_id == our_id {
                        tokio_tx.send(TextChange::SendState).await?;
                    }
                }
            }
        } else {
            map.insert(incoming_site_id, partials);
        }
    }
}

#[instrument(skip(site_id, site, recv))]
pub async fn write_to_multicast(
    site_id: u32,
    site: Site,
    mut recv: TokioReceiver<TextChange>,
) -> Result<()> {
    let mut seq: u32 = 1;

    let send_socket = Socket::new(Domain::IPV4, Type::DGRAM, None)?;
    send_socket.set_nonblocking(true)?;

    let send_addr = "239.1.1.1:1111".parse::<SocketAddr>()?.into();

    let socket = UdpSocket::from_std(send_socket.into())?;

    while let Some(change) = recv.recv().await {
        let mut slock = site.write().await;
        let send_text_id = slock
            .get(ROOT, "text")?
            .ok_or_else(|| eyre!("Text Object not initialised!"))?
            .1;

        let to_send = match change {
            TextChange::Insert { offset, text } => {
                slock.splice_text(&send_text_id, offset, 0, &text)?;
                McastMessage::DeltaChange(slock.save_incremental())
            }
            TextChange::Remove { offset, len } => {
                slock.splice_text(&send_text_id, offset, len, "")?;
                McastMessage::DeltaChange(slock.save_incremental())
            }
            TextChange::SendState => McastMessage::State(slock.save()),
            TextChange::RequestState { site_id } => McastMessage::RequestState(site_id),
        };

        let encoded: Vec<u8> = bincode::serialize(&to_send)?;

        send_state(site_id, seq, &socket, &send_addr, encoded).await?;

        seq = seq.wrapping_add(1);
    }

    Ok(())
}

async fn send_state(
    site_id: u32,
    seq: u32,
    socket: &UdpSocket,
    addr: &SocketAddr,
    mut val: Vec<u8>,
) -> Result<()> {
    let mut to_send = val.len();

    trace!("Total len to send:{}", to_send);

    let num = (to_send / BUF_SIZE) as u32 + 1;

    trace!("Total number to send:{}", num);

    let mut idx: u32 = 0;

    while to_send > 0 {
        let end = cmp::min(BUF_SIZE, val.len());
        let new_val = val.split_off(end);

        let mut body = val;

        trace!("Body len:{}", body.len());

        let mut payload = Vec::new();
        trace!(
            "Sending Site:{}, Seq:{}, Num:{} Idx:{}",
            site_id,
            seq,
            num,
            idx
        );

        payload.append(&mut site_id.to_be_bytes().to_vec());
        payload.append(&mut seq.to_be_bytes().to_vec());
        payload.append(&mut num.to_be_bytes().to_vec());
        payload.append(&mut idx.to_be_bytes().to_vec());
        payload.append(&mut body);

        val = new_val;

        socket.send_to(&payload, &addr).await?;

        to_send = to_send - end;

        idx += 1;
    }

    Ok(())
}

struct SitePartials {
    seq: u32,
    num: u32,
    partials: Vec<(u32, Vec<u8>)>,
}

impl SitePartials {
    fn from_buffer(&mut self, amt: usize, buf: &[u8]) {
        let seq = u32::from_be_bytes(buf[4..8].try_into().unwrap());
        let num = u32::from_be_bytes(buf[8..12].try_into().unwrap());
        let idx = u32::from_be_bytes(buf[12..16].try_into().unwrap());

        trace!("Seq:{}, Num:{}, Idx:{}", seq, num, idx);

        if seq < self.seq {
            trace!("Discarding older partial");
            return;
        } else if seq > self.seq {
            trace!("Resetting partials for site");
            self.partials = vec![];
            self.seq = seq;
            self.num = num;
        }

        let partial = buf[16..amt].to_vec();

        trace!("Partial len:{}", partial.len());

        self.partials.push((idx, partial));
    }

    fn can_reconstruct(&self) -> bool {
        trace!("Current length:{}", self.partials.len());
        self.partials.len() == self.num as usize
    }

    fn get_buffer(&mut self) -> Vec<u8> {
        let mut partials = std::mem::take(&mut self.partials);

        partials.sort_by(|(left, _), (right, _)| left.cmp(right));

        partials
            .into_iter()
            .map(|(_idx, buf)| buf)
            .flatten()
            .collect()
    }
}

impl Default for SitePartials {
    fn default() -> Self {
        Self {
            seq: 0,
            num: 0,
            partials: vec![],
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_automerge() {
        let mut site_1 = AutoCommit::new();
        let site_1_text_id = site_1.put_object(ROOT, "text", ObjType::Text).unwrap();
        site_1.splice_text(&site_1_text_id, 0, 0, "").unwrap();

        let mut site_2 = AutoCommit::new();
        let site2_text_id = site_2.put_object(ROOT, "text", ObjType::Text).unwrap();
        site_2
            .splice_text(&site2_text_id, 0, 0, "testing automerge")
            .unwrap();

        let state = site_2.save();

        let mut site_2_load = AutoCommit::load(&state).unwrap();

        site_1.merge(&mut site_2_load).unwrap();

        let text_id = site_1.get(ROOT, "text").unwrap().unwrap().1;

        assert_eq!(site_1.text(text_id).unwrap(), "testing automerge")
    }
}
