use std::net::SocketAddr;

use remote_steer_core::Result;
use tokio::net::UdpSocket;

use crate::{decode_message, encode_message, Channel, TransportMessage};

pub struct UdpPeer {
    socket: UdpSocket,
    token: String,
    session_id: u64,
    next_seq: u64,
    remote: Option<SocketAddr>,
}

impl UdpPeer {
    pub async fn bind(bind: SocketAddr, token: String, session_id: u64) -> Result<Self> {
        let socket = UdpSocket::bind(bind).await?;
        Ok(Self {
            socket,
            token,
            session_id,
            next_seq: 1,
            remote: None,
        })
    }

    pub async fn connect(
        bind: SocketAddr,
        remote: SocketAddr,
        token: String,
        session_id: u64,
    ) -> Result<Self> {
        let socket = UdpSocket::bind(bind).await?;
        socket.connect(remote).await?;
        Ok(Self {
            socket,
            token,
            session_id,
            next_seq: 1,
            remote: Some(remote),
        })
    }

    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.socket.local_addr()?)
    }

    pub async fn send_to_remote(
        &mut self,
        channel: Channel,
        message: &TransportMessage,
    ) -> Result<usize> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let bytes = encode_message(&self.token, self.session_id, seq, channel, message)?;
        if let Some(remote) = self.remote {
            Ok(self.socket.send_to(&bytes, remote).await?)
        } else {
            Ok(self.socket.send(&bytes).await?)
        }
    }

    pub async fn recv(
        &mut self,
        buf: &mut [u8],
    ) -> Result<(SocketAddr, Channel, TransportMessage)> {
        loop {
            let (len, addr) = match self.socket.recv_from(buf).await {
                Ok(packet) => packet,
                Err(err) if err.kind() == std::io::ErrorKind::ConnectionReset => continue,
                Err(err) => return Err(err.into()),
            };
            self.remote = Some(addr);
            let (packet, message) = decode_message(&self.token, &buf[..len])?;
            return Ok((addr, packet.channel, message));
        }
    }
}
