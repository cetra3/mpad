use automerge::transaction::Transactable;
use automerge::{AutoCommit, ObjType, ReadDoc, ROOT};
use rand::Rng;

use socket2::{Domain, Socket, Type};
use tokio::net::UdpSocket;
use tokio::runtime::Builder;

use std::net::{Ipv4Addr, SocketAddr};

use std::convert::TryInto;

use eyre::Result;
use std::collections::BTreeMap;

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
}

pub fn setup(tx: GlibSender<String>) -> TokioSender<TextChange> {
    let (tokio_tx, tokio_rx) = tokio_channel(128);

    thread::spawn(|| {
        let runtime = Builder::new_current_thread()
            .thread_name("mpad-multicast")
            .enable_all()
            .build()
            .expect("Could not build tokio runtime!");

        let site_id = rand::thread_rng().gen();

        if let Err(err) = runtime.block_on(async_inner(site_id, tx, tokio_rx)) {
            error!("Error with async task:{err:?}");
        }
    });

    tokio_tx
}

#[instrument(skip(tx, rx))]
pub async fn async_inner(
    site_id: u32,
    tx: GlibSender<String>,
    rx: TokioReceiver<TextChange>,
) -> Result<()> {
    let site = Arc::new(RwLock::new(AutoCommit::new()));

    let read = read_from_multicast(site_id, site.clone(), tx);
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
#[instrument(skip(site_id, site, tx))]
pub async fn read_from_multicast(site_id: u32, site: Site, tx: GlibSender<String>) -> Result<()> {
    let mut buf = [0u8; BUF_SIZE + 16];

    let mut map = BTreeMap::new();

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

        if incoming_site_id == site_id {
            trace!("Our packet, skipping!");
            continue;
        }

        let mut partials: SitePartials = map.remove(&incoming_site_id).unwrap_or_default();

        partials.from_buffer(amt, &buf);

        if partials.can_reconstruct() {
            let total_buffer = partials.get_buffer();

            debug!("Reconstructed message len:{}", total_buffer.len());

            let mut other = AutoCommit::load(&total_buffer)?;

            let mut wrt = site.write().await;
            wrt.merge(&mut other).unwrap();

            let text_id = wrt.get(ROOT, "text")?.unwrap().1;

            let new_value = wrt.text(&text_id).unwrap();

            tx.send(new_value)?;
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
        let maybe_id = { site.read().await.get(ROOT, "text")?.map(|val| val.1) };

        let send_text_id = match maybe_id {
            Some(val) => val,
            None => site.write().await.put_object(ROOT, "text", ObjType::Text)?,
        };

        let mut slock = site.write().await;

        match change {
            TextChange::Insert { offset, text } => {
                slock.splice_text(&send_text_id, offset, 0, &text)?;
            }
            TextChange::Remove { offset, len } => {
                slock.splice_text(&send_text_id, offset, len, "")?;
            }
        };

        let state = slock.save();

        send_state(site_id, seq, &socket, &send_addr, state).await?;

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
        let mut partials = Vec::new();

        std::mem::swap(&mut partials, &mut self.partials);

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
