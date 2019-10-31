use failure::Error;

use crate::ditto::{Text, TextState};
use serde_cbor;

use rand::Rng;

use socket2::{Domain, SockAddr, Socket, Type};

use std::net::{Ipv4Addr, SocketAddr};

use std::convert::TryInto;

use std::collections::BTreeMap;

use log::*;

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;

use std::cmp;

use std::sync::{Arc, RwLock};

use difference::{Changeset, Difference::*};

pub fn setup_channels() -> Result<(Sender<String>, Receiver<String>), Error> {
    let mut rng = rand::thread_rng();

    let site_id = rng.gen();

    let site = Arc::new(RwLock::new(Text::with_id(site_id)));

    let (inner_sender, receiver) = channel::<String>();
    let (sender, inner_receiver) = channel::<String>();

    let recv_socket = Socket::new(Domain::ipv4(), Type::dgram(), None)?;
    recv_socket.set_reuse_address(true)?;
    recv_socket.join_multicast_v4(&Ipv4Addr::new(239, 1, 1, 1), &Ipv4Addr::new(0, 0, 0, 0))?;
    recv_socket.bind(&"0.0.0.0:1111".parse::<SocketAddr>()?.into())?;

    let send_socket = Socket::new(Domain::ipv4(), Type::dgram(), None)?;
    let send_addr = "239.1.1.1:1111".parse::<SocketAddr>()?.into();

    let send_site = site.clone();

    thread::spawn(move || {
        let mut buf = [0u8; 1500];

        let mut map = BTreeMap::new();

        loop {
            let (amt, src) = recv_socket
                .recv_from(&mut buf)
                .expect("Could not receive from multicast");
            debug!("received {} bytes from {:?}", amt, src);

            if amt < 16 {
                warn!("Amount on the wire is below the partial header size!");
                continue;
            }

            let incoming_site_id = u32::from_be_bytes(buf[0..4].try_into().unwrap());

            if incoming_site_id == site_id {
                debug!("Our packet, skipping!");
                continue;
            }

            let mut partials: SitePartials = map.remove(&incoming_site_id).unwrap_or_default();

            partials.from_buffer(amt, &buf);

            if partials.can_reconstruct() {
                //reconstruct
                debug!("We have enough to reconstruct");

                let total_buffer = partials.get_buffer();

                debug!("Total len:{}", total_buffer.len());

                if let Ok(recv_state) = serde_cbor::from_slice::<TextState>(&total_buffer) {

                    debug!("Received state:{:?}", recv_state);

                    let mut slock = site.write().unwrap();

                    let cur_value = slock.local_value();

                    slock.merge(recv_state).unwrap();

                    let new_value = slock.local_value();

                    if cur_value != new_value {
                        debug!("Updating value to:{}", new_value);
                        inner_sender.send(new_value).expect("Could not send update");
                    }
                }
            } else {
                map.insert(incoming_site_id, partials);
            }

        }
    });

    let mut seq: u32 = 1;

    thread::spawn(move || loop {
        let new_value = inner_receiver.recv().unwrap();

        let cur_value = send_site.read().unwrap().local_value();

        if new_value != cur_value {
            let changeset = Changeset::new(&cur_value, &new_value, "");
            debug!("New Value:`{}` \n,Change:{:?}", new_value, changeset.diffs);

            let mut slock = send_site.write().unwrap();

            let mut idx = 0;

            for diff in changeset.diffs {
                match diff {
                    Same(val) => {
                        idx += val.len();
                    }
                    Rem(val) => {
                        slock.replace(idx, val.len(), "");
                    }
                    Add(val) => {
                        slock.replace(idx, 0, &val);
                        idx += val.len();
                    }
                }
            }

            if let Err(err) = send_state(site_id, seq, &send_socket, &send_addr, &slock.state()) {
                error!("{}", err);
            }

            seq = seq.wrapping_add(1);
        }
    });

    Ok((sender, receiver))
}

const BUF_SIZE: usize = 1400;

fn send_state(site_id: u32, seq: u32, socket: &Socket, addr: &SockAddr, state: &TextState) -> Result<(), Error> {
    let mut val = serde_cbor::to_vec(&state)?;

    let mut to_send = val.len();

    debug!("Total len to send:{}", to_send);

    let num = (to_send / BUF_SIZE) as u32 + 1;

    debug!("Total number to send:{}", num);

    let mut idx: u32 = 0;

    while to_send > 0 {
        let end = cmp::min(BUF_SIZE, val.len());
        let new_val = val.split_off(end);

        let mut body = val;

        debug!("Body len:{}", body.len());

        let mut payload = Vec::new();
        debug!("Sending Site:{}, Seq:{}, Num:{} Idx:{}", site_id, seq, num, idx);

        payload.append(&mut site_id.to_be_bytes().to_vec());
        payload.append(&mut seq.to_be_bytes().to_vec());
        payload.append(&mut num.to_be_bytes().to_vec());
        payload.append(&mut idx.to_be_bytes().to_vec());
        payload.append(&mut body);

        val = new_val;

        debug!("Payload len:{}", payload.len());

        socket.send_to(&payload, &addr)?;

        to_send = to_send - end;

        idx += 1;
    }

    Ok(())
}


struct SitePartials {
    seq: u32,
    num: u32,
    partials: Vec<(u32, Vec<u8>)>
}

impl SitePartials {

    fn from_buffer(&mut self, amt: usize, buf: &[u8]) {

            let seq = u32::from_be_bytes(buf[4..8].try_into().unwrap());
            let num = u32::from_be_bytes(buf[8..12].try_into().unwrap());
            let idx = u32::from_be_bytes(buf[12..16].try_into().unwrap());

            debug!("Seq:{}, Num:{}, Idx:{}", seq, num, idx);

            if seq < self.seq {
                debug!("Discarding older partial");
                return;
            } else if seq > self.seq {
                debug!("Resetting partials for site");
                self.partials = vec!();
                self.seq = seq;
                self.num = num; 
            }


            let partial =  buf[16..amt].to_vec();

            debug!("Partial len:{}", partial.len());

            self.partials.push((idx, partial));
    }

    fn can_reconstruct(&self) -> bool {
        debug!("Current length:{}", self.partials.len());
        self.partials.len() == self.num as usize
    }

    fn get_buffer(&mut self) -> Vec<u8> {

        let mut partials = Vec::new();

        std::mem::swap(&mut partials, &mut self.partials);

        partials.sort_by(|(left, _), (right, _)| left.cmp(right));

        partials.into_iter().map(|(_idx, buf)| buf).flatten().collect()
    }

}


impl Default for SitePartials {
    fn default() -> Self {
        Self {
            seq: 0,
            num: 0,
            partials: vec!()
        }
    }
}